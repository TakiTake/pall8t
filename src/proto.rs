//! Holder ⇄ TUI socket protocol. Version-frozen: old holders must keep
//! working with new TUIs (ADR-0005), so change nothing here lightly.
//!
//! On connect the holder sends MAGIC, replays its ring buffer, then streams
//! live PTY output as raw bytes. The client sends framed messages.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 4] = b"P8T1";

const FRAME_INPUT: u8 = 1;
const FRAME_RESIZE: u8 = 2;
const FRAME_KILL: u8 = 3;

const MAX_INPUT: usize = 1 << 20;

pub enum ClientFrame {
    Input(Vec<u8>),
    Resize(u16, u16),
    Kill,
}

pub fn write_input<W: Write>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    w.write_all(&[FRAME_INPUT])?;
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

pub fn write_resize<W: Write>(w: &mut W, rows: u16, cols: u16) -> io::Result<()> {
    w.write_all(&[FRAME_RESIZE])?;
    w.write_all(&rows.to_le_bytes())?;
    w.write_all(&cols.to_le_bytes())?;
    w.flush()
}

pub fn write_kill<W: Write>(w: &mut W) -> io::Result<()> {
    w.write_all(&[FRAME_KILL])?;
    w.flush()
}

pub fn read_frame<R: Read>(r: &mut R) -> io::Result<ClientFrame> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    match tag[0] {
        FRAME_INPUT => {
            let mut len = [0u8; 4];
            r.read_exact(&mut len)?;
            let n = u32::from_le_bytes(len) as usize;
            if n > MAX_INPUT {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
            }
            let mut bytes = vec![0u8; n];
            r.read_exact(&mut bytes)?;
            Ok(ClientFrame::Input(bytes))
        }
        FRAME_RESIZE => {
            let mut b = [0u8; 4];
            r.read_exact(&mut b)?;
            Ok(ClientFrame::Resize(
                u16::from_le_bytes([b[0], b[1]]),
                u16::from_le_bytes([b[2], b[3]]),
            ))
        }
        FRAME_KILL => Ok(ClientFrame::Kill),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unknown frame")),
    }
}

/// Sentinel file the holder creates when its child exits.
pub fn exited_marker(socket: &Path) -> PathBuf {
    PathBuf::from(format!("{}.exited", socket.display()))
}
