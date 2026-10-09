#!/bin/sh
# Keep compilation and process tests sequential and resource-bounded.
set -eu

task_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd)
cd "$task_root"
export CARGO_BUILD_JOBS=1
export RUST_TEST_THREADS=1

cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
RUNQ_BIN="${CARGO_TARGET_DIR:-target}/release/runq" python3 tests/integration.py
