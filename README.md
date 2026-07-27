# payjoin-no-std-harness

Embedded test harness proving that [rust-payjoin](https://github.com/payjoin/rust-payjoin)'s
`no_std` support (see [PR #1615](https://github.com/payjoin/rust-payjoin/pull/1615))
actually runs a real payjoin round trip on real hardware, not just compiles.

## Status (updated after real hardware validation)

**v1 (BIP78): confirmed working on real hardware, both roles.**

- **Receiver role**: a WeAct STM32F411CEU6 Black Pill, running
  `harness_device::run_receiver` over real USB CDC ACM, received a real
  request, ran the full BIP78 validation chain, and returned a signed
  proposal -- confirmed via `usbmon` packet capture, not just a status
  LED. Tested against `sender-sim`.
- **Sender role**: the same board, different firmware, running
  `harness_device::run_sender`, built a real request and processed a
  real response. Tested against `receiver-sim`.
- Two real bugs were found and fixed getting here (now fixed in
  `harness-device`/`harness-host` themselves, not just in test tooling):
  - `run_receiver` was sending raw serialized PSBT bytes; the sender
    side's `process_response` expects base64-encoded text.
  - Wiring `run_sender`/`run_receiver` directly together (no host in
    between) needs a command translation layer -- `run_sender` emits
    `OutRequest`, `run_receiver` only accepts `OriginalPsbt` (and
    `SignedPsbt`/`InResponse` on the way back). `harness-host`'s
    `run_v1_roundtrip` already does this correctly for two real boards;
    `sender-sim`/`receiver-sim` (and `harness-device`'s own
    `v1_round_trip_over_real_transport` test) needed the same fix.

**v2 probe (`ShortId`/mailbox derivation): confirmed working on real
hardware, through the real orchestrator.**

- `harness_device::run_v2_probe`, run on the same board over real USB
  CDC, tested against `harness-host --mode v2-probe` (not a throwaway
  tool -- the actual code that ships in the harness). Sent a seed, got
  back a well-formed bech32m `ShortId`.
- This is **not** a live `receive::v2` receiver session -- see
  `harness_device::run_v2_probe`'s own docs for why that's not possible
  on bare-metal today (`v2-ohttp` requires `std`, confirmed by
  [payjoin-blackpill-test](https://github.com/caarloshenriq/payjoin-blackpill-test)'s
  own earlier findings). This is the one v2 primitive that genuinely is
  `no_std`-safe.

**What's still open:**

- **Two physical boards running simultaneously**, with `harness-host`
  relaying between them for real, has not happened yet -- only one board
  is currently available. Each role (v1 sender, v1 receiver, v2 probe)
  has been proven individually against a faithful simulation of its
  counterpart (`sender-sim`, `receiver-sim`, or `harness-host` itself
  for the v2 probe), which exercises the same protocol logic, but isn't
  literally two boards talking through the real host relay at the same
  time. Getting a second board (or coordinating with
  [payjoin-pico2](https://github.com/benalleng/payjoin-pico2)) would
  close this.
- Phase 2 (regtest wallets instead of a fixture PSBT) -- always been
  future scope, unrelated to the hardware work above.

## Architecture

```
┌──────────────────┐        serial        ┌──────────────────────┐
│   harness-host     │◄──────────────────►│  Board A (no_std)      │
│   (this PC)          │                     │  harness-device        │
│                       │                     │  role: sender           │
│  - orchestrates       │                     └──────────────────────┘
│  - relays raw bytes    │
│  - no payjoin logic     │        serial        ┌──────────────────────┐
│                       │◄──────────────────►│  Board B (no_std)      │
└──────────────────┘                     │  harness-device        │
                                            │  role: receiver         │
                                            └──────────────────────┘
```

The host never makes a payjoin decision. It only knows how to open serial
ports and relay length-prefixed frames between them, translating command
tags where needed (see the v1 bug notes above). All the protocol logic --
building requests, validating proposals, signing -- runs on the boards,
using the same `payjoin` API calls already proven in `rust-payjoin`'s
`payjoin/tests/e2e.rs` (v1 module).

Until a second board is available, `sender-sim` and `receiver-sim` (in
this repo) stand in for the missing board, running the exact same
`harness_device::run_sender`/`run_receiver` functions on the host instead
-- useful for validating the payjoin logic itself, though not a
substitute for two real boards talking through the real host relay.

## Crates

- **`harness-proto`**: the wire format shared by host and device (`no_std`,
  no `alloc`). `[command][len][payload][crc16]`. See the module docs in
  `harness-proto/src/lib.rs` for the exact format.
- **`harness-device`**: board-agnostic `no_std` payjoin logic (`alloc`,
  `v1`, `v2` features of `payjoin`). Defines a `Transport` trait that the
  board firmware implements over UART/USB CDC; this crate has no idea
  what board it's running on. Confirmed correct on real hardware, both
  v1 roles and the v2 probe (see Status above).
- **`harness-host`**: a normal `std` binary. Two modes:
  - `--mode v1` (default): relays frames between two boards, one sender
    one receiver, translating `OutRequest`→`OriginalPsbt` and
    `SignedPsbt`→`InResponse` along the way.
  - `--mode v2-probe`: talks to a single board, sends it seed bytes,
    reports back the `ShortId` it computed. Confirmed working against
    real hardware.
- **`sender-sim`** / **`receiver-sim`**: standalone host tools that play
  the sender/receiver role directly (not through `harness-host`'s
  two-board relay), for validating one board's logic against a faithful
  simulation of its counterpart when only one physical board is
  available. Each depends on `payjoin` directly (unlike `harness-host`,
  which stays deliberately protocol-blind).

`harness-device`, `sender-sim`, and `receiver-sim` are intentionally
**not** members of the top-level workspace (see the root `Cargo.toml`'s
`exclude`) -- they depend on `payjoin` (git, `feat/payjoin-nostd`
branch), which needs a newer Cargo resolver than the rest of this
workspace uses. Keeping them separate means a plain `cargo build` at the
repo root (for `harness-proto`/`harness-host`) doesn't need to deal with
any of that.

## Board firmware

Board-specific bring-up (HAL setup, flashing, the concrete `Transport`
implementation) lives in board-specific firmware repos, not here:

- [payjoin-blackpill-test](https://github.com/caarloshenriq/payjoin-blackpill-test)
  (WeAct STM32F411CEU6 Black Pill): `usb-harness` (v1 receiver),
  `usb-harness-sender` (v1 sender), `usb-harness-v2` (v2 probe), plus
  `usb-echo` and `led-blink` as bring-up diagnostics. Flash via SWD
  (ST-Link + `probe-rs`) -- DFU was unreliable for this board/cable/
  adapter combination in practice.
- [payjoin-pico2](https://github.com/benalleng/payjoin-pico2) (RP2350):
  proves the `ShortId`/mailbox primitive compiles and runs on Cortex-M33
  hardware, printed over a real serial console.

## Building

```sh
nix develop                     # nightly + pkg-config/udev for serialport
cd harness-proto && cargo test
cd ../harness-host && cargo test
cd ../harness-device && cargo test   # separate workspace root, see above
cd ../sender-sim && cargo build
cd ../receiver-sim && cargo build

# Cross-compiled for real hardware
nix develop .#embedded
cd harness-device
cargo build --release --target thumbv7em-none-eabihf -Zbuild-std=core,alloc
```

`harness-device`, `sender-sim`, and `receiver-sim`'s `Cargo.toml` point
`payjoin` at this fork's `feat/payjoin-nostd` branch via git. Once the
`no_std` work is merged and released upstream, switch that to a real
version dependency.

## Running against real hardware

Flash the appropriate firmware from
[payjoin-blackpill-test](https://github.com/caarloshenriq/payjoin-blackpill-test)
(`usb-harness`, `usb-harness-sender`, or `usb-harness-v2`) via SWD, then:

```sh
# v1 receiver on the board, sender-sim on the host
cd sender-sim && sudo -E cargo run -- /dev/ttyACM0

# v1 sender on the board, receiver-sim on the host
cd receiver-sim && sudo -E cargo run -- /dev/ttyACM0

# v2 probe on the board, through the real harness-host
cargo run -p harness-host -- --mode v2-probe \
  --device-port /dev/ttyACM0 --seed "some seed bytes"

# v1, two boards (once a second board is available)
cargo run -p harness-host -- --mode v1 \
  --sender-port /dev/ttyACM0 --receiver-port /dev/ttyACM1
```

(`sudo -E` is only needed until your user is in the right group for
serial port access -- e.g. `dialout` on most distros.)
