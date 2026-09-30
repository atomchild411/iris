//! Presenting a finished frame without the program waiting for it.
//!
//! A swap normally costs the program everything the host does with the frame:
//! the blit, the readback, and then `Display::present`, which walks the pixels
//! one at a time into the X screen's buffer. That last part is per pixel and
//! by far the largest of the three at a real window size -- and the program is
//! blocked in a system call (or a channel round trip) the whole time.
//!
//! None of it has to be. Once the pixels are out of the GL they are just bytes,
//! and nothing the program does next depends on them. So with
//! `glXSwapIntervalSGI(0)` -- "do not wait for the retrace", the standard way
//! for a program to say it does not want to be paced by the display -- the
//! frame is handed to this thread and the swap returns.
//!
//! # What is bounded, and how
//!
//! At most two frames are in flight: one this thread is presenting and one
//! waiting behind it.
//!
//! - A frame arriving while one is *queued but not started* replaces it. The
//!   queued frame is stale by definition -- the program has drawn a newer one
//!   -- and presenting it first would only add latency and show the viewer an
//!   older picture. The replaced frame's buffer is kept and reused.
//! - A frame arriving while one is queued *and* one is being presented waits
//!   for the presenting one to finish. That is the throttle: a program using
//!   the swap as its clock still gets paced, just two frames later than
//!   before, and a guest that renders faster than the host can present cannot
//!   run away.
//!
//! The emulator's own window presents with swap interval 1, so the display's
//! refresh is the real ceiling. Frames beyond it are dropped here by the
//! replacement rule, which is the point: the guest stops paying for frames
//! nobody will see, rather than queueing them.
//!
//! # What still waits
//!
//! [`drain`] waits for the queue to empty and the thread to be idle. `glFinish`
//! and `glXWaitGL` call it, so they keep meaning what they say: when they
//! return, the frames the program asked for really are on the screen. A
//! readback needs nothing: it reads the drawable's framebuffer object, which
//! this thread never touches -- it has its own copy of the pixels.
//!
//! # Windows that cannot be presented into
//!
//! `Display::present` returns false for a window it does not know (a program
//! drawing into a GLX pixmap, or one whose window has gone), and the frame is
//! then handed back for the program to put up itself. That answer is only
//! known after the fact, which an asynchronous swap cannot use -- so the first
//! swap of each drawable presents inline whatever the interval, and [`note`]
//! records what happened. Only a drawable known to present goes on to the
//! asynchronous path; one that does not keeps presenting inline and handing
//! the frame back, exactly as it did before any of this.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use crate::backend::Surface;

/// Frames in flight at once: one being presented, one waiting.
const MAX_IN_FLIGHT: usize = 2;
/// Spare pixel buffers kept for reuse rather than freed.
const SPARE_BUFFERS: usize = 2;

/*
 * What the replacement rule actually did.
 *
 * The rule was documented from the start and counted nothing, so the drop rate
 * -- the one number that says whether asynchronous swap is throwing away work
 * or absorbing it -- could not be quoted by anyone. These are global rather
 * than per client because the presenting thread is: one queue serves every
 * channel.
 *
 * `QUEUED` is every frame handed over, `PRESENTED` every frame that reached
 * the screen, `REPLACED` every frame dropped because a newer one arrived
 * before the thread got to it, and `WAITED` the times a swap had to wait for
 * the queue because the guest was two frames ahead. QUEUED = PRESENTED +
 * REPLACED + whatever is in flight at the moment of reading.
 */
static QUEUED: AtomicU64 = AtomicU64::new(0);
static PRESENTED: AtomicU64 = AtomicU64::new(0);
static REPLACED: AtomicU64 = AtomicU64::new(0);
static WAITED: AtomicU64 = AtomicU64::new(0);

/// Frames queued, presented, replaced, and swaps that had to wait.
pub fn counters() -> (u64, u64, u64, u64) {
    (
        QUEUED.load(Ordering::Relaxed),
        PRESENTED.load(Ordering::Relaxed),
        REPLACED.load(Ordering::Relaxed),
        WAITED.load(Ordering::Relaxed),
    )
}

struct Frame {
    window: u32,
    at: Option<(i32, i32)>,
    src: Src,
    w: usize,
    h: usize,
}

/// A frame's pixels: a copy, or the surface the GPU is rendering them into,
/// mapped on this thread (which is where the wait for the GPU then falls).
enum Src {
    Bytes { bgra: Vec<u8>, stride: usize },
    Surface { sf: Arc<dyn Surface>, busy: Arc<AtomicBool> },
}

impl Src {
    /// Done with: a surface goes back to its drawable, a buffer is returned
    /// for reuse.
    fn release(self) -> Option<Vec<u8>> {
        match self {
            Src::Bytes { bgra, .. } => Some(bgra),
            Src::Surface { busy, .. } => {
                busy.store(false, Ordering::Release);
                None
            }
        }
    }
}

#[derive(Default)]
struct State {
    queued: Option<Frame>,
    /// A frame is being presented right now.
    busy: bool,
    /// Buffers of frames already presented or replaced.
    spare: Vec<Vec<u8>>,
    /// What `Display::present` last said about a window.
    presents: HashMap<u32, bool>,
    started: bool,
}

struct Presenter {
    st: Mutex<State>,
    /// The thread waits on this one; queuers and [`drain`] on `idle`.
    work: Condvar,
    idle: Condvar,
}

fn presenter() -> &'static Presenter {
    static P: OnceLock<Presenter> = OnceLock::new();
    P.get_or_init(|| Presenter { st: Mutex::new(State::default()), work: Condvar::new(), idle: Condvar::new() })
}

