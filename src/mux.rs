//! TUI-side tab handling: spawn detached holders, attach to their sockets,
//! and keep a client-side vt100 screen per tab (ADR-0005).

use crate::detect::{TabKind, TabState};
use crate::proto;
use anyhow::{anyhow, Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Tab {
    pub id: String,
    pub project: usize,
    pub project_name: String,
    pub kind: TabKind,
    pub title: String,
    pub state: TabState,
    parser: Arc<Mutex<vt100::Parser>>,
    stream: UnixStream,
    last_output: Arc<Mutex<Instant>>,
    eof: Arc<AtomicBool>,
    exited_marker: PathBuf,
    size: (u16, u16),
}

/// Spawn a detached pall8t-tab holder (new process group, no stdio, no
/// controlling terminal dependency); returns its pid.
pub fn spawn_holder(
    id: &str,
    socket: &Path,
    rows: u16,
    cols: u16,
    argv: &[String],
) -> Result<u32> {
    let sibling = std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("pall8t-tab"))
        .filter(|p| p.exists());
    let exe = sibling.unwrap_or_else(|| PathBuf::from("pall8t-tab"));
    let mut cmd = Command::new(exe);
    cmd.arg("--id")
        .arg(id)
        .arg("--socket")
        .arg(socket)
        .arg("--rows")
        .arg(rows.to_string())
        .arg("--cols")
        .arg(cols.to_string())
        .arg("--")
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.process_group(0);
    let child = cmd.spawn().context("failed to spawn pall8t-tab")?;
    Ok(child.id())
}

/// Connect to a holder socket; the holder replays its ring buffer into our
/// fresh vt100, then streams live output. Retries briefly (a just-spawned
/// holder may not have bound its socket yet).
#[allow(clippy::too_many_arguments)]
pub fn attach(
    id: &str,
    project: usize,
    project_name: &str,
    kind: TabKind,
    title: &str,
    socket: &Path,
    rows: u16,
    cols: u16,
) -> Result<Tab> {
    let mut stream = connect_retry(socket)?;
    let mut magic = [0u8; 4];
    stream
        .read_exact(&mut magic)
        .context("holder handshake failed")?;
    if &magic != proto::MAGIC {
        return Err(anyhow!("holder protocol mismatch"));
    }

    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 2000)));
    let last_output = Arc::new(Mutex::new(Instant::now()));
    let eof = Arc::new(AtomicBool::new(false));
    {
        let mut reader = stream.try_clone().context("clone stream")?;
        let parser = Arc::clone(&parser);
        let last_output = Arc::clone(&last_output);
        let eof = Arc::clone(&eof);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
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
                    }
                }
            }
        });
    }

    let mut tab = Tab {
        id: id.to_string(),
        project,
        project_name: project_name.to_string(),
        kind,
        title: title.to_string(),
        state: TabState::Working,
        parser,
        stream,
        last_output,
        eof,
        exited_marker: proto::exited_marker(socket),
        size: (0, 0),
    };
    // Nudge: full-screen apps repaint on resize, restoring the screen after
    // a replay-based reattach.
    tab.resize(rows, cols);
    Ok(tab)
}

fn connect_retry(socket: &Path) -> Result<UnixStream> {
    let mut last_err = None;
    for _ in 0..50 {
        match UnixStream::connect(socket) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(40));
            }
        }
    }
    Err(anyhow!(
        "cannot attach to {}: {}",
        socket.display(),
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

impl Tab {
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        let mut w = &self.stream;
        let _ = proto::write_input(&mut w, bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size == (rows, cols) || rows == 0 || cols == 0 {
            return;
        }
        self.size = (rows, cols);
        let mut w = &self.stream;
        let _ = proto::write_resize(&mut w, rows, cols);
        if let Ok(mut p) = self.parser.lock() {
            p.set_size(rows, cols);
        }
    }

    /// Ask the holder to kill its child and exit.
    pub fn kill(&mut self) {
        let mut w = &self.stream;
        let _ = proto::write_kill(&mut w);
    }

    pub fn exited(&self) -> bool {
        self.eof.load(Ordering::Relaxed) || self.exited_marker.exists()
    }

    pub fn since_output(&self) -> Duration {
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
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }

    /// Run `f` with the current screen (short lock — render/detect only).
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> Option<R> {
        self.parser.lock().ok().map(|p| f(p.screen()))
    }
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
