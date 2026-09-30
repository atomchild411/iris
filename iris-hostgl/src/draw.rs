//! Drawables: where a context's pixels live, and how a finished frame leaves.
//!
//! Every drawable -- an X window, a GLX pixmap, a pbuffer -- is a framebuffer
//! object of its own, so a program that draws into a window, then a pbuffer,
//! then the window again finds each as it left it. Framebuffer objects belong
//! to a share group, so drawables are kept per group (see service.rs) and
//! built with one of the group's contexts current.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::ffi::c_void;

use crate::backend::{Backend, Surface};
use crate::gl::*;

pub struct Draw {
    pub fbo: u32,
    color: u32,
    /// Depth and stencil, packed; or depth alone if the host would not make
    /// the packed format complete.
    depth: u32,
    /// The frame turned the right way up for X, read back from here. Several
    /// when they are surfaces, so that a frame can be read on the presenting
    /// thread while the next one is drawn (see `swap`).
    flips: Vec<Flip>,
    /// The flip target the next swap tries first.
    next_flip: AtomicUsize,
    /// A multisampled drawable cannot be read: its samples are resolved into
    /// this one first (GLX_SGIS_multisample).
    resolve_fbo: u32,
    resolve_color: u32,
    pub samples: i32,
    pub w: i32,
    pub h: i32,
}

/// A flip target: a framebuffer object whose colour is a CPU-addressable
/// surface where the platform gives one, a renderbuffer where it does not.
struct Flip {
    fbo: u32,
    surface: Option<Arc<dyn Surface>>,
    color: u32,
    /// Queued for or being read on the presenting thread: not to be drawn
    /// into until that is done.
    busy: Arc<AtomicBool>,
}

/// Flip targets a drawable with surfaces gets: one being drawn, one queued,
/// one being presented.
const FLIPS: usize = 3;

impl Flip {
    /// A flip target `w` x `h`; a context of the group must be current.
    /// Leaves its framebuffer bound.
    unsafe fn new(backend: &mut dyn Backend, w: i32, h: i32) -> Flip {
        let mut f = Flip { fbo: 0, surface: None, color: 0, busy: Arc::new(AtomicBool::new(false)) };
        glGenFramebuffersEXT(1, &mut f.fbo);
        glBindFramebufferEXT(GL_FRAMEBUFFER, f.fbo);
        f.surface = backend.new_surface(w, h).map(Arc::from);
        if let Some(sf) = &f.surface {
            let (target, tex) = sf.texture();
            glFramebufferTexture2DEXT(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, target, tex, 0);
        }
        if f.surface.is_none() || glCheckFramebufferStatusEXT(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
            // No surface, or one the driver will not render into.
            f.drop_surface();
            glGenRenderbuffersEXT(1, &mut f.color);
            glBindRenderbufferEXT(GL_RENDERBUFFER, f.color);
            glRenderbufferStorageEXT(GL_RENDERBUFFER, GL_RGBA8, w, h);
            glFramebufferRenderbufferEXT(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, f.color);
        }
        f
    }

    fn drop_surface(&mut self) {
        if let Some(mut sf) = self.surface.take() {
            // Only this drawable holds it once the presenter has drained.
            if let Some(s) = Arc::get_mut(&mut sf) {
                s.delete_texture();
            }
        }
    }

    /// Delete its objects; the presenter must have drained.
    unsafe fn delete(mut self) {
        self.drop_surface();
        if self.color != 0 {
            glDeleteRenderbuffersEXT(1, &self.color);
        }
        if self.fbo != 0 {
            glDeleteFramebuffersEXT(1, &self.fbo);
        }
    }
}

/// Where a drawable's pixels are read from: itself, or its resolve buffer.
pub fn read_source(d: &Draw) -> u32 {
    if d.samples > 1 {
        d.resolve_fbo
    } else {
        d.fbo
    }
}