/// The state, poisoning ignored: a panic while presenting one frame says
/// nothing about the next one, and frames are not worth failing the process
/// for.
fn lock(p: &Presenter) -> MutexGuard<'_, State> {
    p.st.lock().unwrap_or_else(|e| e.into_inner())
}

fn wait<'a>(cv: &Condvar, st: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
    cv.wait(st).unwrap_or_else(|e| e.into_inner())
}

/// Record what a synchronous present answered for `window`, so that later
/// swaps of the same drawable know whether the asynchronous path can work.
pub fn note(window: u32, ok: bool) {
    lock(presenter()).presents.insert(window, ok);
}

/// Whether frames for `window` are known to present. False for a window never
/// presented synchronously yet, which is what makes the first swap of each
/// drawable take the synchronous path.
pub fn presents(window: u32) -> bool {
    lock(presenter()).presents.get(&window).copied().unwrap_or(false)
}

/// Forget a drawable: its next swap learns again. Called when a drawable goes,
/// so that a window id reused for another drawable is not taken on trust.
pub fn forget(window: u32) {
    lock(presenter()).presents.remove(&window);
}

/// Take a copy of a finished frame and present it on this thread's own time.
/// Blocks only while [`MAX_IN_FLIGHT`] frames are already in flight.
///
/// `bgra` is `h` rows of `stride` bytes, `w` pixels each, as
/// `Display::present` wants them.
pub fn queue(window: u32, at: Option<(i32, i32)>, bgra: &[u8], stride: usize, w: usize, h: usize) {
    let mut st = admit();
    let mut buf = st.spare.pop().unwrap_or_default();
    buf.clear();
    buf.extend_from_slice(bgra);
    put(st, Frame { window, at, src: Src::Bytes { bgra: buf, stride }, w, h });
}

/// Queue a frame still being rendered into `sf`: this thread maps it (waiting
/// for the GPU there, not in the swap) and presents `w` x `h` of it, then
/// clears `busy` so its drawable may draw into it again.
pub fn queue_surface(window: u32, at: Option<(i32, i32)>, sf: Arc<dyn Surface>, busy: Arc<AtomicBool>, w: usize, h: usize) {
    let st = admit();
    put(st, Frame { window, at, src: Src::Surface { sf, busy }, w, h });
}

/// The state, once there is room for another frame: starts the thread, and
/// waits while [`MAX_IN_FLIGHT`] frames are already in flight.
fn admit() -> MutexGuard<'static, State> {
    let p = presenter();
    let mut st = lock(p);
    if !st.started {
        st.started = true;
        // Detached on purpose: it lives as long as the process, and the
        // frames it holds are its own copies.
        let _ = std::thread::Builder::new().name("hostgl-present".into()).spawn(run);
    }
    // One being presented and one already waiting: wait for the first to
    // finish rather than let the guest run further ahead.
    let mut waited = false;
    while st.queued.is_some() as usize + st.busy as usize >= MAX_IN_FLIGHT {
        waited = true;
        st = wait(&p.idle, st);
    }
    if waited {
        WAITED.fetch_add(1, Ordering::Relaxed);
    }
    st
}

/// Put `f` in the queue, replacing whatever is waiting there: it is older.
fn put(mut st: MutexGuard<'static, State>, f: Frame) {
    if let Some(old) = st.queued.take() {
        REPLACED.fetch_add(1, Ordering::Relaxed);
        if let Some(buf) = old.src.release() {
            if st.spare.len() < SPARE_BUFFERS {
                st.spare.push(buf);
            }
        }
    }
    QUEUED.fetch_add(1, Ordering::Relaxed);
    st.queued = Some(f);
    drop(st);
    presenter().work.notify_one();
}

/// Wait until nothing is queued and nothing is being presented.
pub fn drain() {
    let p = presenter();
    let mut st = lock(p);
    while st.queued.is_some() || st.busy {
        st = wait(&p.idle, st);
    }
}

fn run() {
    let p = presenter();
    crate::qos::latency_critical("presenter");
    loop {
        let frame = {
            let mut st = lock(p);
            loop {
                match st.queued.take() {
                    Some(f) => {
                        st.busy = true;
                        break f;
                    }
                    None => st = wait(&p.work, st),
                }
            }
        };
        // Outside the lock: presenting takes the display's own lock, and a
        // queuer must not be held up by it.
        let ok = match iris_hostcall::display() {
            Some(d) => match &frame.src {
                Src::Bytes { bgra, stride } => crate::draw::present_to(d.as_ref(), frame.window, frame.at, bgra, *stride, frame.w, frame.h),
                Src::Surface { sf, .. } => {
                    let mut ok = false;
                    let mapped = sf.with_pixels(&mut |px, stride| {
                        if px.len() >= stride * frame.h && stride >= frame.w * 4 {
                            ok = crate::draw::present_to(d.as_ref(), frame.window, frame.at, px, stride, frame.w, frame.h);
                        }
                    });
                    mapped && ok
                }
            },
            None => false,
        };
        if ok {
            PRESENTED.fetch_add(1, Ordering::Relaxed);
        }
        let mut st = lock(p);
        st.busy = false;
        if !ok {
            // The window has gone, or was never one we can draw into. This
            // frame is lost; the drawable's next swap takes the synchronous
            // path and hands the pixels back to the program instead.
            log::debug!("host GL: a frame for window {:#x} could not be presented; swaps of it go back to waiting", frame.window);
            st.presents.insert(frame.window, false);
        }
        if let Some(buf) = frame.src.release() {
            if st.spare.len() < SPARE_BUFFERS {
                st.spare.push(buf);
            }
        }
        drop(st);
        p.idle.notify_all();
    }
}
