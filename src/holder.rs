//! pall8t-tab: the per-tab session holder (ADR-0005). Owns one PTY running
//! `container exec`, keeps a ring buffer of raw output, and serves a Unix
//! socket: on attach it replays the buffer and then broadcasts live output;
//! clients send input/resize/kill frames. Treat this module as frozen —
//! running holders must keep working across pall8t upgrades.

use crate::proto::{self, ClientFrame};
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RING_CAP: usize = 256 * 1024;

pub struct HolderArgs {
    pub id: String,
    pub socket: PathBuf,
    pub rows: u16,
    pub cols: u16,
    pub argv: Vec<String>,
}

pub fn run(args: HolderArgs) -> Result<()> {
    let _ = std::fs::remove_file(&args.socket);
    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener =
        UnixListener::bind(&args.socket).context("cannot bind holder socket")?;
    let marker = proto::exited_marker(&args.socket);
    let _ = std::fs::remove_file(&marker);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: args.rows,
            cols: args.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty failed")?;
    let mut cmd = CommandBuilder::new(&args.argv[0]);
    cmd.args(&args.argv[1..]);
    cmd.env("TERM", "xterm-256color");
    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("failed to spawn: {}", args.argv.join(" ")))?;
    drop(pair.slave);

    let mut pty_reader = pair.master.try_clone_reader().context("clone reader")?;
    let pty_writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
        pair.master.take_writer().context("take writer")?,
    ));
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));
    let child: Arc<Mutex<Box<dyn Child + Send + Sync>>> = Arc::new(Mutex::new(child));
    let parser = Arc::new(Mutex::new(vt100::Parser::new(args.rows, args.cols, 0)));
    let ring: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::new()));
    let clients: Arc<Mutex<Vec<std::os::unix::net::UnixStream>>> =
        Arc::new(Mutex::new(Vec::new()));

    // PTY output → ring buffer + vt100 + query replies + broadcast.
    {
        let ring = Arc::clone(&ring);
        let parser = Arc::clone(&parser);
        let pty_writer = Arc::clone(&pty_writer);
        let clients = Arc::clone(&clients);
        let marker = marker.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut tail: Vec<u8> = Vec::new();
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = std::fs::File::create(&marker);
                        break;
                    }
                    Ok(n) => {
                        if let Ok(mut r) = ring.lock() {
                            r.extend(&buf[..n]);
                            let excess = r.len().saturating_sub(RING_CAP);
                            if excess > 0 {
                                r.drain(0..excess);
                            }
                        }
                        if let Ok(mut p) = parser.lock() {
                            p.process(&buf[..n]);
                        }
                        // Answer terminal queries (DSR etc.) exactly once,
                        // here — not in the (possibly multiple) TUIs.
                        let mut data = std::mem::take(&mut tail);
                        data.extend_from_slice(&buf[..n]);
                        let mut replies: Vec<u8> = Vec::new();
                        tail = answer_queries(&data, &parser, &mut replies);
                        if !replies.is_empty() {
                            if let Ok(mut w) = pty_writer.lock() {
                                let _ = w.write_all(&replies);
                                let _ = w.flush();
                            }
                        }
                        if let Ok(mut cs) = clients.lock() {
                            cs.retain_mut(|c| c.write_all(&buf[..n]).is_ok());
                        }
                    }
                }
            }
        });
    }

    // Child exit watcher (covers exits that keep the PTY open).
    {
        let child = Arc::clone(&child);
        let marker = marker.clone();
        std::thread::spawn(move || loop {
            if let Ok(mut c) = child.lock() {
                if matches!(c.try_wait(), Ok(Some(_))) {
                    let _ = std::fs::File::create(&marker);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        });
    }

    // Accept loop: replay + register, then handle client frames.
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        if stream.write_all(proto::MAGIC).is_err() {
            continue;
        }
        let replay: Vec<u8> = match ring.lock() {
            Ok(r) => r.iter().copied().collect(),
            Err(_) => Vec::new(),
        };
        if stream.write_all(&replay).is_err() {
            continue;
        }
        let Ok(broadcast_half) = stream.try_clone() else {
            continue;
        };
        if let Ok(mut cs) = clients.lock() {
            cs.push(broadcast_half);
        }
        let pty_writer = Arc::clone(&pty_writer);
        let master = Arc::clone(&master);
        let parser = Arc::clone(&parser);
        let child = Arc::clone(&child);
        let socket = args.socket.clone();
        let marker = marker.clone();
        std::thread::spawn(move || loop {
            match proto::read_frame(&mut stream) {
                Ok(ClientFrame::Input(bytes)) => {
                    if let Ok(mut w) = pty_writer.lock() {
                        let _ = w.write_all(&bytes);
                        let _ = w.flush();
                    }
                }
                Ok(ClientFrame::Resize(rows, cols)) => {
                    if rows > 0 && cols > 0 {
                        if let Ok(m) = master.lock() {
                            let _ = m.resize(PtySize {
                                rows,
                                cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                        }
                        if let Ok(mut p) = parser.lock() {
                            p.set_size(rows, cols);
                        }
                    }
                }
                Ok(ClientFrame::Kill) => {
                    if let Ok(mut c) = child.lock() {
                        let _ = c.kill();
                    }
                    let _ = std::fs::remove_file(&socket);
                    let _ = std::fs::remove_file(&marker);
                    std::process::exit(0);
                }
                Err(_) => break,
            }
        });
    }
    Ok(())
}

/// Scan child output for terminal queries and append the replies a real
/// terminal would send. Returns trailing bytes that might be the start of
/// an incomplete query (carried over to the next chunk).
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
            b'n' if params == b"6" => {
                if let Ok(p) = parser.lock() {
                    let (row, col) = p.screen().cursor_position();
                    out.extend_from_slice(
                        format!("\x1b[{};{}R", row + 1, col + 1).as_bytes(),
                    );
                }
            }
            b'n' if params == b"5" => out.extend_from_slice(b"\x1b[0n"),
            b'c' if params.is_empty() || params == b"0" => {
                out.extend_from_slice(b"\x1b[?62;22c");
            }
            b'c' if params.first() == Some(&b'>') => {
                out.extend_from_slice(b"\x1b[>1;10;0c");
            }
            _ => {}
        }
        i = j + 1;
    }
    Vec::new()
}
