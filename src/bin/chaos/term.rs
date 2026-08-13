//! Terminal plumbing for the live view.
//!
//! Zero dependencies means `stty` and ANSI escapes, which turns out to be
//! everything this instrument needs: put the terminal into character-at-a-time
//! mode, read keys off a background thread, and always — *always* — put the
//! terminal back the way we found it.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    /// Same four, with shift held — the coarse-adjust modifier.
    ShiftUp,
    ShiftDown,
    ShiftLeft,
    ShiftRight,
    Char(char),
    Esc,
}

/// Owns the terminal mode for as long as it is alive.
///
/// `stty -g` gives us the *exact* prior settings as an opaque string, so
/// restoring is a byte-for-byte undo rather than a guess like `stty sane`.
pub struct Term {
    saved: Option<String>,
    /// Asking `stty` for the window size means forking a process, which is far
    /// too expensive to do on every one of twenty frames a second. Once a
    /// second is plenty for catching a resize.
    size: std::cell::Cell<Option<(usize, usize)>>,
    stale: std::cell::Cell<u32>,
}

impl Term {
    pub fn enter() -> Term {
        let saved = stty(&["-g"]).map(|s| s.trim().to_string());
        // -icanon: deliver keys immediately instead of at newline.
        // -echo:   do not paint the keystrokes over our own output.
        // isig stays ON, so ctrl-c remains a real signal and can always
        // rescue a wedged view.
        let _ = stty(&["-icanon", "-echo", "min", "1", "time", "0"]);
        print!("\x1b[?1049h\x1b[?25l\x1b[2J");
        Term { saved, size: std::cell::Cell::new(None), stale: std::cell::Cell::new(0) }
    }

    /// Rows and columns, re-asked about once a second, so resizing the window
    /// works without a SIGWINCH handler and without a fork per frame.
    pub fn size(&self) -> Option<(usize, usize)> {
        let stale = self.stale.get();
        self.stale.set(stale.wrapping_add(1));
        if !stale.is_multiple_of(20) {
            if let Some(s) = self.size.get() {
                return Some(s);
            }
        }
        let out = stty(&["size"])?;
        let mut it = out.split_whitespace();
        let rows = it.next()?.parse().ok()?;
        let cols = it.next()?.parse().ok()?;
        self.size.set(Some((rows, cols)));
        Some((rows, cols))
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        print!("\x1b[?25h\x1b[?1049l");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        match &self.saved {
            Some(s) => {
                let _ = stty(&[s]);
            }
            // We never learned the old settings, so undo what we know we did.
            None => {
                let _ = stty(&["icanon", "echo"]);
            }
        }
    }
}

/// `stty` inherits our stdin, which is the controlling terminal — that avoids
/// the `-f` / `-F` split between BSD and GNU entirely.
fn stty(args: &[&str]) -> Option<String> {
    let out = Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8(out.stdout).ok()
}

/// Non-blocking keyboard input.
///
/// Reading stdin blocks, so it happens on its own thread and arrives here over
/// a channel. Escape sequences straddle reads, so undecodable bytes stay in the
/// buffer until the rest of the sequence turns up.
pub struct Keys {
    rx: Receiver<u8>,
    buf: Vec<u8>,
    /// Frames a lone ESC has been sitting unresolved. A real escape sequence
    /// arrives in one burst; a bare ESC keypress never completes.
    stale: u8,
    pub closed: bool,
}

impl Keys {
    pub fn spawn() -> Keys {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut chunk = [0u8; 64];
            loop {
                match stdin.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        for b in &chunk[..n] {
                            if tx.send(*b).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
        Keys { rx, buf: Vec::new(), stale: 0, closed: false }
    }

    pub fn poll(&mut self) -> Vec<Key> {
        loop {
            match self.rx.try_recv() {
                Ok(b) => self.buf.push(b),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.closed = true;
                    break;
                }
            }
        }

        let mut keys = Vec::new();
        while !self.buf.is_empty() {
            match self.decode() {
                Some((k, used)) => {
                    self.buf.drain(..used);
                    self.stale = 0;
                    keys.push(k);
                }
                // Incomplete sequence: wait for the rest.
                None => {
                    self.stale += 1;
                    if self.stale > 2 {
                        // It was a bare ESC after all.
                        self.buf.clear();
                        self.stale = 0;
                        keys.push(Key::Esc);
                    }
                    break;
                }
            }
        }
        keys
    }

    /// Returns the key and how many bytes it consumed, or `None` if the buffer
    /// holds the start of a sequence whose tail has not arrived yet.
    fn decode(&self) -> Option<(Key, usize)> {
        let b = self.buf.as_slice();
        if b[0] != 0x1b {
            return Some((Key::Char(b[0] as char), 1));
        }
        if b.len() < 3 || b[1] != b'[' {
            return None;
        }
        // CSI: parameter bytes, then a final byte in 0x40..=0x7e.
        let end = b[2..].iter().position(|c| (0x40..=0x7e).contains(c))? + 2;
        // Shift is parameter modifier 2 — `ESC [ 1 ; 2 D` and friends.
        let shift = b[2..end].windows(2).any(|w| w == b";2");
        let key = match b[end] {
            b'A' if shift => Key::ShiftUp,
            b'B' if shift => Key::ShiftDown,
            b'C' if shift => Key::ShiftRight,
            b'D' if shift => Key::ShiftLeft,
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            _ => Key::Esc,
        };
        Some((key, end + 1))
    }
}
