#!/usr/bin/env bash
set -euo pipefail

windows_target=x86_64-pc-windows-gnu

cargo fmt --check
cargo check
cargo clippy -- -D warnings
cargo test
cargo check --target "$windows_target"
cargo clippy --target "$windows_target" -- -D warnings
