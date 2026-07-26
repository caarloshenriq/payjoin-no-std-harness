//! Binary entry point. All the testable logic lives in the crate's
//! `lib.rs` (`Args`, `FramedPort`, `run_v1_roundtrip`, `run_v2_probe`);
//! this file is just the untested glue that opens real serial ports and
//! dispatches on `Args::mode`.

use std::time::Duration;

use anyhow::{Context, Result};
use harness_host::{run_v1_roundtrip, run_v2_probe, Args, FramedPort, Mode};

fn main() -> Result<()> {
    let args = Args::parse()?;

    match args.mode {
        Mode::V1 => {
            let sender_port = args.sender_port.as_deref().expect("validated by Args::parse");
            let receiver_port = args.receiver_port.as_deref().expect("validated by Args::parse");

            let sender = serialport::new(sender_port, args.baud)
                .timeout(Duration::from_secs(30))
                .open()
                .with_context(|| format!("failed to open sender port {sender_port}"))?;
            let receiver = serialport::new(receiver_port, args.baud)
                .timeout(Duration::from_secs(30))
                .open()
                .with_context(|| format!("failed to open receiver port {receiver_port}"))?;

            let mut sender = FramedPort::new(sender);
            let mut receiver = FramedPort::new(receiver);

            run_v1_roundtrip(&mut sender, &mut receiver)
        }
        Mode::V2Probe => {
            let device_port = args.device_port.as_deref().expect("validated by Args::parse");
            let seed = args.seed.as_deref().expect("validated by Args::parse");

            let device = serialport::new(device_port, args.baud)
                .timeout(Duration::from_secs(30))
                .open()
                .with_context(|| format!("failed to open device port {device_port}"))?;

            let mut device = FramedPort::new(device);

            run_v2_probe(&mut device, seed.as_bytes())?;
            Ok(())
        }
    }
}
