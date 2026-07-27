# Adding a new board

This is for anyone (including future-you) wiring `harness-device` into a
new board's firmware -- what
[payjoin-blackpill-test](https://github.com/caarloshenriq/payjoin-blackpill-test)
did for the WeAct STM32F411 Black Pill, and what
[payjoin-pico2](https://github.com/benalleng/payjoin-pico2) would need to
do next for the RP2350.

`harness-device` is deliberately board-agnostic: it only needs something
that implements the `Transport` trait. Everything below is about getting
from "I have a board" to "I have a `Transport` impl I trust", plus the
specific bugs that cost real time getting there the first time.

## The `Transport` contract

```rust
pub trait Transport {
    type Error;
    fn send(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;
    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
}
```

- **`send` must block until the write actually completes.** Not "queued
  into a driver buffer" -- actually gone out over the wire. See the
  "queued isn't sent" pitfall below; this is the single bug most likely
  to bite you.
- **`recv` can return 0** if nothing is available yet (non-blocking
  poll-and-return) or block until at least one byte arrives -- either is
  fine. `recv_frame` (in `harness-device`) calls `recv` in a loop until
  it has a complete frame, so it doesn't care which style you use, as
  long as returning 0 doesn't mean "connection closed" (unlike a Unix
  socket convention -- here it means "try again").
- **No assumption about chunk boundaries.** A single `recv` call might
  return one byte or the whole frame at once, depending on your
  transport's buffering. `harness-proto`'s framing handles reassembly;
  your `Transport` impl doesn't need to.

## Two-stage bring-up

Don't wire `harness-device` in on day one. Two stages, in order, each
fully working before moving to the next:

### Stage A: prove the raw transport works, with zero payjoin involved

For USB CDC specifically: get the board enumerating as a serial port and
echoing bytes back, with no `harness-device`, no `payjoin`, nothing but
the USB stack. This isolates "is my USB bring-up right" from "is my
Transport wiring right" -- when something breaks later, you'll know
which half to suspect.

Test it with a plain serial terminal (`picocom`, `screen`) typing bytes
by hand before writing any automated test tooling. This is fast and
directly confirms the physical link is real.

### Stage B: wire `Transport` into `harness-device`, one function at a
time

Once Stage A works, implement `Transport` for your board's transport,
and call `harness_device::run_receiver`, `run_sender`, or `run_v2_probe`
directly -- don't reimplement any of that logic in your firmware. Test
against `sender-sim` / `receiver-sim` (in this repo) if you only have
one board, since they play the missing counterpart role on the host.

## Pitfalls found the hard way

These cost real debugging time on the Black Pill. They're general enough
to bite any board using a similar (polled, non-interrupt) USB CDC stack.

### "Queued" isn't "sent"

If your USB stack is polled rather than interrupt-driven, writing into
the serial driver's internal buffer does **not** guarantee the bytes
have gone out over the wire yet. The real transfer to the host completes
across subsequent poll cycles. If your firmware stops servicing the USB
peripheral right after a "successful" write (e.g. because it's about to
halt, sleep, or move on to other work), the last chunk of data can get
silently stuck in the buffer forever -- the device thinks it succeeded,
the host never receives anything.

Fix: keep polling the USB peripheral for a while (or forever, if that's
the last thing your firmware does) after any write, not just up to the
point where the write call returns `Ok`.

### The initiating side can race the host's port-open

If your firmware's role writes *first* (the sender role does: it builds
a request and sends it without waiting to be asked), it can start
writing as soon as USB enumerates -- independent of whether the
host-side application has actually opened the port yet. The OS driver
accepts the bulk transfer at the kernel level regardless of whether
anyone's reading, but the first byte(s) can get lost once the host
application does finally open the port (confirmed via a raw packet
capture that was missing exactly its leading byte).

The receiver role doesn't have this problem, since the host always
writes first there.

Fix: wait for DTR (most serial libraries assert it on `open()`) before
sending anything, if your USB serial stack exposes it. This is the
correct, portable signal for "the host is actually listening now."

### Command tag translation, if bypassing `harness-host`

`run_sender` emits frames tagged `OutRequest` and expects
`InResponse` back. `run_receiver` only accepts `OriginalPsbt` and
responds with `SignedPsbt`. In a real two-board setup, `harness-host`'s
`run_v1_roundtrip` translates between these tag pairs while relaying --
that's intentional, not an inconsistency to "fix" by making the tags
match.

