//! Binary entry point. All the testable logic lives in the crate's
//! `lib.rs` (`Args`, `FramedPort`, `run_v1_roundtrip`); this file is just
//! the untested glue that opens real serial ports.

use std::time::Duration;

use anyhow::{Context, Result};
use harness_host::{run_v1_roundtrip, Args, FramedPort};

fn main() -> Result<()> {
    let args = Args::parse()?;

    let sender = serialport::new(&args.sender_port, args.baud)
        .timeout(Duration::from_secs(30))
        .open()
        .with_context(|| format!("failed to open sender port {}", args.sender_port))?;
    let receiver = serialport::new(&args.receiver_port, args.baud)
        .timeout(Duration::from_secs(30))
        .open()
        .with_context(|| format!("failed to open receiver port {}", args.receiver_port))?;

    let mut sender = FramedPort::new(sender);
    let mut receiver = FramedPort::new(receiver);

    run_v1_roundtrip(&mut sender, &mut receiver)
}
