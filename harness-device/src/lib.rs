//! Board-agnostic payjoin sender/receiver logic for the embedded harness.
//!
//! This crate has no knowledge of any specific board (Black Pill, Pico 2,
//! ...). It only knows how to drive the payjoin v1 protocol given a
//! [`Transport`] -- something that can send and receive raw bytes over
//! whatever link the board uses to talk to the host (typically UART).
//! Board-specific bring-up (HAL setup, flashing, the concrete `Transport`
//! implementation) lives in the board-specific firmware repos
//! (payjoin-blackpill-test, payjoin-pico2), not here.
//!
//! This mirrors, byte for byte in terms of payjoin API calls, the in-memory
//! `v1` round trip proven in `rust-payjoin`'s `payjoin/tests/e2e.rs`. The
//! only thing that changes here is where the bytes come from and go: real
//! serial framing instead of a Rust variable.
//!
//! `#![no_std]` is conditional on `not(test)` so `cargo test` can pull in
//! `std` (needed for the threaded round-trip test below, which spins up
//! two real threads to stand in for the two boards). The actual embedded
//! build (`--target thumbv7em-none-eabihf`) always compiles with
//! `cfg(test)` off, so it stays genuinely `no_std`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::str::FromStr;

use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::psbt::Psbt;
use bitcoin::transaction::Version;
use bitcoin::{
    Address, Amount, FeeRate, Network, OutPoint, Sequence, Transaction, TxIn, TxOut, Txid,
    WPubkeyHash, Witness,
};
use harness_proto::{decode, encode, Command};
use payjoin::bitcoin::hashes::{sha256, HashEngine};
use payjoin::directory::ShortId;
use payjoin::receive::v1::{Headers, UncheckedOriginalPayload};
use payjoin::send::v1::SenderBuilder;
use payjoin::PjParam;

/// Something that can send and receive raw bytes to/from the host. The
/// concrete implementation (UART DMA, USB CDC, whatever the board uses)
/// lives in the board firmware, not here.
pub trait Transport {
    type Error;

