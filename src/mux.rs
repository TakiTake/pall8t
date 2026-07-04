use crate::detect::{TabKind, TabState};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct Tab {
    pub project: usize,
    pub kind: TabKind,
    pub title: String,
    pub state: TabState,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: SharedWriter,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    last_output: Arc<Mutex<Instant>>,
    eof: Arc<AtomicBool>,
    size: (u16, u16),
}

impl Tab {
    /// Spawn `argv[0] argv[1..]` on a fresh PTY of `rows`x`cols`.
    pub fn spawn(
        project: usize,
        kind: TabKind,
        title: String,
        argv: &[String],
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn: {}", argv.join(" ")))?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer: SharedWriter = Arc::new(Mutex::new(
            pair.master.take_writer().context("take writer")?,
        ));

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 2000)));
        let last_output = Arc::new(Mutex::new(Instant::now()));
        let eof = Arc::new(AtomicBool::new(false));

        {
            let parser = Arc::clone(&parser);
            let writer = Arc::clone(&writer);
            let last_output = Arc::clone(&last_output);
            let eof = Arc::clone(&eof);
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut tail: Vec<u8> = Vec::new();
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            eof.store(true, Ordering::Relaxed);
                            break;
                        }
                        Ok(n) => {
                            if let Ok(mut p) = parser.lock() {
                                p.process(&buf[..n]);
                            }
                            if let Ok(mut t) = last_output.lock() {
                                *t = Instant::now();
                            }
                            // A real terminal answers queries like "where is
                            // the cursor?" (DSR); vt100 only parses. Without
                            // replies, apps using them (gh/survey, some TUIs)
                            // block forever. Sniff and answer here.
                            let mut data = std::mem::take(&mut tail);
                            data.extend_from_slice(&buf[..n]);
                            let mut replies: Vec<u8> = Vec::new();
                            tail = answer_queries(&data, &parser, &mut replies);
                            if !replies.is_empty() {
                                if let Ok(mut w) = writer.lock() {
                                    let _ = w.write_all(&replies);
                                    let _ = w.flush();
                                }
                            }
                        }
                    }
                }
            });
        }

        Ok(Self {
            project,
            kind,
            title,
            state: TabState::Working,
            parser,
            writer,
            child,
            master: pair.master,
            last_output,
            eof,
            size: (rows, cols),
        })
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size == (rows, cols) || rows == 0 || cols == 0 {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut p) = self.parser.lock() {
            p.set_size(rows, cols);
        }
    }

    pub fn exited(&mut self) -> bool {
        if self.eof.load(Ordering::Relaxed) {
            return true;
        }
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub fn since_output(&self) -> std::time::Duration {
        self.last_output
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    /// Last `n` non-empty rows of the screen, joined by newlines.
    pub fn bottom_text(&self, n: usize) -> String {
        let contents = match self.parser.lock() {
            Ok(p) => p.screen().contents(),
            Err(_) => return String::new(),
        };
        let lines: Vec<&str> = contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }

    /// Run `f` with the current screen (short lock — render/detect only).
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> Option<R> {
        self.parser.lock().ok().map(|p| f(p.screen()))
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

/// Scan child output for terminal queries and append the replies a real
/// terminal would send. Returns trailing bytes that might be the start of an
/// incomplete query (carried over to the next chunk).
fn answer_queries(
    data: &[u8],
    parser: &Arc<Mutex<vt100::Parser>>,
    out: &mut Vec<u8>,
) -> Vec<u8> {
    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1b {
            i += 1;
            continue;
        }
        if i + 1 >= data.len() {
            return data[i..].to_vec();
        }
        if data[i + 1] != b'[' {
            i += 1;
            continue;
        }
        // CSI: params, then a final byte in 0x40..=0x7e.
        let mut j = i + 2;
        let mut fin: Option<u8> = None;
        while j < data.len() && j - i <= 18 {
            let b = data[j];
            if (0x40..=0x7e).contains(&b) {
                fin = Some(b);
                break;
            }
            j += 1;
        }
        let Some(fin) = fin else {
            if data.len() - i <= 18 {
                return data[i..].to_vec();
            }
            i += 2;
            continue;
        };
        let params = &data[i + 2..j];
        match fin {
            // DSR: cursor position report (this is what gh/survey block on)
            b'n' if params == b"6" => {
                if let Ok(p) = parser.lock() {
                    let (row, col) = p.screen().cursor_position();
                    out.extend_from_slice(
                        format!("\x1b[{};{}R", row + 1, col + 1).as_bytes(),
                    );
                }
            }
            // DSR: device status → OK
            b'n' if params == b"5" => out.extend_from_slice(b"\x1b[0n"),
            // Primary device attributes → VT220 with color
            b'c' if params.is_empty() || params == b"0" => {
                out.extend_from_slice(b"\x1b[?62;22c");
            }
            // Secondary device attributes
            b'c' if params.first() == Some(&b'>') => {
                out.extend_from_slice(b"\x1b[>1;10;0c");
            }
            _ => {}
        }
        i = j + 1;
    }
    Vec::new()
}

/// Encode a key event as the byte sequence a terminal would send.
pub fn encode_key(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    match code {
        KeyCode::Char(c) => {
            if mods.contains(KeyModifiers::CONTROL) {
                let c = c.to_ascii_lowercase();
                if c == ' ' {
                    out.push(0x00);
                } else if c.is_ascii_lowercase() {
                    out.push((c as u8) & 0x1f);
                } else {
                    return None;
                }
            } else {
                if mods.contains(KeyModifiers::ALT) {
                    out.push(0x1b);
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::F(n) => match n {
            1 => out.extend_from_slice(b"\x1bOP"),
            2 => out.extend_from_slice(b"\x1bOQ"),
            3 => out.extend_from_slice(b"\x1bOR"),
            4 => out.extend_from_slice(b"\x1bOS"),
            5 => out.extend_from_slice(b"\x1b[15~"),
            6 => out.extend_from_slice(b"\x1b[17~"),
            7 => out.extend_from_slice(b"\x1b[18~"),
            8 => out.extend_from_slice(b"\x1b[19~"),
            9 => out.extend_from_slice(b"\x1b[20~"),
            10 => out.extend_from_slice(b"\x1b[21~"),
            11 => out.extend_from_slice(b"\x1b[23~"),
            12 => out.extend_from_slice(b"\x1b[24~"),
            _ => return None,
        },
        _ => return None,
    }
    Some(out)
}

/// Wrap pasted text in bracketed-paste markers.
pub fn encode_paste(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}
