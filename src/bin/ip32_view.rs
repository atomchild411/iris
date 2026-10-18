//! ip32-view FBFILE [KBDPORT [PID]]: a window onto the O2 bring-up harness's
//! screen. With PID, the window closes once that process (the emulator) has
//! gone.
//!
//! The harness (`ip32::bringup`, run as a cargo test) has no main thread to
//! open a window on, so it scans GBE's framebuffer out into a shared file
//! (`IRIS_IP32_FB`) and this process shows it. The file is a 64-byte header
//! followed by pixels:
//!
//! | offset | what                                              |
//! |--------|---------------------------------------------------|
//! | 0      | magic `O2FB`                                      |
//! | 4      | version (1)                                       |
//! | 8      | width in pixels (0 while the display is off)      |
//! | 12     | height                                            |
//! | 16     | frame counter, bumped after each complete frame   |
//! | 64     | `width * height` pixels, 4 bytes each: R, G, B, A |
//!
//! Keys go back the other way: with KBDPORT, each key press and release is
//! sent to 127.0.0.1:KBDPORT as a line, `d KeyA` or `u KeyA` (winit's names
//! for physical keys), and the harness turns them into PS/2 scan codes in
//! whatever set the guest asked its keyboard for. The window reconnects to
//! the file and the port across harness restarts.

use std::io::Write;
use std::net::TcpStream;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use glow::HasContext;
use glutin::config::ConfigTemplateBuilder;
use glutin::context::{ContextApi, ContextAttributesBuilder, GlProfile, PossiblyCurrentContext, Version};
use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin::surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface};
use glutin_winit::DisplayBuilder;
use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowAttributes, WindowId};

const HEADER: usize = 64;
const MAGIC: &[u8; 4] = b"O2FB";

/// The shared file, mapped read-only. Remapped whenever its size changes
/// (the harness recreates it on every start).
struct Shared {
    path: String,
    map: *const u8,
    len: usize,
}

impl Shared {
    fn new(path: String) -> Self {
        Self { path, map: std::ptr::null(), len: 0 }
    }

    fn unmap(&mut self) {
        if !self.map.is_null() {
            unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
            self.map = std::ptr::null();
            self.len = 0;
        }
    }

    /// (width, height, frame, pixels), or None while there is nothing to show.
    fn frame(&mut self) -> Option<(u32, u32, u64, &[u8])> {
        let len = std::fs::metadata(&self.path).map(|m| m.len() as usize).unwrap_or(0);
        if len != self.len {
            self.unmap();
            if len < HEADER {
                return None;
            }
            let f = std::fs::File::open(&self.path).ok()?;
            use std::os::fd::AsRawFd;
            let p = unsafe {
                libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0)
            };
            if p == libc::MAP_FAILED {
                return None;
            }
            self.map = p as *const u8;
            self.len = len;
        }
        if self.map.is_null() {
            return None;
        }
        let all = unsafe { std::slice::from_raw_parts(self.map, self.len) };
        if &all[0..4] != MAGIC {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(all[o..o + 4].try_into().unwrap());
        let (w, h) = (u32_at(8), u32_at(12));
        let frame = u64::from_le_bytes(all[16..24].try_into().unwrap());
        let need = HEADER + w as usize * h as usize * 4;
        if w == 0 || h == 0 || need > all.len() {
            return None;
        }
        Some((w, h, frame, &all[HEADER..need]))
    }
}

struct Gl {
    window: Window,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    gl: glow::Context,
    tex: glow::Texture,
    fbo: glow::Framebuffer,
    tex_size: (u32, u32),
}

struct App {
    shared: Shared,
    kbd_port: Option<u16>,
    kbd: Option<TcpStream>,
    kbd_retry: Instant,
    gl: Option<Gl>,
    shown: Option<(u32, u32, u64)>,
    /// Close when this process exits.
    owner: Option<i32>,
    owner_check: Instant,
    /// Redraw only for a new frame, a resize, or an expose.
    dirty: bool,
}

impl App {
    fn send_key(&mut self, name: &str, down: bool) {
        let Some(port) = self.kbd_port else { return };
        if self.kbd.is_none() && Instant::now() >= self.kbd_retry {
            self.kbd = TcpStream::connect(("127.0.0.1", port)).ok();
            if self.kbd.is_none() {
                self.kbd_retry = Instant::now() + Duration::from_secs(1);
            }
        }
        if let Some(s) = self.kbd.as_mut() {
            let line = format!("{} {}\n", if down { "d" } else { "u" }, name);
            if s.write_all(line.as_bytes()).is_err() {
                self.kbd = None;
            }
        }
    }