    /// Write `bytes` to the host. Should block until the write completes.
    fn send(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Read up to `buf.len()` bytes from the host into `buf`, returning the
    /// number of bytes actually read. May return 0 if nothing is available
    /// yet (non-blocking) or block until at least one byte arrives
    /// (blocking) -- either is fine as long as [`recv_frame`] gets called
    /// in a loop by the caller until it has a full frame.
    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
}

/// Errors that can occur while running the harness protocol over a
/// [`Transport`].
#[derive(Debug)]
pub enum HarnessError<E> {
    Transport(E),
    Framing(harness_proto::DecodeError),
    /// The host sent a well-formed frame, but not the command we expected
    /// at this point in the protocol.
    UnexpectedCommand,
    Payjoin(String),
}

/// Read frames from `transport` into `read_buf` until a complete frame is
/// available, then return the command and its payload as an owned copy.
///
/// `filled` tracks how many bytes at the front of `read_buf` are currently
/// valid (may include a full frame, a partial frame, or bytes left over
/// from a previous call). Callers should initialize it to 0 and pass the
/// same `read_buf`/`filled` pair across repeated calls.
///
/// The payload is returned as an owned `Vec<u8>` rather than a
/// `(start, len)` pair borrowed from `read_buf`. That's a deliberate fix,
/// not the cheapest possible design: a borrowed-indices version silently
/// corrupts the payload whenever a second frame is already sitting in the
/// buffer behind the one being decoded. The leftover-compaction
/// `copy_within` has to run before returning (so `filled` is correct for
/// the caller's next call), and a frame's payload -- bytes `[3, 3+len)` --
/// always falls inside `[0, consumed)`, which is exactly the range that
/// compaction overwrites. Copying the payload out before compacting avoids
/// that overlap entirely, at the cost of one allocation per frame --
/// acceptable here since frames are small and this isn't a
/// high-throughput path.
///
/// This is intentionally simple (linear rescans, no ring buffer) since
/// frames are small and this only needs to keep up with a human-scale
/// payjoin round trip, not a high-throughput stream.
pub fn recv_frame<T: Transport>(
    transport: &mut T,
    read_buf: &mut [u8],
    filled: &mut usize,
) -> Result<(Command, Vec<u8>), HarnessError<T::Error>> {
    loop {
        if *filled > 0 {
            match decode(&read_buf[..*filled]) {
                Ok((frame, consumed)) => {
                    let command = frame.command;
                    let payload = frame.payload.to_vec();
                    read_buf.copy_within(consumed..*filled, 0);
                    *filled -= consumed;
                    return Ok((command, payload));
                }
                Err(harness_proto::DecodeError::Incomplete) => {} // fall through to read more
                Err(e) => return Err(HarnessError::Framing(e)),
            }
        }

        if *filled == read_buf.len() {
            return Err(HarnessError::Framing(harness_proto::DecodeError::PayloadTooLarge));
        }

        let n = transport.recv(&mut read_buf[*filled..]).map_err(HarnessError::Transport)?;
        *filled += n;
    }
}

/// Send a single frame over `transport`, using `scratch` as scratch space
/// for encoding. `scratch` must be at least
/// `payload.len() + harness_proto::FRAME_OVERHEAD` bytes.
pub fn send_frame<T: Transport>(
    transport: &mut T,
    command: Command,
    payload: &[u8],
    scratch: &mut [u8],
) -> Result<(), HarnessError<T::Error>> {
    let written = encode(command, payload, scratch)
        .map_err(|_| HarnessError::Payjoin("failed to encode frame".to_string()))?;
    transport.send(&scratch[..written]).map_err(HarnessError::Transport)?;
    Ok(())
}

/// Run the sender role: build a payjoin request from `original_psbt`, hand
/// the request bytes to the host over `transport`, then wait for the host
/// to relay back the receiver's response, and finalize.
///
/// `endpoint` is the payjoin endpoint URL (the `pj=` parameter contents);
/// the host is responsible for actually delivering the request bytes there
/// and bringing back the response -- this function only produces and
/// consumes bytes, it never touches the network itself.
pub fn run_sender<T: Transport>(
    transport: &mut T,
    original_psbt: Psbt,
    endpoint: &str,
    payee: &Address,
    max_additional_fee: Amount,
) -> Result<Psbt, HarnessError<T::Error>> {
    let pj_param = match PjParam::parse(endpoint)
        .map_err(|e| HarnessError::Payjoin(alloc::format!("invalid endpoint: {e:?}")))?
    {
        payjoin::PjParam::V1(v1_param) => v1_param,
        _ => return Err(HarnessError::Payjoin("endpoint is not a v1 payjoin URI".to_string())),
    };

    let sender = SenderBuilder::from_parts(original_psbt, &pj_param, payee, None)
        .build_with_additional_fee(max_additional_fee, Some(0), FeeRate::ZERO, true)
        .map_err(|e| HarnessError::Payjoin(alloc::format!("{e:?}")))?;
    let (request, v1_context) = sender.create_v1_post_request();

    let mut scratch = [0u8; harness_proto::MAX_PAYLOAD_LEN + harness_proto::FRAME_OVERHEAD];
    send_frame(transport, Command::OutRequest, &request.body, &mut scratch)?;

    let mut read_buf = [0u8; harness_proto::MAX_PAYLOAD_LEN + harness_proto::FRAME_OVERHEAD];
    let mut filled = 0usize;
    let (command, response_bytes) = recv_frame(transport, &mut read_buf, &mut filled)?;
    if command != Command::InResponse {
        return Err(HarnessError::UnexpectedCommand);
    }

    let final_psbt = v1_context
        .process_response(&response_bytes)
        .map_err(|e| HarnessError::Payjoin(alloc::format!("{e:?}")))?;

    Ok(final_psbt)
}

/// Minimal [`Headers`] implementation driven by whatever query string and
/// body length the host relayed alongside the request bytes. Real HTTP
/// headers never exist on the wire between host and device -- the host's
/// own transport (real HTTP to the counterparty) already stripped them
/// down to exactly what the receiver-side typestate chain needs.
struct FixedHeaders {
    content_length: String,
}

impl FixedHeaders {
    fn for_body(body: &[u8]) -> Self { Self { content_length: body.len().to_string() } }
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

/// Run the receiver role: wait for the host to relay an incoming request,
/// validate and build a proposal (accepting the original PSBT unmodified,
/// no additional inputs/outputs contributed -- see the note below), sign,
/// and hand the finished proposal back to the host.
///
/// This is intentionally the simplest possible receiver policy so the
/// harness can validate the plumbing end to end. A real hardware-signer
/// receiver would plug in actual UTXO selection and signing here instead
/// of the pass-through closure.
pub fn run_receiver<T: Transport>(
    transport: &mut T,
    query: &str,
    is_receiver_output: impl Fn(&bitcoin::Script) -> bool,
) -> Result<Psbt, HarnessError<T::Error>> {
    let mut read_buf = [0u8; harness_proto::MAX_PAYLOAD_LEN + harness_proto::FRAME_OVERHEAD];
    let mut filled = 0usize;
    let (command, request_body) = recv_frame(transport, &mut read_buf, &mut filled)?;
    if command != Command::OriginalPsbt {
        return Err(HarnessError::UnexpectedCommand);
    }

    let headers = FixedHeaders::for_body(&request_body);
    let unchecked = UncheckedOriginalPayload::from_request(&request_body, query, headers)
        .map_err(|e| HarnessError::Payjoin(alloc::format!("{e:?}")))?;

    let maybe_inputs_owned = unchecked.assume_interactive_receiver();
    let maybe_inputs_seen = maybe_inputs_owned
        .check_inputs_not_owned(&mut |_script| Ok(false))
        .map_err(|e| HarnessError::Payjoin(alloc::format!("{e:?}")))?;
    let outputs_unknown = maybe_inputs_seen
        .check_no_inputs_seen_before(&mut |_outpoint| Ok(false))
        .map_err(|e| HarnessError::Payjoin(alloc::format!("{e:?}")))?;
    let wants_outputs = outputs_unknown
        .identify_receiver_outputs(&mut |script| Ok(is_receiver_output(script)))
        .map_err(|e| HarnessError::Payjoin(alloc::format!("{e:?}")))?;

    // No output substitution, no additional inputs contributed: the
    // simplest possible policy, matching the PR's e2e test. A real
    // hardware-signer receiver would call `.contribute_inputs(..)` here
    // with its own UTXO(s) before `.commit_inputs()`.
    let wants_inputs = wants_outputs.commit_outputs();
    let wants_fee_range = wants_inputs.commit_inputs();
    let provisional_proposal = wants_fee_range
        .apply_fee_range(None, None)
        .map_err(|e| HarnessError::Payjoin(alloc::format!("{e:?}")))?;

    // No inputs of ours were added, so there is nothing new to sign here.
    // A real signer would produce real signatures/witnesses in this
    // closure for any inputs it contributed.
    let payjoin_proposal = provisional_proposal
        .finalize_proposal(|psbt| Ok(psbt.clone()))
        .map_err(|e| HarnessError::Payjoin(alloc::format!("{e:?}")))?;

    let proposal_psbt = payjoin_proposal.psbt().clone();

    // FIX: process_response (called on the sender side, in run_sender)
    // expects base64-encoded text, not raw PSBT bytes. Serializing alone
    // and sending that produced a response the sender could never parse.
    // Found by tracing an actual hardware round trip: the device reported
    // success (send_frame returned Ok) but sender-sim never received a
    // decodable response until this was applied on the wire-encoding side.
    use base64::Engine;
    let response_bytes =
        base64::engine::general_purpose::STANDARD.encode(proposal_psbt.serialize());

    let mut scratch = [0u8; harness_proto::MAX_PAYLOAD_LEN + harness_proto::FRAME_OVERHEAD];
    send_frame(transport, Command::SignedPsbt, response_bytes.as_bytes(), &mut scratch)?;

    Ok(proposal_psbt)
}

/// The `v2` slice of the protocol this crate can actually run standalone
/// on bare-metal: [`ShortId`] round-tripping and SHA256-based mailbox
/// derivation, per `payjoin-blackpill-test`'s `run_payjoin_tests`
/// (proven passing on a real STM32F411CEU6, `thumbv7em-none-eabihf`).
///
/// This deliberately does **not** attempt a live `receive::v2` receiver
/// session. Per that repo's own README: "The full `receive::v2` receiver
/// state machine is gated on `v2-ohttp` (requires `std`). A live receiver
/// session is not possible on bare-metal." `session_context` -- and the
/// OHTTP encapsulate/decapsulate calls it carries through every
/// transition, all the way to `finalize_proposal` -- has no `no_std` path
/// today. Anything claiming to run a full v2 receiver on-device would be
/// wrong; this only exercises the primitive that is genuinely no_std-safe.
pub fn shortid_of(seed: &[u8]) -> ShortId {
    let mut engine = sha256::HashEngine::default();
    engine.input(seed);
    let hash = sha256::Hash::from_engine(engine);
    ShortId::from(hash)
}

/// Run the v2 probe role: wait for the host to relay seed bytes (reusing
/// [`Command::OriginalPsbt`] -- there's no v2-specific opcode yet, this
/// isn't really "the original PSBT", just whatever the host wants hashed),
/// compute the [`ShortId`] the same way `payjoin-blackpill-test` does, and
/// send the bech32m-encoded id back over [`Command::SignedPsbt`].
///
/// This exists so the harness can validate the same primitive on real
/// hardware through the serial framing, rather than only as a bare
/// `main()` with no host round trip at all (which is all
/// `payjoin-blackpill-test` does today).
pub fn run_v2_probe<T: Transport>(transport: &mut T) -> Result<String, HarnessError<T::Error>> {
    let mut read_buf = [0u8; harness_proto::MAX_PAYLOAD_LEN + harness_proto::FRAME_OVERHEAD];
    let mut filled = 0usize;
    let (command, seed) = recv_frame(transport, &mut read_buf, &mut filled)?;
    if command != Command::OriginalPsbt {
        return Err(HarnessError::UnexpectedCommand);
    }

    let id = shortid_of(&seed);
    let encoded = alloc::format!("{id}");

    let mut scratch = [0u8; harness_proto::MAX_PAYLOAD_LEN + harness_proto::FRAME_OVERHEAD];
    send_frame(transport, Command::SignedPsbt, encoded.as_bytes(), &mut scratch)?;

    Ok(encoded)
}

/// A minimal, self-contained original PSBT: one input (with witness_utxo
/// so BIP78 checks pass) funding two outputs, the second of which is the
/// receiver's own address -- same shape as payjoin/tests/e2e.rs's
/// fixture, built locally instead of pulling `payjoin-test-utils` (which
/// drags in testcontainers-modules/tokio and a transitive dependency
/// tree that outran rustc 1.85).
///
/// Public (not test-only) so both this crate's own tests and any
/// external tool driving `run_sender`/`run_receiver` against a real
/// device over a real transport (see e.g. sender-sim) can use the exact
/// same fixture without duplicating it.
pub fn original_psbt_fixture() -> (Psbt, Address) {
    let sender_script = bitcoin::ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([0x11; 20]));
    let receiver_script = bitcoin::ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([0x22; 20]));
    let receiver_address = Address::from_script(&receiver_script, Network::Testnet)
        .expect("p2wpkh script should map to a valid address");

