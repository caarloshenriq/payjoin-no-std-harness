//! Standalone tool: plays the payjoin v1 sender role for real, over a
//! real serial port, against a real receiver board running
//! usb-harness/`harness_device::run_receiver`.
//!
//! This exists because there's currently only one board (the Black Pill
//! running the receiver role) -- harness-host's `--mode v1` expects two
//! boards and just relays frames between them, so it can't drive a round
//! trip with only one board present. This tool plays the missing sender
//! side itself, on the host.
//!
//! IMPORTANT: this does NOT call harness_device::run_sender directly.
//! run_sender emits its request tagged Command::OutRequest ("device ->
//! host: here's my request") -- in the real two-board setup,
//! harness-host translates that to Command::OriginalPsbt before relaying
//! to the receiver board (see run_v1_roundtrip in harness-host's
//! lib.rs), since run_receiver only accepts OriginalPsbt and rejects
//! anything else as UnexpectedCommand. Likewise the receiver responds
//! tagged SignedPsbt, which harness-host translates to InResponse before
//! relaying back to the sender board.
//!
//! Since this tool talks directly to the receiver with no host in
//! between, it plays the host's translation role itself by speaking the
//! receiver's vocabulary directly (send OriginalPsbt, expect SignedPsbt)
//! instead of routing through run_sender's OutRequest/InResponse
//! vocabulary and a translation shim on top.
//!
//! Usage:
//!   sender-sim /dev/ttyACM0

use std::io::{Read, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bitcoin::{Amount, FeeRate};
use harness_device::{original_psbt_fixture, Transport};
use harness_proto::Command;
use payjoin::send::v1::SenderBuilder;
use payjoin::PjParam;

struct SerialTransport(Box<dyn serialport::SerialPort>);

impl Transport for SerialTransport {
    type Error = std::io::Error;

    fn send(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.write_all(bytes)?;
        self.0.flush()
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        match self.0.read(buf) {
            Ok(n) => Ok(n),
            // A read timeout just means "nothing arrived yet, try
            // again" -- matches Transport::recv's documented contract
            // of returning 0 when nothing is available.
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(0),
            Err(e) => Err(e),
        }
    }
}

fn main() -> Result<()> {
    let port_name = std::env::args().nth(1).context("usage: sender-sim <device-port>")?;

    let port = serialport::new(&port_name, 115_200)
        .timeout(Duration::from_millis(500))
        .open()
        .with_context(|| format!("failed to open {port_name}"))?;

    let mut transport = SerialTransport(port);

    let (original_psbt, receiver_address) = original_psbt_fixture();

    let pj_param = match PjParam::parse("https://example.com/")
        .map_err(|e| anyhow::anyhow!("invalid endpoint: {e:?}"))?
    {
        payjoin::PjParam::V1(v1_param) => v1_param,
        _ => bail!("expected a v1 PjParam"),
    };

    let sender = SenderBuilder::from_parts(original_psbt, &pj_param, &receiver_address, None)
        .build_with_additional_fee(Amount::from_sat(182), Some(0), FeeRate::ZERO, true)
        .map_err(|e| anyhow::anyhow!("failed to build sender: {e:?}"))?;
    let (request, v1_context) = sender.create_v1_post_request();

    println!("Sending original PSBT ({} bytes) to device on {port_name}...", request.body.len());

    let mut scratch = vec![0u8; harness_proto::MAX_PAYLOAD_LEN + harness_proto::FRAME_OVERHEAD];
    let written = harness_proto::encode(Command::OriginalPsbt, &request.body, &mut scratch)
        .map_err(|e| anyhow::anyhow!("failed to encode frame: {e:?}"))?;
    transport.0.write_all(&scratch[..written]).context("serial write failed")?;
    transport.0.flush().context("serial flush failed")?;

    println!("Waiting for the device's signed proposal...");

    let mut read_buf = vec![0u8; harness_proto::MAX_PAYLOAD_LEN + harness_proto::FRAME_OVERHEAD];
    let mut filled = 0usize;
    let mut iterations = 0u32;
    let response_bytes = loop {
        match harness_proto::decode(&read_buf[..filled]) {
            Ok((frame, consumed)) => {
                let command = frame.command;
                let payload = frame.payload.to_vec();
                println!(
                    "Decoded a frame: command={command:?}, payload_len={}, consumed={consumed}",
                    payload.len()
                );
                read_buf.copy_within(consumed..filled, 0);
                filled -= consumed;
                if command != Command::SignedPsbt {
                    bail!("expected SignedPsbt from device, got {command:?}");
                }
                break payload;
            }
            Err(harness_proto::DecodeError::Incomplete) => {}
            Err(e) => bail!("framing error: {e:?}"),
        }

        iterations += 1;
        match transport.0.read(&mut read_buf[filled..]) {
            Ok(n) => {
                if n > 0 {
                    println!(
                        "Read {n} bytes (total buffered: {}) after {iterations} read() calls",
                        filled + n
                    );
                }
                filled += n;
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut =>
                if iterations % 20 == 0 {
                    println!(
                        "Still waiting... ({iterations} read() calls so far, 0 bytes buffered)"
                    );
                },
            Err(e) => {
                println!("read() error (not a timeout): {e}");
            }
        }
    };

    let final_psbt = v1_context
        .process_response(&response_bytes)
        .map_err(|e| anyhow::anyhow!("process_response failed: {e:?}"))?;

    println!("Round trip complete!");
    println!("Final PSBT has {} outputs", final_psbt.unsigned_tx.output.len());
    println!("Final PSBT has {} inputs", final_psbt.unsigned_tx.input.len());

    Ok(())
}