If you're testing one board directly against a host-side tool instead of
through `harness-host` (the way `sender-sim`/`receiver-sim` do), that
tool needs to do the same translation, or speak the other side's
vocabulary directly. Wiring `run_sender` and `run_receiver` together
with no translation layer at all always fails with `UnexpectedCommand`
-- this bit both a test in `harness-device` itself and the first version
of `sender-sim` before the fix.

### `process_response` expects base64 text

This one's already fixed in `harness-device`'s `run_receiver`, but worth
knowing if you're touching that code path or writing something similar:
`process_response` (called on the sender side) expects the proposal PSBT
as base64-encoded text, not raw serialized bytes. Sending
`proposal_psbt.serialize()` directly produces a response the sender can
never parse -- the device reports success, the host can't do anything
with what it received.

## Heap sizing

`harness-device` needs `alloc`. A `linked_list_allocator::LockedHeap`
(or equivalent) with a static byte array works fine on Cortex-M-class
chips with even modest RAM. What's worked so far:

- **v1 sender/receiver** (real BIP78 validation, PSBT parsing,
  secp256k1 operations): 16 KiB was sufficient for the fixture PSBT used
  in testing. A much larger real-world PSBT might need more -- if
  something hard-faults where a smaller test case didn't, suspect the
  heap size first.
- **v2 probe** (`ShortId`/mailbox derivation only -- SHA256 and bech32m
  encoding, no PSBT/secp256k1 involved): 8 KiB was plenty.

## Debugging without a serial console

If you don't have `println!`/RTT debug output working yet (chicken-and-
egg: you're bringing up the very serial link you'd use for that), a
single LED is enough to bisect where something breaks -- but **don't
count blinks**. Fast, precisely-timed blink sequences turned out to be
genuinely unreliable to count correctly by eye under real debugging
pressure, and cost real time being mis-read multiple times.

What worked instead: a `STOP_AT` constant you change, rebuild, and
reflash for each candidate checkpoint, where each checkpoint just turns
the LED **solid on and halts forever** (never blinks). The question at
each step is binary -- "did it turn on, yes or no" -- with no counting
involved:

```rust
const STOP_AT: u32 = 1; // bump this, rebuild, reflash, repeat

fn halt_here() -> ! {
    led_on();
    loop { cortex_m::asm::nop(); }
}

// ... at each candidate point in your bring-up sequence:
if STOP_AT == 1 { halt_here(); } // reached main(), LED works at all
// ...
if STOP_AT == 2 { halt_here(); } // clock config didn't hang
// etc.
```

Binary search on `STOP_AT`'s value (jump to the middle of the remaining
range, not always +1) narrows things down fast once you have more than
a couple of checkpoints.

## Flashing

If your board supports both a USB bootloader (DFU or similar) and a
proper debug probe (SWD/JTOG via ST-Link, J-Link, Black Magic Probe,
etc.), prefer the debug probe. USB bootloader protocols turned out to be
noticeably more fragile in practice on real (imperfect) cables/hubs/
adapters -- larger binaries failed partway through transfer with
low-level USB errors, inconsistently, while the same binaries flashed
via SWD (using [`probe-rs`](https://probe.rs)) succeeded reliably on the
first or second try every time. A debug probe also gets you real
breakpoint/RTT debugging for free, which the LED-bisection approach
above is only a fallback for.

## Checklist

- [ ] Stage A: raw transport (USB CDC or otherwise) enumerates and
      echoes bytes, confirmed by hand with a serial terminal
- [ ] `Transport` implemented, following the contract above
- [ ] If your USB stack is polled: confirmed you keep polling after
      writes, not just up to `Ok`
- [ ] If your role writes first (sender): DTR (or equivalent) wait
      before sending
- [ ] Wired to `harness_device::run_receiver` / `run_sender` /
      `run_v2_probe` directly -- no reimplemented payjoin logic in your
      firmware
- [ ] Tested against `sender-sim` / `receiver-sim` (or a second board,
      if you have one) with a real packet capture confirming actual
      bytes moved, not just a status LED
- [ ] Heap sized generously for your test PSBT, revisit if larger real
      PSBTs are tested later
- [ ] CI added for at least a cross-compile build check (hardware
      itself can't run in CI, but a broken build shouldn't go unnoticed)
