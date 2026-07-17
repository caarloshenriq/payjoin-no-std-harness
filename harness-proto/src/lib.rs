//! Length-prefixed serial framing protocol shared between `harness-host`
//! and `harness-device`.
//!
//! Wire format, matching the spec discussed with the rust-payjoin
//! maintainers (see the payjoin/rust-payjoin no_std PR discussion):
//!
//! ```text
//! [u8 command][u16 len_le][payload; len bytes][u16 crc16_le]
//! ```
//!
//! The CRC covers the command byte, the length field, and the payload.
//!
//! This crate is `no_std` and allocation-free: encoding writes into a
//! caller-provided buffer, and decoding borrows from the input slice. That
//! makes it usable as-is on the embedded side (Black Pill / Pico 2 firmware)
//! without pulling in `alloc`, and on the host side inside a normal binary.
//!
//! ## v1 vs v2 payloads
//!
//! The same commands carry both protocol versions. For v1, payloads are
//! the protocol's own plain bytes (there's no separate transport envelope).
//! For v2, payloads are always **decrypted, OHTTP-decapsulated application
//! bytes** -- `harness-device` never sees an OHTTP envelope, a mailbox ID,
//! or touches the directory/relay. `harness-host` is solely responsible for
//! all OHTTP encapsulation/decapsulation and any real (or faked) network
//! I/O; the device only ever processes plaintext payjoin request/response
//! bytes handed to it over this framing.

#![no_std]

/// Maximum payload size in bytes. Chosen to comfortably hold a base64- or
/// binary-encoded PSBT plus BIP78/BIP77 request/response framing; adjust if
/// a real payload turns out to exceed this.
pub const MAX_PAYLOAD_LEN: usize = 4096;

/// Fixed overhead of a frame: 1 (command) + 2 (length) + 2 (crc).
pub const FRAME_OVERHEAD: usize = 5;

/// A command byte identifying the meaning of a frame's payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    /// device -> host: device's pubkey/descriptor material, sent once at
    /// startup so the host knows which role/identity it's talking to.
    InitKeys = 0x01,
    /// host -> device: the original PSBT (v1 sender role), or an incoming
    /// proposal/request (receiver role, either version). For v2, this is
    /// already OHTTP-decapsulated by the host -- the device only ever sees
    /// plain application bytes.
    OriginalPsbt = 0x02,
    /// device -> host: bytes ready to hand to the transport. For v1, these
    /// are the protocol's own bytes (PSBT + params), sent as-is. For v2,
    /// these are decrypted application-layer request/response bytes; the
    /// host is responsible for OHTTP-encapsulating them before they touch
    /// the wire (real or faked).
    OutRequest = 0x03,
    /// host -> device: bytes received from the counterparty or directory.
    /// For v2, already OHTTP-decapsulated by the host before being framed
    /// here.
    InResponse = 0x04,
    /// device -> host: the final, fully signed PSBT.
    SignedPsbt = 0x05,
    /// Either direction: a status/ack/error code. Payload is a single
    /// [`StatusCode`] byte.
    Status = 0x06,
}

impl Command {
    fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(Self::InitKeys),
            0x02 => Some(Self::OriginalPsbt),
            0x03 => Some(Self::OutRequest),
            0x04 => Some(Self::InResponse),
            0x05 => Some(Self::SignedPsbt),
            0x06 => Some(Self::Status),
            _ => None,
        }
    }
}

/// Payload of a [`Command::Status`] frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StatusCode {
    Ok = 0x00,
    Error = 0x01,
}

impl StatusCode {
    fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0x00 => Some(Self::Ok),
            0x01 => Some(Self::Error),
            _ => None,
        }
    }
}

/// A decoded frame borrowing its payload from the buffer it was parsed
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub command: Command,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Not enough bytes buffered yet to even read the header. Not fatal:
    /// the caller should read more bytes and try again.
    Incomplete,
    /// Header claims more payload than fits in a single frame. Likely a
    /// desynced stream; the caller should treat this as a fatal framing
    /// error and probably reset the connection.
    PayloadTooLarge,
    /// The command byte isn't one we recognize.
    UnknownCommand(u8),
    /// CRC didn't match; the frame is corrupt.
    CrcMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// Payload is larger than [`MAX_PAYLOAD_LEN`].
    PayloadTooLarge,
    /// The destination buffer isn't large enough for this frame.
    BufferTooSmall,
}

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF, no reflection).
///
/// NOTE: reconstructed from the wire-format spec in this file's doc
/// comment, not recovered verbatim -- if your local copy of this crate
/// uses a different polynomial/init value, encode/decode are only
/// interoperable within one consistent choice. Diff this against your
/// working copy before replacing it wholesale.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Encode a frame into `out`, returning the number of bytes written.
pub fn encode(command: Command, payload: &[u8], out: &mut [u8]) -> Result<usize, EncodeError> {
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(EncodeError::PayloadTooLarge);
    }
    let total_len = FRAME_OVERHEAD + payload.len();
    if out.len() < total_len {
        return Err(EncodeError::BufferTooSmall);
    }

    out[0] = command as u8;
    let len = payload.len() as u16;
    out[1..3].copy_from_slice(&len.to_le_bytes());
    out[3..3 + payload.len()].copy_from_slice(payload);

    let crc = crc16(&out[0..3 + payload.len()]);
    out[3 + payload.len()..5 + payload.len()].copy_from_slice(&crc.to_le_bytes());

    Ok(total_len)
}

