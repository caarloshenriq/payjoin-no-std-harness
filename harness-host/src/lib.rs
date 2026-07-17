//! Orchestrates a payjoin v1 round trip between two boards connected over
//! serial: one running the sender role, one running the receiver role
//! (see `harness-device`). The host never makes payjoin decisions -- it
//! only relays frames between the two boards and reports what happened.
//!
//! Phase 1 (this file): validate the plumbing with two boards directly
//! wired to the host, no real Bitcoin network involved -- the "original
//! PSBT" is a fixture, not a real wallet's UTXO.
//!
//! Phase 2 (not yet implemented here): fund real wallets on regtest and
//! assert the resulting transaction actually confirms. Left out for now
//! so this can be validated against real hardware first; the regtest
//! orchestration itself doesn't need hardware to develop against.
//!
//! NOTE: this orchestration is v1-only. It assumes both endpoints are
//! boards running the full sender/receiver payjoin logic themselves, and
//! the host is a dumb relay between them. v2 doesn't fit this shape --
//! there the host itself plays the sender role and does OHTTP
//! encapsulation/decapsulation, with only the receiver on a board. That
//! needs its own orchestration function (and likely its own `--mode`
//! flag on this binary), not a generalization of `run_v1_roundtrip`.

use std::io::{Read, Write};

use anyhow::{bail, Context, Result};
use harness_proto::{decode, Command, DecodeError};

#[derive(Debug)]
pub struct Args {
    pub sender_port: String,
    pub receiver_port: String,
    pub baud: u32,
}

impl Args {
    pub fn parse() -> Result<Self> { Self::parse_from(std::env::args().skip(1)) }

    pub fn parse_from(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut sender_port = None;
        let mut receiver_port = None;
        let mut baud = 115_200u32;

        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--sender-port" =>
                    sender_port = Some(args.next().context("--sender-port needs a value")?),
                "--receiver-port" =>
                    receiver_port = Some(args.next().context("--receiver-port needs a value")?),
                "--baud" =>
                    baud = args
                        .next()
                        .context("--baud needs a value")?
                        .parse()
                        .context("--baud must be a number")?,
                other => bail!("unknown argument: {other}"),
            }
        }

        Ok(Self {
            sender_port: sender_port.context("--sender-port is required")?,
            receiver_port: receiver_port.context("--receiver-port is required")?,
            baud,
        })
    }
}

/// A framed duplex port: pairs a byte stream with a **persistent** read
/// buffer.
///
/// This buffer must not be local to a single `read_frame` call. A single
/// physical `.read()` can return more bytes than one frame's worth (the OS
/// may have buffered several frames' worth of serial data before we got
/// around to reading), and if the leftover bytes after decoding the first
/// frame aren't kept somewhere that survives past the current call, they're
/// silently lost -- the next `read_frame` call starts from an empty buffer
/// and waits forever for bytes that already arrived. Keeping `buf` on the
/// struct instead of on the stack fixes that.
pub struct FramedPort<T> {
    port: T,
    buf: Vec<u8>,
}

impl<T: Read + Write> FramedPort<T> {
    pub fn new(port: T) -> Self { Self { port, buf: Vec::new() } }

