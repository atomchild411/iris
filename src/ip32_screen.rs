//! The O2 harness's screen and keyboard, for `ip32-view`.
//!
//! The harness runs as a cargo test with no main thread to open a window on,
//! so the screen goes out through a shared file and keys come back over a
//! local TCP port; `src/bin/ip32_view.rs` is the other end and documents the
//! file layout.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const HEADER: usize = 64;
/// Room for the largest mode GBE drives (1600x1200 and the 1280x1024 the
/// O2 usually runs), with margin.
const MAX_PIXELS: usize = 2048 * 2048;

/// The writer's end of the shared screen file.
pub struct ScreenFile {
    map: *mut u8,
    len: usize,
    frame: u64,
}

// The mapping is only touched by whoever owns the ScreenFile.
unsafe impl Send for ScreenFile {}

impl ScreenFile {
    /// Create (or recreate) the file at `path`, sized for the largest mode.
    pub fn create(path: &str) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        let len = HEADER + MAX_PIXELS * 4;
        let _ = std::fs::remove_file(path);
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        f.set_len(len as u64)?;
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, f.as_raw_fd(), 0)
        };
        if p == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let map = p as *mut u8;
        unsafe {
            std::ptr::copy_nonoverlapping(b"O2FB".as_ptr(), map, 4);
            std::ptr::copy_nonoverlapping(1u32.to_le_bytes().as_ptr(), map.add(4), 4);
        }
        Ok(Self { map, len, frame: 0 })
    }

    fn put_u32(&mut self, off: usize, v: u32) {
        unsafe { std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), self.map.add(off), 4) };
    }

    /// The pixels to fill for a `w` x `h` frame, as R, G, B, A bytes, row 0
    /// at the top. Call [`Self::publish`] when they are written.
    pub fn pixels(&mut self, w: u32, h: u32) -> &mut [u8] {
        let n = (w as usize * h as usize).min(MAX_PIXELS);
        unsafe { std::slice::from_raw_parts_mut(self.map.add(HEADER), n * 4) }
    }

    /// The frame just written is `w` x `h`; 0 x 0 means the display is off.
    pub fn publish(&mut self, w: u32, h: u32) {
        self.put_u32(8, w);
        self.put_u32(12, h);
        self.frame += 1;
        let f = self.frame;
        unsafe {
            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            std::ptr::copy_nonoverlapping(f.to_le_bytes().as_ptr(), self.map.add(16), 8);
        }
    }
}

impl Drop for ScreenFile {
    fn drop(&mut self) {
        // Leave the file showing "no signal" for a viewer still watching it.
        self.publish(0, 0);
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
    }
}

/// Key presses and releases from `ip32-view`, by winit physical-key name
/// (`KeyA`, `Enter`, `ShiftLeft`, ...), oldest first.
#[derive(Clone, Default)]
pub struct KeyEvents(Arc<Mutex<VecDeque<(String, bool)>>>);

impl KeyEvents {
    /// Listen on 127.0.0.1:`port` for viewers; each sends lines `d NAME` or
    /// `u NAME`. Several may connect, one after another or at once.
    pub fn listen(port: u16) -> std::io::Result<Self> {
        let ev = KeyEvents::default();
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let q = ev.0.clone();
        std::thread::Builder::new().name("ip32-keys".into()).spawn(move || {
            for conn in listener.incoming().flatten() {
                let q = q.clone();
                let _ = std::thread::Builder::new().name("ip32-keys-client".into()).spawn(move || {
                    for line in BufReader::new(conn).lines().map_while(Result::ok) {
                        let mut it = line.split_whitespace();
                        let (Some(dir), Some(name)) = (it.next(), it.next()) else { continue };
                        let down = match dir { "d" => true, "u" => false, _ => continue };
                        q.lock().unwrap().push_back((name.to_string(), down));
                    }
                });
            }
        })?;
        Ok(ev)
    }

    pub fn pop(&self) -> Option<(String, bool)> {
        self.0.lock().unwrap().pop_front()
    }
}

