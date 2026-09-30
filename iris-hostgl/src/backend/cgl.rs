//! Apple's OpenGL.framework: CGL contexts in the legacy profile, and IOSurface
//! render targets.
//!
//! IOSurface is the one kind of GL-renderable surface on the Mac whose pixels
//! the CPU can also address. A renderbuffer's layout belongs to the driver --
//! tiled, maybe compressed -- so `glReadPixels` has to untile a whole frame to
//! hand one over; an IOSurface is linear and shared by construction. Render
//! into it through a rectangle texture and its base address is those same
//! pixels, so presenting a frame costs a memcpy rather than a readback.
//! `IRIS_HOSTGL_NOSURFACE=1` turns it off, which is how the two paths are
//! compared.

use std::ffi::c_void;
use std::sync::OnceLock;

use super::{Backend, ContextHandle, Surface};

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    static kIOSurfaceWidth: *const c_void;
    static kIOSurfaceHeight: *const c_void;
    static kIOSurfaceBytesPerElement: *const c_void;
    static kIOSurfacePixelFormat: *const c_void;
    fn IOSurfaceCreate(props: *const c_void) -> *mut c_void;
    fn IOSurfaceGetBaseAddress(s: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(s: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(s: *mut c_void) -> usize;
    fn IOSurfaceLock(s: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(s: *mut c_void, options: u32, seed: *mut u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        n: isize,
        kcb: *const c_void,
        vcb: *const c_void,
    ) -> *const c_void;
    fn CFNumberCreate(alloc: *const c_void, ty: i32, value: *const c_void) -> *const c_void;
    fn CFRelease(v: *const c_void);
}

#[link(name = "OpenGL", kind = "framework")]
extern "C" {
    fn CGLGetCurrentContext() -> *mut c_void;
    fn CGLSetCurrentContext(ctx: *mut c_void) -> i32;
    fn CGLChoosePixelFormat(attribs: *const i32, pix: *mut *mut c_void, npix: *mut i32) -> i32;
    fn CGLCreateContext(pix: *mut c_void, share: *mut c_void, ctx: *mut *mut c_void) -> i32;
    fn CGLDestroyContext(ctx: *mut c_void) -> i32;
    fn CGLDestroyPixelFormat(pix: *mut c_void) -> i32;
    fn CGLTexImageIOSurface2D(
        ctx: *mut c_void,
        target: u32,
        internal_format: u32,
        width: u32,
        height: u32,
        format: u32,
        ty: u32,
        surface: *mut c_void,
        plane: u32,
    ) -> i32;
    fn glBindTexture(target: u32, texture: u32);
    fn glDeleteTextures(n: i32, ids: *const u32);
}

const GL_TEXTURE_RECTANGLE: u32 = 0x84F5;
const GL_RGBA8: u32 = 0x8058;
const GL_BGRA: u32 = 0x80E1;
const GL_UNSIGNED_INT_8_8_8_8_REV: u32 = 0x8367;
const CF_NUMBER_INT: i32 = 9; // kCFNumberIntType
/// 'BGRA', which is what the X side wants a frame in anyway.
const PIXEL_FORMAT_BGRA: i32 = 0x4247_5241;
/// kIOSurfaceLockReadOnly.
const LOCK_READ_ONLY: u32 = 1;

// kCGLPFAAccelerated, kCGLPFAOpenGLProfile, kCGLOGLPVersion_Legacy.
const PFA_ACCELERATED: i32 = 73;
const PFA_PROFILE: i32 = 99;
const PROFILE_LEGACY: i32 = 0x1000;

pub struct Cgl {
    /// OpenGL.framework's own image, where entry points are looked up. One
    /// for the process: a channel used to `dlopen` its own, which meant a
    /// framework reference per GL program run and no matching close. Nothing
    /// bad came of it -- the framework is linked anyway, so the count never
    /// reached zero -- but a per-run acquisition with no release is the shape
    /// of a leak whether or not it bites, and one handle is also one fewer
    /// thing that differs between a program that ended politely and one that
    /// did not.
    framework: *mut c_void,
    surfaces: bool,
}

// SAFETY: `framework` is a dlopen handle, usable from any thread; the service
// that owns the backend is only ever used by one thread at a time.
unsafe impl Send for Cgl {}

impl Cgl {
    pub fn new() -> Cgl {
        // Opened once and never closed: dlclose of a framework other code in
        // this process is still using is a well known way to crash, and there
        // is nothing to gain by it.
        static FRAMEWORK: OnceLock<usize> = OnceLock::new();
        let framework = *FRAMEWORK.get_or_init(|| {
            let path = c"/System/Library/Frameworks/OpenGL.framework/OpenGL";
            // SAFETY: a plain dlopen of a system framework this crate links anyway.
            unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) as usize }
        }) as *mut c_void;
        let surfaces = std::env::var("IRIS_HOSTGL_NOSURFACE").as_deref() != Ok("1");
        Cgl { framework, surfaces }
    }
}