/// Resolve a multisampled drawable into the buffer reads come from. GL will
/// not read a multisampled framebuffer, and a resolve cannot flip, so this is
/// a plain same-rectangle blit. Leaves the resolve buffer bound for reading
/// and drawing bound where it was -- which is another drawable when the
/// program reads from one and draws into another (make_current_read).
pub fn resolve(d: &Draw) {
    if d.samples <= 1 {
        return;
    }
    // SAFETY: framebuffer objects of the current share group.
    unsafe {
        let mut drawing = 0i32;
        glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &mut drawing);
        glBindFramebufferEXT(GL_READ_FRAMEBUFFER, d.fbo);
        glBindFramebufferEXT(GL_DRAW_FRAMEBUFFER, d.resolve_fbo);
        glBlitFramebufferEXT(0, 0, d.w, d.h, 0, 0, d.w, d.h, GL_COLOR_BUFFER_BIT, GL_NEAREST);
        glBindFramebufferEXT(GL_DRAW_FRAMEBUFFER, drawing as u32);
        glBindFramebufferEXT(GL_READ_FRAMEBUFFER, d.resolve_fbo);
    }
}

/// Bind `draw` for drawing and `read` for reading, when both exist.
pub fn bind(draws: &HashMap<u32, Draw>, draw: u32, read: u32) {
    let (Some(d), Some(r)) = (draws.get(&draw), draws.get(&read)) else { return };
    // SAFETY: framebuffer objects of the current share group.
    unsafe {
        glBindFramebufferEXT(GL_DRAW_FRAMEBUFFER, d.fbo);
        glBindFramebufferEXT(GL_READ_FRAMEBUFFER, read_source(r));
    }
}

/// Delete a drawable's objects. A context of its share group must be current.
pub fn delete(mut d: Draw) {
    // The presenting thread may still be reading one of its surfaces.
    crate::present::drain();
    // SAFETY: names of the current share group; zero names are ignored by GL.
    unsafe {
        for f in std::mem::take(&mut d.flips) {
            f.delete();
        }
        for rb in [d.color, d.depth, d.resolve_color] {
            if rb != 0 {
                glDeleteRenderbuffersEXT(1, &rb);
            }
        }
        for fb in [d.fbo, d.resolve_fbo] {
            if fb != 0 {
                glDeleteFramebuffersEXT(1, &fb);
            }
        }
    }
}

