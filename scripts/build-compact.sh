#!/bin/sh
# Rebuild std with the same size policy as runq, using a fixed toolchain.
set -eu

task_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd)
task_toolchain=nightly-2026-09-24
task_host=$(rustup run "$task_toolchain" rustc -vV | sed -n 's/^host: //p')
if [ -z "$task_host" ]; then
    printf '%s\n' "Install $task_toolchain with rust-src first; see README.md." >&2
    exit 1
fi

export CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$task_root/target/compact}"
rustup run "$task_toolchain" cargo build \
    --manifest-path "$task_root/Cargo.toml" --release --locked --target "$task_host" \
    -Zbuild-std=std,panic_abort -Zbuild-std-features=optimize_for_size "$@"

printf '%s\n' "Compact executable: $CARGO_TARGET_DIR/$task_host/release/runq"
