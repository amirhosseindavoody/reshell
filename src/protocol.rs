use std::io::{self, Read, Write};

use anyhow::{bail, Context, Result};

/// Detach key: Ctrl+\ (ASCII FS, 0x1c).
pub const DETACH_BYTE: u8 = 0x1c;

pub const MSG_DATA: u8 = 1;
pub const MSG_RESIZE: u8 = 2;
pub const MSG_DETACH: u8 = 3;
pub const MSG_ATTACH: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug)]
pub enum Message {
    Data(Vec<u8>),
    Resize(Winsize),
    Detach,
    Attach(Winsize),
}

pub fn write_message(mut w: impl Write, msg: &Message) -> Result<()> {
    let bytes = encode_message(msg)?;
    w.write_all(&bytes).context("write message")?;
    Ok(())
}

/// Encode a message to a complete framed byte buffer (safe to write partially).
pub fn encode_message(msg: &Message) -> Result<Vec<u8>> {
    let (kind, payload): (u8, &[u8]) = match msg {
        Message::Data(data) => (MSG_DATA, data.as_slice()),
        Message::Resize(ws) => {
            return encode_winsize(MSG_RESIZE, *ws);
        }
        Message::Detach => (MSG_DETACH, &[]),
        Message::Attach(ws) => {
            return encode_winsize(MSG_ATTACH, *ws);
        }
    };
    encode_framed(kind, payload)
}

fn encode_winsize(kind: u8, ws: Winsize) -> Result<Vec<u8>> {
    let mut payload = [0u8; 4];
    payload[0..2].copy_from_slice(&ws.rows.to_le_bytes());
    payload[2..4].copy_from_slice(&ws.cols.to_le_bytes());
    encode_framed(kind, &payload)
}

fn encode_framed(kind: u8, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > u32::MAX as usize {
        bail!("message payload too large");
    }
    let mut out = Vec::with_capacity(1 + 4 + payload.len());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Push bytes into `buf` and decode as many complete messages as possible.
pub fn drain_messages(buf: &mut Vec<u8>) -> Result<Vec<Message>> {
    let mut out = Vec::new();
    loop {
        if buf.len() < 5 {
            break;
        }
        let len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
        let total = 5 + len;
        if buf.len() < total {
            break;
        }
        let frame: Vec<u8> = buf.drain(..total).collect();
        let mut cursor = io::Cursor::new(frame);
        match read_message(&mut cursor)? {
            Some(msg) => out.push(msg),
            None => break,
        }
    }
    Ok(out)
}

pub fn read_message(mut r: impl Read) -> Result<Option<Message>> {
    let mut kind = [0u8; 1];
    match r.read_exact(&mut kind) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("read message type"),
    }

    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)
        .context("read message length")?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)
            .context("read message payload")?;
    }

    let msg = match kind[0] {
        MSG_DATA => Message::Data(payload),
        MSG_RESIZE => Message::Resize(parse_winsize(&payload)?),
        MSG_DETACH => Message::Detach,
        MSG_ATTACH => Message::Attach(parse_winsize(&payload)?),
        other => bail!("unknown message type {other}"),
    };
    Ok(Some(msg))
}

fn parse_winsize(payload: &[u8]) -> Result<Winsize> {
    if payload.len() != 4 {
        bail!("winsize payload must be 4 bytes");
    }
    let rows = u16::from_le_bytes([payload[0], payload[1]]);
    let cols = u16::from_le_bytes([payload[2], payload[3]]);
    Ok(Winsize { rows, cols })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_messages() {
        let messages = [
            Message::Data(b"hello".to_vec()),
            Message::Resize(Winsize {
                rows: 24,
                cols: 80,
            }),
            Message::Detach,
            Message::Attach(Winsize {
                rows: 40,
                cols: 120,
            }),
        ];

        for original in &messages {
            let mut buf = Vec::new();
            write_message(&mut buf, original).unwrap();
            let mut cursor = Cursor::new(buf);
            let decoded = read_message(&mut cursor).unwrap().unwrap();
            match (original, &decoded) {
                (Message::Data(a), Message::Data(b)) => assert_eq!(a, b),
                (Message::Resize(a), Message::Resize(b)) => assert_eq!(a, b),
                (Message::Detach, Message::Detach) => {}
                (Message::Attach(a), Message::Attach(b)) => assert_eq!(a, b),
                _ => panic!("mismatch"),
            }
        }
    }
}