/// A drawable's framebuffer object, made or resized to `w` x `h` with
/// `samples` samples a pixel. A context of the group must be current; this
/// leaves the drawable bound, so the caller puts the current bindings back.
pub fn ensure(draws: &mut HashMap<u32, Draw>, backend: &mut dyn Backend, id: u32, w: i32, h: i32, samples: i32) {
    let d = draws.entry(id).or_insert_with(|| Draw {
        fbo: 0,
        color: 0,
        depth: 0,
        flips: Vec::new(),
        next_flip: AtomicUsize::new(0),
        resolve_fbo: 0,
        resolve_color: 0,
        samples: 0,
        w: 0,
        h: 0,
    });
    if d.fbo != 0 && d.w == w && d.h == h && d.samples == samples {
        return;
    }
    log::debug!("host GL: drawable {id:#x} {} at {w}x{h}, {samples} samples", if d.fbo == 0 { "made" } else { "remade" });
    // SAFETY: GL object management in the current context.
    unsafe {
        if d.fbo == 0 {
            glGenFramebuffersEXT(1, &mut d.fbo);
        }
        if !d.flips.is_empty() {
            // The presenting thread may still be reading the old ones.
            crate::present::drain();
            for f in std::mem::take(&mut d.flips) {
                f.delete();
            }
        }
        for rb in [&mut d.color, &mut d.depth, &mut d.resolve_color] {
            if *rb != 0 {
                glDeleteRenderbuffersEXT(1, rb);
                *rb = 0;
            }
        }

        // The flip targets are what frames are read out of, so they are the
        // ones that want to be addressable: surfaces if the platform will
        // give them (several, see FLIPS), one renderbuffer if not.
        let first = Flip::new(backend, w, h);
        let rotate = first.surface.is_some();
        d.flips.push(first);
        if rotate {
            for _ in 1..FLIPS {
                let f = Flip::new(backend, w, h);
                if f.surface.is_none() {
                    f.delete();
                    break;
                }
                d.flips.push(f);
            }
        }
        d.next_flip.store(0, Ordering::Relaxed);

        let storage = |fmt: u32| {
            if samples > 1 {
                glRenderbufferStorageMultisampleEXT(GL_RENDERBUFFER, samples, fmt, w, h);
            } else {
                glRenderbufferStorageEXT(GL_RENDERBUFFER, fmt, w, h);
            }
        };
        glBindFramebufferEXT(GL_FRAMEBUFFER, d.fbo);
        glGenRenderbuffersEXT(1, &mut d.color);
        glBindRenderbufferEXT(GL_RENDERBUFFER, d.color);
        storage(GL_RGBA8);
        glFramebufferRenderbufferEXT(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, d.color);
        // GLX says every visual has an 8-bit stencil buffer (glXGetConfig in
        // the shim), so there is one: packed with the depth buffer.
        glGenRenderbuffersEXT(1, &mut d.depth);
        glBindRenderbufferEXT(GL_RENDERBUFFER, d.depth);
        storage(GL_DEPTH24_STENCIL8);
        glFramebufferRenderbufferEXT(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_RENDERBUFFER, d.depth);
        glFramebufferRenderbufferEXT(GL_FRAMEBUFFER, GL_STENCIL_ATTACHMENT, GL_RENDERBUFFER, d.depth);
        if glCheckFramebufferStatusEXT(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
            const GL_DEPTH_COMPONENT24: u32 = 0x81A6;
            glFramebufferRenderbufferEXT(GL_FRAMEBUFFER, GL_STENCIL_ATTACHMENT, GL_RENDERBUFFER, 0);
            glBindRenderbufferEXT(GL_RENDERBUFFER, d.depth);
            storage(GL_DEPTH_COMPONENT24);
            glFramebufferRenderbufferEXT(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_RENDERBUFFER, d.depth);
        }
        if samples > 1 {
            // What reads of this drawable come from.
            if d.resolve_fbo == 0 {
                glGenFramebuffersEXT(1, &mut d.resolve_fbo);
            }
            glBindFramebufferEXT(GL_FRAMEBUFFER, d.resolve_fbo);
            glGenRenderbuffersEXT(1, &mut d.resolve_color);
            glBindRenderbufferEXT(GL_RENDERBUFFER, d.resolve_color);
            glRenderbufferStorageEXT(GL_RENDERBUFFER, GL_RGBA8, w, h);
            glFramebufferRenderbufferEXT(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, d.resolve_color);
            glBindFramebufferEXT(GL_FRAMEBUFFER, d.fbo);
        }
        glBindRenderbufferEXT(GL_RENDERBUFFER, 0);
        d.w = w;
        d.h = h;
        d.samples = samples;
        // A framebuffer that is not complete draws nothing, and the error it
        // leaves is read by whatever asks next -- so it is taken here.
        let status = glCheckFramebufferStatusEXT(GL_FRAMEBUFFER);
        let err = glGetError();
        if status != GL_FRAMEBUFFER_COMPLETE || err != 0 {
            log::warn!(
                "host GL: drawable {id:#x} at {w}x{h} with {samples} samples is not usable (status {status:#x}, error {err:#x})"
            );
        }
    }
}