impl Default for Cgl {
    fn default() -> Self {
        Cgl::new()
    }
}

impl Backend for Cgl {
    fn name(&self) -> &'static str {
        "CGL"
    }

    fn create_context(&mut self, share: Option<ContextHandle>) -> Option<ContextHandle> {
        // The legacy profile is the one that still has glBegin, display lists
        // and the matrix stack.
        let attrs = [PFA_ACCELERATED, PFA_PROFILE, PROFILE_LEGACY, 0];
        let mut pix = std::ptr::null_mut();
        let mut n = 0;
        // SAFETY: attribute list is zero-terminated; out pointers are valid.
        if unsafe { CGLChoosePixelFormat(attrs.as_ptr(), &mut pix, &mut n) } != 0 || pix.is_null() {
            log::warn!("host GL: CGL has no accelerated legacy pixel format");
            return None;
        }
        let share = share.map_or(std::ptr::null_mut(), |c| c.0 as *mut c_void);
        let mut ctx = std::ptr::null_mut();
        // SAFETY: `pix` is a live pixel format, `share` null or a live context.
        let r = unsafe { CGLCreateContext(pix, share, &mut ctx) };
        unsafe { CGLDestroyPixelFormat(pix) };
        if r != 0 || ctx.is_null() {
            log::warn!("host GL: CGLCreateContext failed ({r})");
            return None;
        }
        Some(ContextHandle(ctx as usize))
    }

    fn destroy_context(&mut self, ctx: ContextHandle) {
        // SAFETY: `ctx` came from create_context and is destroyed once.
        unsafe {
            if CGLGetCurrentContext() as usize == ctx.0 {
                CGLSetCurrentContext(std::ptr::null_mut());
            }
            CGLDestroyContext(ctx.0 as *mut c_void);
        }
    }

    fn make_current(&mut self, ctx: Option<ContextHandle>) -> bool {
        let p = ctx.map_or(std::ptr::null_mut(), |c| c.0 as *mut c_void);
        // SAFETY: null or a live context.
        unsafe { CGLSetCurrentContext(p) == 0 }
    }

    fn current(&self) -> Option<ContextHandle> {
        // SAFETY: a query.
        let p = unsafe { CGLGetCurrentContext() };
        (!p.is_null()).then_some(ContextHandle(p as usize))
    }

    fn lookup(&self, name: &str) -> *const c_void {
        let Ok(c) = std::ffi::CString::new(name) else { return std::ptr::null() };
        let handle = if self.framework.is_null() { libc::RTLD_DEFAULT } else { self.framework };
        // SAFETY: a symbol lookup; the result is only called through a
        // signature generated from the same entry point's declaration.
        unsafe { libc::dlsym(handle, c.as_ptr()) as *const c_void }
    }

    fn new_surface(&mut self, w: i32, h: i32) -> Option<Box<dyn Surface>> {
        if !self.surfaces {
            return None;
        }
        IoSurface::new(w, h).map(|s| Box::new(s) as Box<dyn Surface>)
    }
}

/// A linear, CPU-addressable surface the GPU renders into.
struct IoSurface {
    iosurface: *mut c_void,
    tex: u32,
    h: usize,
    stride: usize,
}

// SAFETY: an IOSurfaceRef may be used from any thread; the texture name is
// only used with a context of its share group current.
unsafe impl Send for IoSurface {}
// SAFETY: shared use is `texture` (a name) and `with_pixels`, whose
// IOSurfaceLock/Unlock pair is thread-safe; deleting the texture takes &mut.
unsafe impl Sync for IoSurface {}