    /// Read bytes until a complete frame is decoded, growing/draining the
    /// persistent buffer as needed. Unlike `harness-device`'s `recv_frame`
    /// (which works with a fixed-size buffer since it has no heap-backed
    /// growth story worth using on a microcontroller), the host side can
    /// just use a `Vec` since it has a real allocator and no flash-size
    /// constraints.
    pub fn read_frame(&mut self) -> Result<(Command, Vec<u8>)> {
        let mut chunk = [0u8; 256];
        loop {
            match decode(&self.buf) {
                Ok((frame, consumed)) => {
                    let command = frame.command;
                    let payload = frame.payload.to_vec();
                    self.buf.drain(..consumed);
                    return Ok((command, payload));
                }
                Err(DecodeError::Incomplete) => {}
                Err(e) => bail!("framing error: {e:?}"),
            }

            let n = self.port.read(&mut chunk).context("serial read failed")?;
            if n == 0 {
                bail!("serial port closed before a full frame arrived");
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    pub fn write_frame(&mut self, command: Command, payload: &[u8]) -> Result<()> {
        let mut scratch = vec![0u8; payload.len() + harness_proto::FRAME_OVERHEAD];
        let written = harness_proto::encode(command, payload, &mut scratch)
            .map_err(|e| anyhow::anyhow!("failed to encode frame: {e:?}"))?;
        self.port.write_all(&scratch[..written]).context("serial write failed")?;
        Ok(())
    }
}

/// The v1 orchestration itself, generic over anything that behaves like a
/// duplex byte stream. Real usage wraps `Box<dyn serialport::SerialPort>`
/// (which implements `Read + Write`) in a [`FramedPort`]; tests wrap an
/// in-memory double, so this whole state machine is exercised without any
/// hardware attached.
pub fn run_v1_roundtrip<S, R>(
    sender: &mut FramedPort<S>,
    receiver: &mut FramedPort<R>,
) -> Result<()>
where
    S: Read + Write,
    R: Read + Write,
{
    println!("Waiting for the sender board's request...");
    let (command, request_bytes) = sender.read_frame()?;
    if command != Command::OutRequest {
        bail!("expected OutRequest from sender board, got {command:?}");
    }
    println!("Got {} byte request from sender, relaying to receiver...", request_bytes.len());

    receiver.write_frame(Command::OriginalPsbt, &request_bytes)?;

    println!("Waiting for the receiver board's signed proposal...");
    let (command, proposal_bytes) = receiver.read_frame()?;
    if command != Command::SignedPsbt {
        bail!("expected SignedPsbt from receiver board, got {command:?}");
    }
    println!("Got {} byte proposal from receiver, relaying to sender...", proposal_bytes.len());

    sender.write_frame(Command::InResponse, &proposal_bytes)?;

    println!("Waiting for the sender board to confirm it finalized the PSBT...");
    let (command, _) = sender.read_frame()?;
    match command {
        Command::Status => println!("Sender reported completion. Round trip done."),
        other => bail!("expected a Status frame from sender at the end, got {other:?}"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// A scripted in-memory duplex: bytes queued into `to_read` are handed
    /// back on `.read()` in order; everything written via `.write()` is
    /// captured in `written` for the test to inspect afterward. Doesn't
    /// model real causality (it doesn't react to what's written) -- for
    /// the structural round-trip tests here that's enough, since each
    /// board's scripted responses don't actually depend on host behavior
    /// beyond "did it ask".
    struct ScriptedPort {
        to_read: VecDeque<u8>,
        written: Vec<u8>,
    }

    impl ScriptedPort {
        fn preloaded_with_frames(frames: &[(Command, &[u8])]) -> Self {
            let mut to_read = VecDeque::new();
            for (command, payload) in frames {
                let mut scratch = vec![0u8; payload.len() + harness_proto::FRAME_OVERHEAD];
                let written = harness_proto::encode(*command, payload, &mut scratch).unwrap();
                to_read.extend(scratch[..written].iter().copied());
            }
            Self { to_read, written: Vec::new() }
        }
    }

    impl Read for ScriptedPort {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.to_read.is_empty() {
                // Real serial ports block; a scripted test that runs out
                // of canned bytes has a bug in its script, not a
                // legitimate "port closed" -- fail loudly instead of
                // returning Ok(0) and letting read_frame report a
                // confusing "port closed" error.
                panic!("ScriptedPort ran out of preloaded bytes -- script is missing a frame");
            }
            let n = buf.len().min(self.to_read.len());
            for slot in buf.iter_mut().take(n) {
                *slot = self.to_read.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl Write for ScriptedPort {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    #[test]
    fn write_frame_then_read_frame_round_trips() {
        // Cursor<Vec<u8>> implements both Read and Write, unlike a bare
        // Vec<u8> (Write only), so it satisfies FramedPort's bound.
        let mut port = FramedPort::new(std::io::Cursor::new(Vec::<u8>::new()));
        port.write_frame(Command::OriginalPsbt, b"decrypted bytes").unwrap();

        let written = port.port.into_inner();
        let mut reader = FramedPort::new(std::io::Cursor::new(written));
        let (command, payload) = reader.read_frame().unwrap();
        assert_eq!(command, Command::OriginalPsbt);
        assert_eq!(payload, b"decrypted bytes");
    }

    #[test]
    fn read_frame_accumulates_across_short_reads() {
        // A reader that only ever hands back 1 byte per call, to make sure
        // read_frame's buffering loop actually reassembles a frame spread
        // across many small reads instead of assuming one read = one frame
        // (which is not a safe assumption for real serial ports).
        struct OneByteAtATime(std::collections::VecDeque<u8>);
        impl Read for OneByteAtATime {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.pop_front() {
                    Some(b) => {
                        buf[0] = b;
                        Ok(1)
                    }
                    None => Ok(0),
                }
            }
        }
        impl Write for OneByteAtATime {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { Ok(buf.len()) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }

        let mut scratch = vec![0u8; b"hi".len() + harness_proto::FRAME_OVERHEAD];
        let written = harness_proto::encode(Command::Status, b"hi", &mut scratch).unwrap();
        let source = OneByteAtATime(scratch[..written].iter().copied().collect());

        let mut port = FramedPort::new(source);
        let (command, payload) = port.read_frame().unwrap();
        assert_eq!(command, Command::Status);
        assert_eq!(payload, b"hi");
    }

    /// Regression test for a real bug found while writing these tests: the
    /// original `read_frame` used a buffer local to the function, so if a
    /// single `.read()` returned more than one frame's worth of bytes (the
    /// OS had buffered several frames before we read), everything after
    /// the first decoded frame was silently discarded. This reproduces
    /// that exact shape -- two frames available in one read -- and checks
    /// both are retrievable across two `read_frame` calls.
    #[test]
    fn read_frame_retains_leftover_bytes_from_multi_frame_reads() {
        let mut scratch1 = vec![0u8; b"first".len() + harness_proto::FRAME_OVERHEAD];
        let n1 = harness_proto::encode(Command::OutRequest, b"first", &mut scratch1).unwrap();
        let mut scratch2 = vec![0u8; 1 + harness_proto::FRAME_OVERHEAD];
        let n2 = harness_proto::encode(Command::Status, &[0x00], &mut scratch2).unwrap();

        let mut both = Vec::new();
        both.extend_from_slice(&scratch1[..n1]);
        both.extend_from_slice(&scratch2[..n2]);

        // A single read() call hands back both frames' bytes at once, the
        // way a real serial port can if several frames arrived before we
        // got around to reading.
        let mut port = FramedPort::new(std::io::Cursor::new(both));

        let (command, payload) = port.read_frame().unwrap();
        assert_eq!(command, Command::OutRequest);
        assert_eq!(payload, b"first");

        let (command, _) = port.read_frame().unwrap();
        assert_eq!(command, Command::Status);
    }

    #[test]
    fn roundtrip_happy_path() {
        let mut sender = FramedPort::new(ScriptedPort::preloaded_with_frames(&[
            (Command::OutRequest, b"original psbt fixture"),
            (Command::Status, &[0x00]),
        ]));
        let mut receiver = FramedPort::new(ScriptedPort::preloaded_with_frames(&[(
            Command::SignedPsbt,
            b"signed psbt fixture",
        )]));

        run_v1_roundtrip(&mut sender, &mut receiver).unwrap();

        // The receiver should have been handed exactly the sender's
        // request, framed as OriginalPsbt.
        let (frame, _) = decode(&receiver.port.written).unwrap();
        assert_eq!(frame.command, Command::OriginalPsbt);
        assert_eq!(frame.payload, b"original psbt fixture");

        // The sender should have been handed the receiver's proposal,
        // framed as InResponse.
        let (frame, _) = decode(&sender.port.written).unwrap();
        assert_eq!(frame.command, Command::InResponse);
        assert_eq!(frame.payload, b"signed psbt fixture");
    }

    #[test]
    fn roundtrip_rejects_wrong_command_from_sender() {
        let mut sender = FramedPort::new(ScriptedPort::preloaded_with_frames(&[(
            Command::SignedPsbt,
            b"not a request",
        )]));
        let mut receiver = FramedPort::new(ScriptedPort::preloaded_with_frames(&[]));

        let err = run_v1_roundtrip(&mut sender, &mut receiver).unwrap_err();
        assert!(err.to_string().contains("expected OutRequest"));
    }

    #[test]
    fn roundtrip_rejects_wrong_command_from_receiver() {
        let mut sender = FramedPort::new(ScriptedPort::preloaded_with_frames(&[(
            Command::OutRequest,
            b"original psbt",
        )]));
        let mut receiver =
            FramedPort::new(ScriptedPort::preloaded_with_frames(&[(Command::Status, &[0x01])]));

        let err = run_v1_roundtrip(&mut sender, &mut receiver).unwrap_err();
        assert!(err.to_string().contains("expected SignedPsbt"));
    }

    #[test]
    fn args_parse_requires_both_ports() {
        let err = Args::parse_from(
            vec!["--sender-port".to_string(), "/dev/ttyACM0".to_string()].into_iter(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--receiver-port"));
    }

    #[test]
    fn args_parse_defaults_baud_and_accepts_override() {
        let args = Args::parse_from(
            vec![
                "--sender-port".to_string(),
                "/dev/ttyACM0".to_string(),
                "--receiver-port".to_string(),
                "/dev/ttyACM1".to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(args.baud, 115_200);

        let args = Args::parse_from(
            vec![
                "--sender-port".to_string(),
                "/dev/ttyACM0".to_string(),
                "--receiver-port".to_string(),
                "/dev/ttyACM1".to_string(),
                "--baud".to_string(),
                "9600".to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(args.baud, 9600);
    }
}
