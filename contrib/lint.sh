#!/usr/bin/env bash
set -e

# Run clippy at top level for crates without feature-specific checks
echo "Running workspace lint..."
cargo clippy --locked --all-targets --keep-going --all-features -- -D warnings