/// Save `px` (R, G, B, A bytes, `w` x `h`) as a PNG, written to a temporary
/// name and renamed, so a reader never sees half a file.
pub fn save_png(path: &str, px: &[u8], w: u32, h: u32) -> Result<(), String> {
    let tmp = format!("{path}.tmp");
    {
        let f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(|e| e.to_string())?;
        wr.write_image_data(px).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// The winit key a viewer's name stands for (its `Debug` name).
pub fn keycode(name: &str) -> Option<winit::keyboard::KeyCode> {
    use winit::keyboard::KeyCode::*;
    Some(match name {
        "KeyA" => KeyA,
        "KeyB" => KeyB,
        "KeyC" => KeyC,
        "KeyD" => KeyD,
        "KeyE" => KeyE,
        "KeyF" => KeyF,
        "KeyG" => KeyG,
        "KeyH" => KeyH,
        "KeyI" => KeyI,
        "KeyJ" => KeyJ,
        "KeyK" => KeyK,
        "KeyL" => KeyL,
        "KeyM" => KeyM,
        "KeyN" => KeyN,
        "KeyO" => KeyO,
        "KeyP" => KeyP,
        "KeyQ" => KeyQ,
        "KeyR" => KeyR,
        "KeyS" => KeyS,
        "KeyT" => KeyT,
        "KeyU" => KeyU,
        "KeyV" => KeyV,
        "KeyW" => KeyW,
        "KeyX" => KeyX,
        "KeyY" => KeyY,
        "KeyZ" => KeyZ,
        "Digit0" => Digit0,
        "Digit1" => Digit1,
        "Digit2" => Digit2,
        "Digit3" => Digit3,
        "Digit4" => Digit4,
        "Digit5" => Digit5,
        "Digit6" => Digit6,
        "Digit7" => Digit7,
        "Digit8" => Digit8,
        "Digit9" => Digit9,
        "F1" => F1,
        "F2" => F2,
        "F3" => F3,
        "F4" => F4,
        "F5" => F5,
        "F6" => F6,
        "F7" => F7,
        "F8" => F8,
        "F9" => F9,
        "F10" => F10,
        "F11" => F11,
        "F12" => F12,
        "Numpad0" => Numpad0,
        "Numpad1" => Numpad1,
        "Numpad2" => Numpad2,
        "Numpad3" => Numpad3,
        "Numpad4" => Numpad4,
        "Numpad5" => Numpad5,
        "Numpad6" => Numpad6,
        "Numpad7" => Numpad7,
        "Numpad8" => Numpad8,
        "Numpad9" => Numpad9,
        "Backquote" => Backquote,
        "Minus" => Minus,
        "Equal" => Equal,
        "BracketLeft" => BracketLeft,
        "BracketRight" => BracketRight,
        "Backslash" => Backslash,
        "Semicolon" => Semicolon,
        "Quote" => Quote,
        "Comma" => Comma,
        "Period" => Period,
        "Slash" => Slash,
        "IntlBackslash" => IntlBackslash,
        "Enter" => Enter,
        "Escape" => Escape,
        "Backspace" => Backspace,
        "Tab" => Tab,
        "Space" => Space,
        "CapsLock" => CapsLock,
        "ShiftLeft" => ShiftLeft,
        "ShiftRight" => ShiftRight,
        "ControlLeft" => ControlLeft,
        "ControlRight" => ControlRight,
        "AltLeft" => AltLeft,
        "AltRight" => AltRight,
        "SuperLeft" => SuperLeft,
        "SuperRight" => SuperRight,
        "ContextMenu" => ContextMenu,
        "ArrowUp" => ArrowUp,
        "ArrowDown" => ArrowDown,
        "ArrowLeft" => ArrowLeft,
        "ArrowRight" => ArrowRight,
        "Insert" => Insert,
        "Delete" => Delete,
        "Home" => Home,
        "End" => End,
        "PageUp" => PageUp,
        "PageDown" => PageDown,
        "PrintScreen" => PrintScreen,
        "ScrollLock" => ScrollLock,
        "Pause" => Pause,
        "NumLock" => NumLock,
        "NumpadAdd" => NumpadAdd,
        "NumpadSubtract" => NumpadSubtract,
        "NumpadMultiply" => NumpadMultiply,
        "NumpadDivide" => NumpadDivide,
        "NumpadDecimal" => NumpadDecimal,
        "NumpadEnter" => NumpadEnter,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_frame_reads_back_through_the_file() {
        let dir = std::env::temp_dir().join(format!("ip32-screen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fb");
        let mut s = ScreenFile::create(path.to_str().unwrap()).unwrap();
        s.pixels(2, 1).copy_from_slice(&[1, 2, 3, 255, 4, 5, 6, 255]);
        s.publish(2, 1);
        let b = std::fs::read(&path).unwrap();
        assert_eq!(&b[0..4], b"O2FB");
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 2);
        assert_eq!(u64::from_le_bytes(b[16..24].try_into().unwrap()), 1);
        assert_eq!(&b[64..72], &[1, 2, 3, 255, 4, 5, 6, 255]);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
