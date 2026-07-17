# payjoin-no-std-harness

Embedded test harness proving that [rust-payjoin](https://github.com/payjoin/rust-payjoin)'s
`no_std` support (see [PR #1615](https://github.com/payjoin/rust-payjoin/pull/1615))
actually runs a real payjoin round trip between two boards, not just
compiles.

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

The host never makes a payjoin decision. It only knows how to open two
serial ports and relay length-prefixed frames between them. All the
protocol logic -- building requests, validating proposals, signing -- runs
on the boards, using the same `payjoin` API calls already proven in
`rust-payjoin`'s `payjoin/tests/e2e.rs` (v1 module).

## Crates

- **`harness-proto`**: the wire format shared by host and device (`no_std`,
  no `alloc`). `[command][len][payload][crc16]`. See the module docs in
  `harness-proto/src/lib.rs` for the exact format.
- **`harness-device`**: board-agnostic `no_std` payjoin logic (`alloc`,
  `v1`, `v2` features of `payjoin`). Defines a `Transport` trait that the
  board firmware implements over UART/USB CDC; this crate has no idea
  what board it's running on.
  - v1: full sender/receiver round trip, mirroring the proven `e2e.rs`
    flow.
  - v2: **not** a live receiver session. `receive::v2`'s full state
    machine is gated on `v2-ohttp` (`std`), which has no `no_std` path
    today -- confirmed by
    [payjoin-blackpill-test](https://github.com/caarloshenriq/payjoin-blackpill-test)'s
    own README. What's here is the slice that's genuinely `no_std`-safe
    and already proven on real hardware: `ShortId` round-tripping and
    SHA256-based mailbox derivation, driven over the harness framing
    instead of a bare `main()`.
- **`harness-host`**: a normal `std` binary that opens two serial ports and
  relays frames between them (see Architecture above). Split into a
  testable `lib.rs` (`Args`, `FramedPort`, `run_v1_roundtrip`) and a thin
  `src/bin` binary that only opens the real ports.

`harness-device` is intentionally **not** a member of the top-level
workspace -- it depends on `payjoin` (git, `feat/payjoin-nostd` branch),
which needs a newer Cargo resolver than the rest of this workspace uses.
Keeping it separate means a plain `cargo build` at the repo root (for
`harness-proto`/`harness-host`) doesn't need to deal with any of that.

## Status

- [x] `harness-proto`: implemented, tested, passing.
- [x] `harness-device`: sender/receiver logic (v1) and `ShortId`/mailbox
      probe (v2) implemented and tested -- `cargo test` passes (9 tests,
      including a real two-thread v1 round trip with actual payjoin
      crypto). Also confirmed building for the real target:
      `cargo build --release --target thumbv7em-none-eabihf
    -Zbuild-std=core,alloc` succeeds.
- [x] `harness-host`: orchestration written and tested against in-memory
      doubles (`FramedPort`, `run_v1_roundtrip`). **Not yet run against
      real serial ports** -- see the gap below.
- [ ] **Board firmware implementing `Transport`.** This is the actual
      remaining gap: no board currently exposes a serial port at all.
      [payjoin-blackpill-test](https://github.com/caarloshenriq/payjoin-blackpill-test)
      only proves the `ShortId`/mailbox primitives compile and run on
      hardware (LED-only output, no host communication); it does not yet
      implement USB CDC ACM or wire `harness-device`'s `Transport` trait
      to anything. Same gap on
      [payjoin-pico2](https://github.com/benalleng/payjoin-pico2). Until
      one of these exists, `harness-host` has nothing to talk to.
- [ ] Phase 2: fund real wallets on regtest instead of using a fixture PSBT,
      and assert the resulting transaction actually confirms.

## Building

```sh
# Host-side pieces (this machine)
nix develop           # nightly toolchain + pkg-config/udev for serialport
cd harness-proto && cargo test
cd ../harness-host && cargo test

# Device-side logic, host-target tests (no hardware needed)
cd ../harness-device && cargo test

# Device-side logic, cross-compiled for real hardware
nix develop .#embedded
cd harness-device
cargo build --release --target thumbv7em-none-eabihf -Zbuild-std=core,alloc \
  --no-default-features --features alloc,v1,v2
```

`harness-device/Cargo.toml` points `payjoin` at this fork's
`feat/payjoin-nostd` branch via git. Once the `no_std` work is merged and
released upstream, switch that to a real version dependency.

## Running Phase 1

Blocked on the board firmware gap above: flash the sender role onto one
board and the receiver role onto the other (once a board repo implements
`Transport` over USB CDC/UART and calls into
`harness-device::run_sender` / `run_receiver`), wire both up over USB,
then:

```sh
cargo run -p harness-host -- \
  --sender-port /dev/ttyACM0 \
  --receiver-port /dev/ttyACM1
```
