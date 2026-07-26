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
//! Two orchestration modes live here:
//!
//! - v1 (`run_v1_roundtrip`): two boards, host relays frames between them,
//!   zero payjoin awareness on the host's part.
//! - v2 probe (`run_v2_probe`): one board, host hands it seed bytes and
//!   reports back the ShortId it computed (see harness-device's
//!   `run_v2_probe` docs for why this is a primitive-level probe, not a
//!   live v2 receiver session -- that's not possible on bare-metal today).
//!   Still zero payjoin awareness on the host: it doesn't independently
//!   verify the ShortId is correct, since doing that would mean pulling in
//!   `payjoin` as a host dependency, which is exactly the resolver
//!   complexity keeping `harness-device` out of this workspace in the
//!   first place. It only confirms the round trip happened and reports
//!   what the device said.

use std::io::{Read, Write};

use anyhow::{bail, Context, Result};
use harness_proto::{decode, Command, DecodeError};

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    V1,
    V2Probe,
}

#[derive(Debug)]
pub struct Args {
    pub mode: Mode,
    // v1 fields
    pub sender_port: Option<String>,
    pub receiver_port: Option<String>,
    // v2-probe fields
    pub device_port: Option<String>,
    pub seed: Option<String>,
    pub baud: u32,
}

impl Args {
    pub fn parse() -> Result<Self> { Self::parse_from(std::env::args().skip(1)) }

    pub fn parse_from(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut mode = Mode::V1;
        let mut sender_port = None;
        let mut receiver_port = None;
        let mut device_port = None;
        let mut seed = None;
        let mut baud = 115_200u32;

        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--mode" =>
                    mode = match args.next().context("--mode needs a value")?.as_str() {
                        "v1" => Mode::V1,
                        "v2-probe" => Mode::V2Probe,
                        other => bail!("unknown --mode {other} (expected v1 or v2-probe)"),
                    },
                "--sender-port" =>
                    sender_port = Some(args.next().context("--sender-port needs a value")?),
                "--receiver-port" =>
                    receiver_port = Some(args.next().context("--receiver-port needs a value")?),
                "--device-port" =>
                    device_port = Some(args.next().context("--device-port needs a value")?),
                "--seed" => seed = Some(args.next().context("--seed needs a value")?),
                "--baud" =>
                    baud = args
                        .next()
                        .context("--baud needs a value")?
                        .parse()
                        .context("--baud must be a number")?,
                other => bail!("unknown argument: {other}"),
            }
        }

        match mode {
            Mode::V1 => {
                if sender_port.is_none() {
                    bail!("--sender-port is required for --mode v1");
                }
                if receiver_port.is_none() {
                    bail!("--receiver-port is required for --mode v1");
                }
            }
            Mode::V2Probe => {
                if device_port.is_none() {
                    bail!("--device-port is required for --mode v2-probe");
                }
                if seed.is_none() {
                    bail!("--seed is required for --mode v2-probe");
                }
            }
        }

        Ok(Self { mode, sender_port, receiver_port, device_port, seed, baud })
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

/// Orchestrates the v2 ShortId/mailbox probe against a single board (see
/// `harness-device`'s `run_v2_probe`). Unlike `run_v1_roundtrip`, there's
/// only one board here, not two -- this isn't a live v2 receiver session
/// (see harness-device's module docs for why that's not possible on
/// bare-metal), just a round trip through the one v2 primitive that is:
/// hand the device some seed bytes, get back the ShortId it computed.
///
/// Like `run_v1_roundtrip`, the host doesn't independently verify the
/// payjoin crypto here -- it has no `payjoin` dependency at all, by
/// design (see module docs) -- it only confirms the framing round trip
/// happened and reports what the device said. Verifying the returned
/// ShortId against an independently computed expected value is the
/// caller's job (or a test's, see below), not this function's.
pub fn run_v2_probe<T: Read + Write>(device: &mut FramedPort<T>, seed: &[u8]) -> Result<String> {
    println!("Sending {} byte seed to device...", seed.len());
    device.write_frame(Command::OriginalPsbt, seed)?;

    println!("Waiting for the device's ShortId...");
    let (command, payload) = device.read_frame()?;
    if command != Command::SignedPsbt {
        bail!("expected SignedPsbt (v2 probe response) from device, got {command:?}");
    }

    let encoded = String::from_utf8(payload).context("device returned non-UTF-8 ShortId")?;
    println!("Device reported ShortId: {encoded}");
    Ok(encoded)
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
    fn run_v2_probe_happy_path() {
        let mut device = FramedPort::new(ScriptedPort::preloaded_with_frames(&[(
            Command::SignedPsbt,
            b"bech32m-encoded-shortid",
        )]));

        let encoded = run_v2_probe(&mut device, b"some seed bytes").unwrap();
        assert_eq!(encoded, "bech32m-encoded-shortid");

        // The device should have been sent exactly the seed, framed as
        // OriginalPsbt (same command v1 uses for "here's your input" --
        // there's no dedicated v2 opcode yet).
        let (frame, _) = decode(&device.port.written).unwrap();
        assert_eq!(frame.command, Command::OriginalPsbt);
        assert_eq!(frame.payload, b"some seed bytes");
    }

    #[test]
    fn run_v2_probe_rejects_wrong_command() {
        let mut device =
            FramedPort::new(ScriptedPort::preloaded_with_frames(&[(Command::Status, &[0x00])]));

        let err = run_v2_probe(&mut device, b"seed").unwrap_err();
        assert!(err.to_string().contains("expected SignedPsbt"));
    }

    #[test]
    fn run_v2_probe_rejects_non_utf8_response() {
        let mut device = FramedPort::new(ScriptedPort::preloaded_with_frames(&[(
            Command::SignedPsbt,
            &[0xFF, 0xFE, 0xFD],
        )]));

        let err = run_v2_probe(&mut device, b"seed").unwrap_err();
        assert!(err.to_string().contains("non-UTF-8"));
    }

    #[test]
    fn args_parse_defaults_to_v1_mode() {
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
        assert_eq!(args.mode, Mode::V1);
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

    #[test]
    fn args_parse_v2_probe_mode_requires_device_port_and_seed() {
        let err = Args::parse_from(vec!["--mode".to_string(), "v2-probe".to_string()].into_iter())
            .unwrap_err();
        assert!(err.to_string().contains("--device-port"));

        let err = Args::parse_from(
            vec![
                "--mode".to_string(),
                "v2-probe".to_string(),
                "--device-port".to_string(),
                "/dev/ttyACM0".to_string(),
            ]
            .into_iter(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--seed"));

        let args = Args::parse_from(
            vec![
                "--mode".to_string(),
                "v2-probe".to_string(),
                "--device-port".to_string(),
                "/dev/ttyACM0".to_string(),
                "--seed".to_string(),
                "hello".to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(args.mode, Mode::V2Probe);
        assert_eq!(args.device_port.as_deref(), Some("/dev/ttyACM0"));
        assert_eq!(args.seed.as_deref(), Some("hello"));
    }

    #[test]
    fn args_parse_rejects_unknown_mode() {
        let err =
            Args::parse_from(vec!["--mode".to_string(), "v3".to_string()].into_iter()).unwrap_err();
        assert!(err.to_string().contains("unknown --mode"));
    }
}
