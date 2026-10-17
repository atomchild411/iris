//! The GL service on a thread of its own.
//!
//! A host call is answered on the thread that runs the emulated CPU, and the
//! machine stands still until it returns: no guest instruction runs and no
//! interrupt is taken. Replaying a big command buffer takes milliseconds, and
//! for that long IRIX misses its clock ticks -- its audio driver refills a
//! ring of a few milliseconds from a 1 kHz callback, so a busy GL program made
//! the sound drop out.
//!
//! So the service runs on its own thread, which owns every GL context, and
//! the CPU thread only hands it work:
//!
//! - An OP_BATCH_ASYNC buffer is read from the program's memory here, on the
//!   CPU thread (a page that is not there is still a need-page answer), queued,
//!   and answered at once. The GL thread runs it while the guest goes on.
//! - Every other call goes through the same queue, so it runs after the
//!   buffers sent before it, and the CPU thread waits for its answer. While
//!   it waits it lends the GL thread its view of the program's memory: the
//!   CPU thread is parked until the answer comes, so the two never use it at
//!   once, and the call costs two hand-offs however much memory it reads.
//! - Except that a call from a client with buffers still queued is not
//!   queued behind them: it is answered ENOTYET at once, and the library
//!   yields and makes it again. Waiting here would hold the CPU thread for
//!   the rest of those buffers -- the very stall this is for, moved to the
//!   frame's swap. Only a library that sends OP_BATCH_ASYNC ever has buffers
//!   queued, so only one that knows ENOTYET is ever told it.
//!
//! - OP_PRESENT_LATER is the same: its slots are read here and it is answered
//!   at once, and the GL thread shows it in its turn, or when FLUSH_GAP is up
//!   (service.rs) -- waking for that when nothing else comes.
//!
//! The queue is bounded: a program that sends faster than the host can run
//! waits for room, rather than piling up frames. Each side spins a few
//! microseconds before it sleeps: most answers come sooner than a thread
//! wakes, and in a virtual machine a wake-up is dear.
//!
//! `IRIS_HOSTGL_INLINE=1` keeps the service on the CPU thread, as it was.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iris_hostcall::{GuestMemory, Reply, Service, ENOTYET};

use crate::exec::read_guest;
use crate::service::{read_slots, GlService, EFAULT, EINVAL, MAX_BATCH, OP_BATCH_ASYNC, OP_PRESENT_LATER};

/// Buffers queued at most: 16 of the library's 128 KB ones.
const QUEUE: usize = 16;

/// How long a side spins for its next message before it sleeps.
const SPIN: Duration = Duration::from_micros(30);