/// The current frame of drawable `d` (X window `window`), `w` x `h` as the
/// caller's window is now.
///
/// Presented straight into the window when a display is registered and knows
/// it -- `None` then. Otherwise the pixels for the program to put up itself:
/// `w` x `h` rows top first, each pixel the bytes A R G B, which is X's
/// big-endian 0x00RRGGBB with the pad byte a depth 24 image ignores -- or,
/// with `abgr`, A B G R (0x00BBGGRR), for a visual whose red is the low byte
/// (IMPACT's 24-bit visuals are all that way).
///
/// Only the part both the drawable and the window have is read: the drawable
/// may still be the size it was before the window was resized, but the rows
/// are packed at the caller's width whatever happens (packing at the
/// drawable's width shears every row by the difference).
/// Where a finished frame goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sink {
    /// Presented before the swap returns, as it always was. The answer is
    /// recorded, so the drawable's later swaps know whether they may go the
    /// other way.
    Now,
    /// Handed to the presenting thread, and the swap returns (see
    /// `present.rs`). Refused -- and the frame then handed back to the program
    /// like any unpresentable one -- for a drawable not yet known to present.
    Queued,
}

/// Hand a frame to `d`: at a screen position when the program gave one.
pub(crate) fn present_to(
    d: &dyn iris_hostcall::Display,
    window: u32,
    at: Option<(i32, i32)>,
    bgra: &[u8],
    stride: usize,
    w: usize,
    h: usize,
) -> bool {
    match at {
        Some((x, y)) => d.present_at(window, x, y, bgra, stride, w, h),
        None => d.present(window, bgra, stride, w, h),
    }
}

impl Sink {
    /// True if the frame has been dealt with and the program needs no pixels.
    fn deliver(self, window: u32, at: Option<(i32, i32)>, bgra: &[u8], stride: usize, w: usize, h: usize) -> bool {
        match self {
            Sink::Now => {
                let ok = match iris_hostcall::display() {
                    Some(d) => present_to(d.as_ref(), window, at, bgra, stride, w, h),
                    None => false,
                };
                crate::present::note(window, ok);
                ok
            }
            Sink::Queued if crate::present::presents(window) => {
                crate::present::queue(window, at, bgra, stride, w, h);
                true
            }
            // Not a drawable we have seen present. Whether presenting works is
            // only known afterwards, and an asynchronous swap has returned by
            // then -- so present this one now, which answers the question, and
            // let the next swap of this drawable be the fast one. A drawable
            // that cannot be presented into stays here for ever and behaves
            // exactly as it did before any of this.
            Sink::Queued => Sink::Now.deliver(window, at, bgra, stride, w, h),
        }
    }
}