impl IoSurface {
    /// A surface `w` by `h`, wrapped as a rectangle texture in the current
    /// context.
    fn new(w: i32, h: i32) -> Option<IoSurface> {
        if w <= 0 || h <= 0 {
            return None;
        }
        let num = |v: i32| unsafe { CFNumberCreate(std::ptr::null(), CF_NUMBER_INT, &v as *const i32 as *const c_void) };
        let (vw, vh, vb, vf) = (num(w), num(h), num(4), num(PIXEL_FORMAT_BGRA));
        // SAFETY: CoreFoundation objects made and released here; the key and
        // value arrays outlive the dictionary's creation, which retains them.
        let surface = unsafe {
            let keys = [kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement, kIOSurfacePixelFormat];
            let values = [vw, vh, vb, vf];
            let props = if values.iter().any(|v| v.is_null()) {
                std::ptr::null()
            } else {
                CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    keys.len() as isize,
                    &kCFTypeDictionaryKeyCallBacks as *const c_void,
                    &kCFTypeDictionaryValueCallBacks as *const c_void,
                )
            };
            let s = if props.is_null() { std::ptr::null_mut() } else { IOSurfaceCreate(props) };
            for v in values {
                if !v.is_null() {
                    CFRelease(v);
                }
            }
            if !props.is_null() {
                CFRelease(props);
            }
            s
        };
        if surface.is_null() {
            return None;
        }
        let tex = crate::internal_texture_name();
        // SAFETY: a context is current (the caller's contract); the texture
        // name is ours alone (see internal_texture_name).
        let ok = unsafe {
            glBindTexture(GL_TEXTURE_RECTANGLE, tex);
            let r = CGLTexImageIOSurface2D(
                CGLGetCurrentContext(),
                GL_TEXTURE_RECTANGLE,
                GL_RGBA8,
                w as u32,
                h as u32,
                GL_BGRA,
                GL_UNSIGNED_INT_8_8_8_8_REV,
                surface,
                0,
            );
            glBindTexture(GL_TEXTURE_RECTANGLE, 0);
            r == 0
        };
        if !ok {
            // SAFETY: as above; the surface is released once.
            unsafe {
                glDeleteTextures(1, &tex);
                CFRelease(surface as *const c_void);
            }
            return None;
        }
        // SAFETY: a live surface.
        let (stride, height) = unsafe { (IOSurfaceGetBytesPerRow(surface), IOSurfaceGetHeight(surface)) };
        Some(IoSurface { iosurface: surface, tex, h: height.min(h as usize), stride })
    }
}

impl Surface for IoSurface {
    fn texture(&self) -> (u32, u32) {
        (GL_TEXTURE_RECTANGLE, self.tex)
    }

    fn with_pixels(&self, f: &mut dyn FnMut(&[u8], usize)) -> bool {
        let mut seed = 0u32;
        // SAFETY: a live surface; locking waits for GPU work outstanding on it.
        if unsafe { IOSurfaceLock(self.iosurface, LOCK_READ_ONLY, &mut seed) } != 0 {
            return false;
        }
        let base = unsafe { IOSurfaceGetBaseAddress(self.iosurface) } as *const u8;
        let ok = !base.is_null();
        if ok {
            // SAFETY: the surface is `stride` bytes a row for `h` rows, and it
            // stays locked until `f` returns.
            let all = unsafe { std::slice::from_raw_parts(base, self.stride * self.h) };
            f(all, self.stride);
        }
        unsafe { IOSurfaceUnlock(self.iosurface, LOCK_READ_ONLY, &mut seed) };
        ok
    }

    fn delete_texture(&mut self) {
        if self.tex != 0 {
            // SAFETY: the caller has a context of the share group current.
            unsafe { glDeleteTextures(1, &self.tex) };
            self.tex = 0;
        }
    }
}

impl Drop for IoSurface {
    fn drop(&mut self) {
        // The texture is the share group's and goes with it (or was deleted
        // with delete_texture); the surface itself is ours to release.
        // SAFETY: released exactly once.
        unsafe { CFRelease(self.iosurface as *const c_void) };
    }
}