    fn draw(&mut self) {
        let Some(g) = self.gl.as_mut() else { return };
        let frame = self.shared.frame();
        // Nothing new: leave the last picture up rather than redrawing it
        // sixty times a second.
        let id = frame.as_ref().map(|(w, h, n, _)| (*w, *h, *n));
        if !self.dirty && id == self.shown {
            return;
        }
        self.dirty = false;
        let size = g.window.inner_size();
        unsafe {
            let gl = &g.gl;
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
            gl.viewport(0, 0, size.width as i32, size.height as i32);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            match frame {
                Some((w, h, n, px)) => {
                    if g.tex_size != (w, h) {
                        gl.bind_texture(glow::TEXTURE_2D, Some(g.tex));
                        gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w as i32, h as i32, 0,
                            glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                        g.tex_size = (w, h);
                        let _ = g.window.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
                        g.window.set_title(&format!("O2 {w}x{h}"));
                    }
                    gl.bind_texture(glow::TEXTURE_2D, Some(g.tex));
                    gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, w as i32, h as i32,
                        glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(px)));
                    gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(g.fbo));
                    // Fit the frame in the window, keeping its shape; row 0 is
                    // the top of the screen, so the blit flips it.
                    let (ww, wh) = (size.width as f32, size.height as f32);
                    let s = (ww / w as f32).min(wh / h as f32);
                    let (dw, dh) = ((w as f32 * s) as i32, (h as f32 * s) as i32);
                    let (x0, y0) = ((size.width as i32 - dw) / 2, (size.height as i32 - dh) / 2);
                    gl.blit_framebuffer(0, 0, w as i32, h as i32, x0, y0 + dh, x0 + dw, y0,
                        glow::COLOR_BUFFER_BIT, glow::NEAREST);
                    gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                    self.shown = Some((w, h, n));
                }
                None => {
                    if self.shown.is_some() {
                        g.window.set_title("O2 (no signal)");
                        self.shown = None;
                    }
                }
            }
        }
        let _ = g.surface.swap_buffers(&g.context);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gl.is_some() {
            return;
        }
        let attrs = WindowAttributes::default()
            .with_title("O2 (no signal)")
            .with_resizable(true)
            .with_inner_size(winit::dpi::PhysicalSize::new(1280u32, 1024u32));
        let (window, config) = DisplayBuilder::new()
            .with_window_attributes(Some(attrs))
            .build(event_loop, ConfigTemplateBuilder::new(), |mut c| c.next().unwrap())
            .expect("ip32-view: no GL config");
        let window = window.expect("ip32-view: no window");
        let raw = window.window_handle().unwrap().as_raw();
        let display = config.display();
        let ctx_attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 2))))
            .with_profile(GlProfile::Core)
            .build(Some(raw));
        let not_current = unsafe { display.create_context(&config, &ctx_attrs) }.expect("ip32-view: no GL 3.2 context");
        let sz = window.inner_size();
        let surf_attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw, NonZeroU32::new(sz.width.max(1)).unwrap(), NonZeroU32::new(sz.height.max(1)).unwrap());
        let surface = unsafe { display.create_window_surface(&config, &surf_attrs) }.expect("ip32-view: no surface");
        let context = not_current.make_current(&surface).expect("ip32-view: make_current");
        let _ = surface.set_swap_interval(&context, SwapInterval::Wait(NonZeroU32::new(1).unwrap()));
        let gl = unsafe {
            glow::Context::from_loader_function(|s| display.get_proc_address(&std::ffi::CString::new(s).unwrap()))
        };
        let (tex, fbo) = unsafe {
            let tex = gl.create_texture().unwrap();
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, 1, 1, 0,
                glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            let fbo = gl.create_framebuffer().unwrap();
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::READ_FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            (tex, fbo)
        };
        self.gl = Some(Gl { window, surface, context, gl, tex, fbo, tex_size: (1, 1) });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(sz) => {
                self.dirty = true;
                if let Some(g) = self.gl.as_ref() {
                    g.surface.resize(&g.context, NonZeroU32::new(sz.width.max(1)).unwrap(),
                        NonZeroU32::new(sz.height.max(1)).unwrap());
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    self.send_key(&format!("{code:?}"), event.state == ElementState::Pressed);
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Focused(_) | WindowEvent::Occluded(false) => self.dirty = true,
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(pid) = self.owner {
            if self.owner_check.elapsed() >= Duration::from_secs(1) {
                self.owner_check = Instant::now();
                // Signal 0 checks the process is there without touching it.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    event_loop.exit();
                    return;
                }
            }
        }
        if let Some(g) = self.gl.as_ref() {
            g.window.request_redraw();
        }
        // About 60 polls a second while there is a picture, 4 while there is not.
        let wait = if self.shown.is_some() { 16 } else { 250 };
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(wait)));
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: ip32-view FBFILE [KBDPORT [PID]]");
        std::process::exit(2);
    };
    let kbd_port = args.next().and_then(|p| p.parse().ok());
    let owner = args.next().and_then(|p| p.parse().ok());
    let event_loop = EventLoop::new().expect("ip32-view: no event loop");
    let mut app = App {
        shared: Shared::new(path),
        kbd_port,
        kbd: None,
        kbd_retry: Instant::now(),
        gl: None,
        shown: None,
        owner,
        owner_check: Instant::now(),
        dirty: true,
    };
    event_loop.run_app(&mut app).expect("ip32-view: event loop");
}