pub fn swap(d: &Draw, window: u32, at: Option<(i32, i32)>, w: i32, h: i32, sink: Sink, abgr: bool) -> Option<Vec<u8>> {
    let (w, h) = (w.max(0), h.max(0));
    let (rw, rh) = (w.min(d.w), h.min(d.h));
    let mut frame = vec![0u8; w as usize * h as usize * 4];
    if rw <= 0 || rh <= 0 {
        return Some(frame);
    }
    let (rw, rh, wu) = (rw as usize, rh as usize, w as usize);
    let mut presented = false;
    // SAFETY: GL in the current context, which owns `d`; buffers sized for
    // what is read into them.
    unsafe {
        // The program's state that a blit and a readback would honour.
        let scissor = glIsEnabled(GL_SCISSOR_TEST) != 0;
        let names = [GL_PACK_ALIGNMENT, GL_PACK_ROW_LENGTH, GL_PACK_SKIP_ROWS, GL_PACK_SKIP_PIXELS, GL_PACK_SWAP_BYTES, GL_PACK_LSB_FIRST];
        let mut pack = [0i32; 6];
        for (v, &n) in pack.iter_mut().zip(names.iter()) {
            glGetIntegerv(n, v);
        }
        if scissor {
            glDisable(GL_SCISSOR_TEST);
        }
        for (&n, &v) in names.iter().zip([1, 0, 0, 0, 0, 0].iter()) {
            glPixelStorei(n, v);
        }

        // A multisampled drawable is resolved before the flip: a resolve
        // cannot flip, and a flip cannot resolve.
        resolve(d);
        // A flip target the presenting thread is not reading; with every one
        // in use, wait for it (the guest is that far ahead of the display).
        let n = d.flips.len().max(1);
        let start = d.next_flip.load(Ordering::Relaxed);
        let pick = (0..n).map(|i| (start + i) % n).find(|&i| !d.flips[i].busy.load(Ordering::Acquire));
        let idx = match pick {
            Some(i) => i,
            None => {
                crate::present::drain();
                start % n
            }
        };
        let flip = &d.flips[idx];
        d.next_flip.store((idx + 1) % n, Ordering::Relaxed);
        glBindFramebufferEXT(GL_READ_FRAMEBUFFER, read_source(d));
        glBindFramebufferEXT(GL_DRAW_FRAMEBUFFER, flip.fbo);
        glBlitFramebufferEXT(0, 0, rw as i32, rh as i32, 0, rh as i32, rw as i32, 0, GL_COLOR_BUFFER_BIT, GL_NEAREST);

        // Queued: the presenting thread maps the surface -- which waits for
        // the GPU to finish the frame -- and presents it, and the swap
        // returns now. Mapping it here stopped the whole emulated machine for
        // that wait on every frame (powerflip: over half the CPU thread's
        // time in IOSurfaceLock).
        if let (Some(sf), Sink::Queued) = (&flip.surface, sink) {
            if crate::present::presents(window) {
                glFlush();
                flip.busy.store(true, Ordering::Release);
                crate::present::queue_surface(window, at, sf.clone(), flip.busy.clone(), rw, rh);
                for (&n, &v) in names.iter().zip(pack.iter()) {
                    glPixelStorei(n, v);
                }
                if scissor {
                    glEnable(GL_SCISSOR_TEST);
                }
                return None;
            }
        }

        // Copy BGRA rows (top first, `stride` bytes each) into the frame the
        // program gets back, turning each pixel's bytes round.
        let mut hand_back = |bgra: &[u8], stride: usize| {
            for y in 0..rh {
                let (src, dst) = (y * stride, y * wu * 4);
                for x in 0..rw {
                    let (i, o) = (src + x * 4, dst + x * 4);
                    frame[o] = bgra[i + 3];
                    if abgr {
                        frame[o + 1] = bgra[i];
                        frame[o + 2] = bgra[i + 1];
                        frame[o + 3] = bgra[i + 2];
                    } else {
                        frame[o + 1] = bgra[i + 2];
                        frame[o + 2] = bgra[i + 1];
                        frame[o + 3] = bgra[i];
                    }
                }
            }
        };
        match &flip.surface {
            // The blit has already put the frame where the CPU can see it.
            // Submitted, not finished: locking the surface waits for the GPU
            // work outstanding on it, so a flush is enough.
            Some(sf) => {
                glFlush();
                let mut done = false;
                let mapped = sf.with_pixels(&mut |px, stride| {
                    presented = sink.deliver(window, at, px, stride, rw, rh);
                    if !presented && px.len() >= stride * rh && stride >= rw * 4 {
                        hand_back(px, stride);
                    }
                    done = true;
                });
                if !mapped || !done {
                    log::warn!("host GL: a frame's surface could not be mapped");
                }
            }
            None => {
                glBindFramebufferEXT(GL_READ_FRAMEBUFFER, flip.fbo);
                glReadBuffer(GL_COLOR_ATTACHMENT0);
                let mut bgra = vec![0u8; rw * rh * 4];
                glReadPixels(0, 0, rw as i32, rh as i32, GL_BGRA, GL_UNSIGNED_BYTE, bgra.as_mut_ptr() as *mut c_void);
                presented = sink.deliver(window, at, &bgra, rw * 4, rw, rh);
                if !presented {
                    hand_back(&bgra, rw * 4);
                }
            }
        }

        for (&n, &v) in names.iter().zip(pack.iter()) {
            glPixelStorei(n, v);
        }
        if scissor {
            glEnable(GL_SCISSOR_TEST);
        }
    }
    (!presented).then_some(frame)
}