/// The CPU thread's view of the program's memory, lent to the GL thread for
/// one call. Only dereferenced while that call runs, and the CPU thread does
/// nothing but wait for its answer meanwhile.
struct Lent(*mut (dyn GuestMemory + 'static));

// SAFETY: see `Lent` -- one thread at a time uses it, and the hand-off
// through the channels orders their accesses.
unsafe impl Send for Lent {}

enum Work {
    /// A call to run and answer, and the memory to run it with.
    Call([u64; 8], Lent),
    /// An OP_BATCH_ASYNC buffer, already answered.
    Detached { client: u32, bytes: Vec<u8>, left: Arc<AtomicUsize> },
    /// An OP_PRESENT_LATER, already answered.
    Present { client: u32, slots: [u64; 7] },
}

/// What `next_until` found.
enum Next<T> {
    Got(T),
    /// `until` came first.
    Due,
    Gone,
}

/// `rx`'s next message, or `Due` at `until`: `next` with a deadline.
fn next_until<T>(rx: &Receiver<T>, until: Instant) -> Next<T> {
    let spin = Instant::now() + SPIN;
    loop {
        match rx.try_recv() {
            Ok(m) => return Next::Got(m),
            Err(TryRecvError::Disconnected) => return Next::Gone,
            Err(TryRecvError::Empty) => {}
        }
        let now = Instant::now();
        if now >= until {
            return Next::Due;
        }
        if now < spin {
            std::hint::spin_loop();
            continue;
        }
        return match rx.recv_timeout(until - now) {
            Ok(m) => Next::Got(m),
            Err(RecvTimeoutError::Timeout) => Next::Due,
            Err(RecvTimeoutError::Disconnected) => Next::Gone,
        };
    }
}

/// `rx`'s next message: spin for `SPIN`, then sleep. `None` once the other
/// side has gone.
fn next<T>(rx: &Receiver<T>) -> Option<T> {
    let until = Instant::now() + SPIN;
    loop {
        match rx.try_recv() {
            Ok(m) => return Some(m),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) if Instant::now() < until => std::hint::spin_loop(),
            Err(TryRecvError::Empty) => return rx.recv().ok(),
        }
    }
}

pub struct Threaded {
    work: SyncSender<Work>,
    done: Receiver<Reply>,
    /// Each client's buffers queued and not yet run.
    queued: HashMap<u32, Arc<AtomicUsize>>,
}

impl Threaded {
    /// `svc`, moved to a new thread. Every context it makes lives there.
    pub fn spawn(mut svc: GlService) -> Threaded {
        let (work_tx, work_rx) = mpsc::sync_channel::<Work>(QUEUE);
        let (done_tx, done_rx) = mpsc::channel::<Reply>();
        std::thread::Builder::new()
            .name("hostgl".into())
            .spawn(move || {
                crate::qos::latency_critical("host GL");
                // Ends when the proxy goes (the service is registered again),
                // dropping the service -- and its contexts -- on this thread.
                loop {
                    let w = match svc.next_due() {
                        None => match next(&work_rx) {
                            Some(w) => w,
                            None => break,
                        },
                        Some(due) => match next_until(&work_rx, due) {
                            Next::Got(w) => w,
                            Next::Due => {
                                svc.show_due();
                                continue;
                            }
                            Next::Gone => break,
                        },
                    };
                    match w {
                        Work::Call(args, Lent(mem)) => {
                            // SAFETY: the CPU thread lent this for the call and
                            // waits for the answer before using it again.
                            let reply = svc.call(unsafe { &mut *mem }, &args);
                            if done_tx.send(reply).is_err() {
                                break;
                            }
                        }
                        Work::Detached { client, bytes, left } => {
                            svc.run_detached(client, bytes);
                            left.fetch_sub(1, Ordering::Release);
                        }
                        Work::Present { client, slots } => svc.present_later(client, slots),
                    }
                    // A program that never stops sending still gets its due ones.
                    svc.show_due();
                }
            })
            .expect("host GL: no thread for the GL service");
        Threaded { work: work_tx, done: done_rx, queued: HashMap::new() }
    }

    /// OP_BATCH_ASYNC: read the buffer now, run it later.
    fn queue_batch(&mut self, mem: &mut dyn GuestMemory, args: &[u64; 8]) -> Reply {
        let [buf, len] = match read_slots::<2>(mem, args[1]) {
            Ok(s) => s,
            Err(f) => return Reply::NeedPage(f),
        };
        if len == 0 {
            return Reply::Ok(0, 0);
        }
        if len > MAX_BATCH as u64 || len % 8 != 0 {
            return Reply::Err(EFAULT);
        }
        let bytes = match read_guest(mem, buf, len as usize) {
            Ok(b) => b,
            Err(f) => return Reply::NeedPage(f),
        };
        let client = args[2] as u32;
        let left = self.queued.entry(client).or_default().clone();
        left.fetch_add(1, Ordering::Relaxed);
        match self.work.send(Work::Detached { client, bytes, left: left.clone() }) {
            Ok(()) => Reply::Ok(0, 0),
            Err(_) => {
                left.fetch_sub(1, Ordering::Relaxed);
                Reply::Err(EINVAL)
            }
        }
    }
}

impl Service for Threaded {
    fn call(&mut self, mem: &mut dyn GuestMemory, args: &[u64; 8]) -> Reply {
        if args[0] == OP_BATCH_ASYNC {
            return self.queue_batch(mem, args);
        }
        if args[0] == OP_PRESENT_LATER {
            // Not held back by the client's buffers: it is queued behind them.
            let slots = match read_slots::<7>(mem, args[1]) {
                Ok(s) => s,
                Err(f) => return Reply::NeedPage(f),
            };
            return match self.work.send(Work::Present { client: args[2] as u32, slots }) {
                Ok(()) => Reply::Ok(1, 0),
                Err(_) => Reply::Err(EINVAL),
            };
        }
        if self.queued.get(&(args[2] as u32)).is_some_and(|n| n.load(Ordering::Acquire) > 0) {
            return Reply::Err(ENOTYET);
        }
        let mem: *mut (dyn GuestMemory + '_) = mem;
        // SAFETY: only the lifetime is changed, and the GL thread is done with
        // the pointer before this call returns (it answers, or it has gone).
        let lent = Lent(unsafe { std::mem::transmute::<*mut (dyn GuestMemory + '_), *mut (dyn GuestMemory + 'static)>(mem) });
        if self.work.send(Work::Call(*args, lent)).is_err() {
            return Reply::Err(EINVAL);
        }
        next(&self.done).unwrap_or(Reply::Err(EINVAL))
    }
}