    let tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint { txid: Txid::from_str(&"11".repeat(32)).unwrap(), vout: 0 },
            script_sig: bitcoin::ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![
            TxOut { value: Amount::from_sat(90_000), script_pubkey: sender_script.clone() },
            TxOut { value: Amount::from_sat(50_000), script_pubkey: receiver_script },
        ],
    };

    let mut psbt = Psbt::from_unsigned_tx(tx).unwrap();
    psbt.inputs[0].witness_utxo =
        Some(TxOut { value: Amount::from_sat(150_000), script_pubkey: sender_script });

    (psbt, receiver_address)
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;
    use core::convert::Infallible;

    use super::*;

    // ------------------------------------------------------------------
    // Framing-only tests: no payjoin/bitcoin involved, just recv_frame /
    // send_frame against a scripted in-memory Transport.
    // ------------------------------------------------------------------

    /// A transport whose `recv` hands back bytes from a preloaded queue,
    /// at most `chunk_size` bytes per call, and records everything passed
    /// to `send`.
    struct QueueTransport {
        to_read: VecDeque<u8>,
        chunk_size: usize,
        written: Vec<u8>,
    }

    impl QueueTransport {
        fn preloaded(bytes: &[u8], chunk_size: usize) -> Self {
            Self { to_read: bytes.iter().copied().collect(), chunk_size, written: Vec::new() }
        }
    }

    impl Transport for QueueTransport {
        type Error = Infallible;

        fn send(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
            self.written.extend_from_slice(bytes);
            Ok(())
        }

        fn recv(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            let n = buf.len().min(self.chunk_size).min(self.to_read.len());
            for slot in buf.iter_mut().take(n) {
                *slot = self.to_read.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    fn encode_frame(command: Command, payload: &[u8]) -> Vec<u8> {
        let mut scratch = alloc::vec![0u8; payload.len() + harness_proto::FRAME_OVERHEAD];
        let written = encode(command, payload, &mut scratch).unwrap();
        scratch[..written].to_vec()
    }

    #[test]
    fn recv_frame_reassembles_across_short_reads() {
        let bytes = encode_frame(Command::OriginalPsbt, b"hello device");
        let mut transport = QueueTransport::preloaded(&bytes, 1); // 1 byte per recv() call

        let mut read_buf = [0u8; 64];
        let mut filled = 0usize;
        let (command, payload) = recv_frame(&mut transport, &mut read_buf, &mut filled).unwrap();
        assert_eq!(command, Command::OriginalPsbt);
        assert_eq!(payload, b"hello device");
    }

    /// Regression test for a real corruption bug found while writing these
    /// tests: the original `recv_frame` returned `(start, len)` indices
    /// into `read_buf`, but performed the leftover-compaction
    /// `copy_within` *before* the caller had a chance to read the payload
    /// those indices pointed at. A frame's payload range always overlaps
    /// the range compaction overwrites, so whenever a second frame was
    /// already buffered behind the first, the tail of the "returned"
    /// payload got silently stomped by the next frame's leading bytes.
    /// Returning an owned `Vec<u8>` copied out before compacting fixes it.
    #[test]
    fn recv_frame_does_not_corrupt_payload_when_a_second_frame_is_already_buffered() {
        let mut both = encode_frame(Command::OutRequest, b"first");
        both.extend(encode_frame(Command::Status, &[0x00]));
        // chunk_size larger than both frames combined: a single recv()
        // call hands back everything at once, the way a real serial port
        // can if the OS buffered several frames before we read.
        let mut transport = QueueTransport::preloaded(&both, 1024);

        let mut read_buf = [0u8; 64];
        let mut filled = 0usize;

        let (command, payload) = recv_frame(&mut transport, &mut read_buf, &mut filled).unwrap();
        assert_eq!(command, Command::OutRequest);
        assert_eq!(payload, b"first");

        let (command, _) = recv_frame(&mut transport, &mut read_buf, &mut filled).unwrap();
        assert_eq!(command, Command::Status);
    }

    #[test]
    fn recv_frame_reports_payload_too_large_when_buffer_fills_without_a_frame() {
        // A read_buf too small to ever hold a complete frame: recv_frame
        // must bail instead of looping forever once filled == buf.len().
        let mut transport = QueueTransport::preloaded(&[0xAA; 16], 16);
        let mut read_buf = [0u8; 4];
        let mut filled = 0usize;

        let err = recv_frame(&mut transport, &mut read_buf, &mut filled).unwrap_err();
        assert!(matches!(err, HarnessError::Framing(harness_proto::DecodeError::PayloadTooLarge)));
    }

    #[test]
    fn send_frame_writes_a_well_formed_frame() {
        let mut transport = QueueTransport::preloaded(&[], 0);
        let mut scratch = [0u8; 64];
        send_frame(&mut transport, Command::SignedPsbt, b"proposal bytes", &mut scratch).unwrap();

        let (frame, consumed) = decode(&transport.written).unwrap();
        assert_eq!(consumed, transport.written.len());
        assert_eq!(frame.command, Command::SignedPsbt);
        assert_eq!(frame.payload, b"proposal bytes");
    }

    #[test]
    fn v2_shortid_round_trip_over_transport() {
        // Same seed and assertions as payjoin-blackpill-test's
        // run_payjoin_tests, just driven over the harness framing instead
        // of called directly from main().
        let seed = b"payjoin-blackpill-test";
        let host_frame = encode_frame(Command::OriginalPsbt, seed);
        let mut transport = QueueTransport::preloaded(&host_frame, 1024);

        let encoded = run_v2_probe(&mut transport).unwrap();

        let expected = shortid_of(seed);
        assert_eq!(encoded, alloc::format!("{expected}"));

        // Round-trip through Display/FromStr, exactly like the hardware
        // test's round_trip_ok check.
        let decoded: ShortId = encoded.parse().expect("encoded id should parse back");
        assert_eq!(decoded, expected);

        // And confirm what actually went out over the wire matches.
        let (frame, _) = decode(&transport.written).unwrap();
        assert_eq!(frame.command, Command::SignedPsbt);
        assert_eq!(frame.payload, encoded.as_bytes());
    }

    #[test]
    fn v2_mailbox_id_is_nonempty() {
        // Mirrors the hardware test's second check: a different seed
        // (standing in for an OHTTP-keys hash) still produces a
        // non-empty encoded mailbox id.
        let id = shortid_of(b"ohttp-keys-hash");
        assert!(!alloc::format!("{id}").is_empty());
    }

    #[test]
    fn v2_probe_rejects_wrong_command() {
        let host_frame = encode_frame(Command::Status, &[0x00]);
        let mut transport = QueueTransport::preloaded(&host_frame, 1024);

        let err = run_v2_probe(&mut transport).unwrap_err();
        assert!(matches!(err, HarnessError::UnexpectedCommand));
    }

    // ------------------------------------------------------------------
    // Full round-trip test: real payjoin crypto, two real threads (one
    // per role) connected by a channel-backed Transport, standing in for
    // the two boards. Fixtures mirror payjoin/tests/e2e.rs's `mod v1`
    // test exactly, so the numbers are known to be internally consistent.
    // ------------------------------------------------------------------

    use std::sync::mpsc;
    use std::thread;

    /// Splits a full request URL into its query string, the way a device
    /// would after receiving `Request.url` from the host relay.
    fn query_of(url: &str) -> &str { url.split('?').nth(1).unwrap_or("") }

    /// Duplex byte transport backed by a pair of `mpsc` channels. `send`
    /// pushes one chunk; `recv` drains the oldest pending chunk into
    /// `buf`, splitting it across multiple `recv` calls if `buf` is
    /// smaller than the chunk.
    ///
    /// Also translates the command tag on send, exactly like
    /// harness-host's `run_v1_roundtrip` does when relaying between two
    /// real boards: `run_sender` emits `OutRequest`, but `run_receiver`
    /// only accepts `OriginalPsbt`; `run_receiver` responds with
    /// `SignedPsbt`, but `run_sender` only accepts `InResponse`. Without
    /// this, wiring `run_sender` and `run_receiver` directly together (no
    /// host in between) always fails with `UnexpectedCommand` -- this is
    /// exactly the bug `sender-sim` had before the same fix was applied
    /// there.
    struct ChannelTransport {
        tx: mpsc::Sender<Vec<u8>>,
        rx: mpsc::Receiver<Vec<u8>>,
        pending: Vec<u8>,
        translate_outgoing: fn(Command) -> Command,
    }

    impl Transport for ChannelTransport {
        type Error = String;

        fn send(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
            let (frame, _) = decode(bytes).map_err(|e| alloc::format!("{e:?}"))?;
            let translated_command = (self.translate_outgoing)(frame.command);
            let payload = frame.payload.to_vec();

            let mut scratch = vec![0u8; payload.len() + harness_proto::FRAME_OVERHEAD];
            let written = encode(translated_command, &payload, &mut scratch)
                .map_err(|e| alloc::format!("{e:?}"))?;

            self.tx.send(scratch[..written].to_vec()).map_err(|e| e.to_string())
        }

        fn recv(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            if self.pending.is_empty() {
                self.pending = self.rx.recv().map_err(|e| e.to_string())?;
            }
            let n = buf.len().min(self.pending.len());
            buf[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            Ok(n)
        }
    }

    fn channel_pair() -> (ChannelTransport, ChannelTransport) {
        let (tx_a, rx_a) = mpsc::channel();
        let (tx_b, rx_b) = mpsc::channel();
        (
            ChannelTransport {
                tx: tx_a,
                rx: rx_b,
                pending: Vec::new(),
                // sender side: OutRequest -> OriginalPsbt on the way out
                translate_outgoing: |c| {
                    if c == Command::OutRequest {
                        Command::OriginalPsbt
                    } else {
                        c
                    }
                },
            },
            ChannelTransport {
                tx: tx_b,
                rx: rx_a,
                pending: Vec::new(),
                // receiver side: SignedPsbt -> InResponse on the way out
                translate_outgoing: |c| {
                    if c == Command::SignedPsbt {
                        Command::InResponse
                    } else {
                        c
                    }
                },
            },
        )
    }

    #[test]
    fn v1_round_trip_over_real_transport() {
        let (original_psbt, receiver_address) = original_psbt_fixture();
        let receiver_script = original_psbt.unsigned_tx.output[1].script_pubkey.clone();

        let pj_param = match PjParam::parse("https://example.com/").unwrap() {
            payjoin::PjParam::V1(v1_param) => v1_param,
            _ => panic!("expected a v1 PjParam"),
        };

        // The receiver device needs the query string up front (it's
        // configuration the host would supply, not something learned
        // from the wire in this protocol). Since it's derived
        // deterministically from the same PjParam the sender uses, this
        // reruns the same request-building step run_sender does
        // internally just to read it off -- not a guess, the exact code
        // path proven in e2e.rs's `mod v1` test.
        let probe_sender =
            SenderBuilder::from_parts(original_psbt.clone(), &pj_param, &receiver_address, None)
                .build_with_additional_fee(Amount::from_sat(182), Some(0), FeeRate::ZERO, true)
                .unwrap();
        let (probe_request, _) = probe_sender.create_v1_post_request();
        let query = query_of(&probe_request.url).to_string();

        let (sender_transport, receiver_transport) = channel_pair();

        let sender_handle = thread::spawn({
            let original_psbt = original_psbt.clone();
            let receiver_address = receiver_address.clone();
            let mut sender_transport = sender_transport;
            move || {
                run_sender(
                    &mut sender_transport,
                    original_psbt,
                    "https://example.com/",
                    &receiver_address,
                    Amount::from_sat(182),
                )
            }
        });

        let receiver_handle = thread::spawn({
            let receiver_script = receiver_script.clone();
            let mut receiver_transport = receiver_transport;
            move || {
                run_receiver(&mut receiver_transport, &query, move |script: &bitcoin::Script| {
                    script == receiver_script.as_script()
                })
            }
        });

        let proposal_psbt =
            receiver_handle.join().expect("receiver thread panicked").expect("run_receiver failed");
        let final_psbt =
            sender_handle.join().expect("sender thread panicked").expect("run_sender failed");

        assert_eq!(final_psbt, proposal_psbt);
        assert!(final_psbt.unsigned_tx.output.len() >= 2);
    }
}
