//! Standalone tool: plays the payjoin v1 receiver role for real, over a
//! real serial port, against a real sender board running
//! usb-harness-sender/`harness_device::run_sender`.
//!
//! Mirrors sender-sim, role reversed. Exists for the same reason: only
//! one physical board is available right now, so the sender role runs on
//! that board (usb-harness-sender) and this tool plays the missing
//! receiver side on the host.
//!
//! Same vocabulary note as sender-sim: this does NOT call
//! harness_device::run_receiver directly, because run_receiver expects
//! Command::OriginalPsbt/responds with Command::SignedPsbt -- the
//! commands harness-host would tag them as *after* translating from a
//! real sender board's OutRequest/InResponse. Since this tool talks
//! directly to the sender board with no host in between, it speaks the
//! sender's vocabulary directly (expect OutRequest, respond
//! InResponse) instead of routing through run_receiver's
//! OriginalPsbt/SignedPsbt vocabulary and a translation shim on top.
//!
//! The actual BIP78 validation chain below is copy-identical to
//! harness_device::run_receiver's body (including the same base64
//! response encoding fix) -- just re-tagged on the wire.
//!
//! Usage:
//!   receiver-sim /dev/ttyACM0

use std::io::{Read, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use harness_device::original_psbt_fixture;
use harness_proto::Command;
use payjoin::receive::v1::{Headers, UncheckedOriginalPayload};

struct FixedHeaders {
    content_length: String,
}

impl Headers for FixedHeaders {
    fn get_header(&self, key: &str) -> Option<&str> {
        match key {
            "content-length" => Some(&self.content_length),
            "content-type" => Some("text/plain"),
            _ => None,
        }
    }
}

fn main() -> Result<()> {
    let port_name = std::env::args().nth(1).context("usage: receiver-sim <device-port>")?;

    let mut port = serialport::new(&port_name, 115_200)
        .timeout(Duration::from_millis(500))
        .open()
        .with_context(|| format!("failed to open {port_name}"))?;

    // Same fixture sender-sim/usb-harness-sender use, so the receiver
    // script this tool looks for matches what the sender board actually
    // paid.
    let (_, receiver_address) = original_psbt_fixture();
    let receiver_script = receiver_address.script_pubkey();

    println!("Waiting for the device's request on {port_name}...");

    let mut read_buf = vec![0u8; harness_proto::MAX_PAYLOAD_LEN + harness_proto::FRAME_OVERHEAD];
    let mut filled = 0usize;
    let mut iterations = 0u32;
    let request_body = loop {
        match harness_proto::decode(&read_buf[..filled]) {
            Ok((frame, consumed)) => {
                let command = frame.command;
                let payload = frame.payload.to_vec();
                read_buf.copy_within(consumed..filled, 0);
                filled -= consumed;
                if command != Command::OutRequest {
                    bail!("expected OutRequest from device, got {command:?}");
                }
                break payload;
            }
            Err(harness_proto::DecodeError::Incomplete) => {}
            Err(e) => bail!("framing error: {e:?}"),
        }

        iterations += 1;
        match port.read(&mut read_buf[filled..]) {
            Ok(n) => {
                if n > 0 {
                    println!(
                        "Read {n} bytes (total buffered: {}) after {iterations} read() calls",
                        filled + n
                    );
                    let hex: String = read_buf[filled..filled + n]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!("Raw bytes: {hex}");
                }
                filled += n;
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut =>
                if iterations % 20 == 0 {
                    println!("Still waiting... ({iterations} read() calls so far)");
                },
            Err(e) => println!("read() error (not a timeout): {e}"),
        }
    };

    println!("Got {} byte request, processing...", request_body.len());

    let headers = FixedHeaders { content_length: request_body.len().to_string() };
    let unchecked = UncheckedOriginalPayload::from_request(&request_body, "", headers)
        .map_err(|e| anyhow::anyhow!("from_request failed: {e:?}"))?;

    let maybe_inputs_owned = unchecked.assume_interactive_receiver();
    let maybe_inputs_seen = maybe_inputs_owned
        .check_inputs_not_owned(&mut |_script| Ok(false))
        .map_err(|e| anyhow::anyhow!("check_inputs_not_owned failed: {e:?}"))?;
    let outputs_unknown = maybe_inputs_seen
        .check_no_inputs_seen_before(&mut |_outpoint| Ok(false))
        .map_err(|e| anyhow::anyhow!("check_no_inputs_seen_before failed: {e:?}"))?;
    let wants_outputs = outputs_unknown
        .identify_receiver_outputs(&mut |script| Ok(script == receiver_script.as_script()))
        .map_err(|e| anyhow::anyhow!("identify_receiver_outputs failed: {e:?}"))?;

    let wants_inputs = wants_outputs.commit_outputs();
    let wants_fee_range = wants_inputs.commit_inputs();
    let provisional_proposal = wants_fee_range
        .apply_fee_range(None, None)
        .map_err(|e| anyhow::anyhow!("apply_fee_range failed: {e:?}"))?;

    let payjoin_proposal = provisional_proposal
        .finalize_proposal(|psbt| Ok(psbt.clone()))
        .map_err(|e| anyhow::anyhow!("finalize_proposal failed: {e:?}"))?;

    let proposal_psbt = payjoin_proposal.psbt().clone();
    println!(
        "Proposal built: {} inputs, {} outputs",
        proposal_psbt.unsigned_tx.input.len(),
        proposal_psbt.unsigned_tx.output.len()
    );

    use base64::Engine;
    let response_text = base64::engine::general_purpose::STANDARD.encode(proposal_psbt.serialize());

    let mut scratch = vec![0u8; harness_proto::MAX_PAYLOAD_LEN + harness_proto::FRAME_OVERHEAD];
    let written =
        harness_proto::encode(Command::InResponse, response_text.as_bytes(), &mut scratch)
            .map_err(|e| anyhow::anyhow!("failed to encode response frame: {e:?}"))?;
    port.write_all(&scratch[..written]).context("serial write failed")?;
    port.flush().context("serial flush failed")?;

    println!("Sent InResponse ({} bytes). Round trip complete!", response_text.len());

    Ok(())
}