/// Convenience wrapper for encoding a [`StatusCode`] frame.
pub fn encode_status(status: StatusCode, out: &mut [u8]) -> Result<usize, EncodeError> {
    encode(Command::Status, &[status as u8], out)
}

/// Attempt to decode a single frame from the start of `buf`.
///
/// On success, returns the frame and the number of bytes consumed from
/// `buf`; the caller should advance its read cursor by that amount. On
/// [`DecodeError::Incomplete`], the caller should read more bytes and
/// retry; `buf` should not be advanced.
///
/// CRC is checked before the command byte is validated, so a corrupt
/// frame is reported as [`DecodeError::CrcMismatch`] even if the command
/// byte also happens to be unrecognized.
pub fn decode(buf: &[u8]) -> Result<(Frame<'_>, usize), DecodeError> {
    if buf.len() < FRAME_OVERHEAD {
        return Err(DecodeError::Incomplete);
    }
    let len = u16::from_le_bytes([buf[1], buf[2]]) as usize;
    if len > MAX_PAYLOAD_LEN {
        return Err(DecodeError::PayloadTooLarge);
    }
    let total_len = FRAME_OVERHEAD + len;
    if buf.len() < total_len {
        return Err(DecodeError::Incomplete);
    }

    let crc_expected = u16::from_le_bytes([buf[3 + len], buf[4 + len]]);
    let crc_actual = crc16(&buf[0..3 + len]);
    if crc_expected != crc_actual {
        return Err(DecodeError::CrcMismatch);
    }

    let command = Command::from_u8(buf[0]).ok_or(DecodeError::UnknownCommand(buf[0]))?;
    let payload = &buf[3..3 + len];

    Ok((Frame { command, payload }, total_len))
}

/// Decode a [`Command::Status`] frame's payload.
pub fn decode_status(payload: &[u8]) -> Option<StatusCode> {
    payload.first().copied().and_then(StatusCode::from_u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_original_psbt() {
        let payload = b"fake decrypted payjoin bytes";
        let mut buf = [0u8; 64];
        let written = encode(Command::OriginalPsbt, payload, &mut buf).unwrap();
        let (frame, consumed) = decode(&buf[..written]).unwrap();
        assert_eq!(consumed, written);
        assert_eq!(frame.command, Command::OriginalPsbt);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn incomplete_header() {
        let buf = [0x02, 0x05, 0x00];
        assert_eq!(decode(&buf), Err(DecodeError::Incomplete));
    }

    #[test]
    fn incomplete_payload() {
        let mut buf = [0u8; 64];
        let written = encode(Command::OutRequest, b"hello", &mut buf).unwrap();
        assert_eq!(decode(&buf[..written - 1]), Err(DecodeError::Incomplete));
    }

    #[test]
    fn payload_too_large_to_encode() {
        let payload = [0u8; MAX_PAYLOAD_LEN + 1];
        let mut buf = [0u8; MAX_PAYLOAD_LEN + FRAME_OVERHEAD + 1];
        assert_eq!(
            encode(Command::OutRequest, &payload, &mut buf),
            Err(EncodeError::PayloadTooLarge)
        );
    }

    #[test]
    fn buffer_too_small_to_encode() {
        let mut buf = [0u8; 4];
        assert_eq!(encode(Command::Status, &[0x00], &mut buf), Err(EncodeError::BufferTooSmall));
    }

    #[test]
    fn crc_mismatch_is_detected() {
        let mut buf = [0u8; 64];
        let written = encode(Command::OutRequest, b"hello", &mut buf).unwrap();
        // Flip a payload bit without recomputing the CRC.
        buf[3] ^= 0xFF;
        assert_eq!(decode(&buf[..written]), Err(DecodeError::CrcMismatch));
    }

    #[test]
    fn unknown_command_after_valid_crc() {
        let mut buf = [0u8; 64];
        let written = encode(Command::OutRequest, b"hi", &mut buf).unwrap();
        // Corrupt the command byte to something unrecognized, then
        // recompute the CRC so the frame is otherwise well-formed --
        // this isolates the UnknownCommand check from CrcMismatch.
        buf[0] = 0xEE;
        let payload_len = u16::from_le_bytes([buf[1], buf[2]]) as usize;
        let crc = crc16(&buf[0..3 + payload_len]);
        buf[3 + payload_len..5 + payload_len].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(decode(&buf[..written]), Err(DecodeError::UnknownCommand(0xEE)));
    }

    #[test]
    fn status_round_trip() {
        let mut buf = [0u8; 16];
        let written = encode_status(StatusCode::Error, &mut buf).unwrap();
        let (frame, _) = decode(&buf[..written]).unwrap();
        assert_eq!(frame.command, Command::Status);
        assert_eq!(decode_status(frame.payload), Some(StatusCode::Error));
    }
}
