# Development

## Layout

| Path | Responsibility |
|---|---|
| `src/main.rs` | CLI and argument validation |
| `src/client.rs` | Foreground supervision, launch gate, and control commands |
| `src/daemon.rs` | Bounded queue, reservations, and memory enforcement |
| `src/os.rs` | Process accounting, signals, and platform policy |
| `src/wire.rs` | Fixed 32-byte protocol frames |
| `tests/integration.py` | Real-process lifecycle and resource checks |
| `scripts/` | Builds, sequential checks, and measurement harnesses |

Keep binaries, generated measurements, and experiment reports outside the source
repo. Local `target/`, `results/`, `runs/`, and `dist/` directories are ignored.

## Build and check

Rust 1.88+ and Python 3 are required. Run all checks sequentially:

```sh
./scripts/check.sh
```

This runs formatting, Clippy with warnings denied, unit tests, a release build,
and process integration tests. It sets one Cargo build job and one test thread.
It respects `CARGO_TARGET_DIR`. Tests use private temporary sockets and allocate
at most 64 MiB per workload. Only macOS has been exercised so far.

The standard release build is sufficient for development:

```sh
CARGO_BUILD_JOBS=1 cargo build --release --locked
```

An optional compact build recompiles Rust's existing standard library for size.
It requires Cargo's unstable `build-std` support and a pinned nightly:

```sh
rustup toolchain install nightly-2026-09-24 --profile minimal --component rust-src
./scripts/build-compact.sh
```

The script prints the executable path and leaves the default toolchain unchanged.
Both release builds abort on panic. The compact build omits the standard library's
default backtrace feature. This reduces file size; it is not a 10× RAM improvement.

## Runtime policy

On macOS, the broker and supervisors self-exec once with `MallocMaxMagazines=1`
and `MallocNanoZone=0`. One cache set without a nano zone reduces their steady
footprint. The launch helper restores both original settings, including absence,
and removes internal variables before the workload starts. This policy adds
startup work and does not apply to Linux.

Keep the signal wakeup, launch-registration gate, and cleanup guards intact.
Idle processes block in `poll`. Memory enforcement is sampled, never a hard limit.

## Measure

Preserve the previous binary before changing performance-sensitive code. Compare
before and after on the same machine, with the same workload and process counts.

```sh
python3 scripts/measure.py --binary /path/to/runq --seconds 10 \
  --max-daemon-mib 2 --max-supervisor-mib 2
python3 scripts/compare.py --runq /path/to/runq \
  --runq-before /path/to/previous/runq --trials 3 --seconds 2
```

For the compact macOS build, also pass `--max-binary-kib 160 --max-manager-mib 8`
to `measure.py`. These local regression budgets allow headroom above 152 KiB and
7.22 MiB while catching a return to the previous 8.2 MiB footprint. The 8 MiB
manager budget follows the paired allocator measurements; the file budget still
catches a return to the previous 347 KiB build.
They are not allocator limits. Harnesses write raw JSON under ignored `results/`.
The comparator accepts isolated competitor executables:

```sh
python3 scripts/compare.py --runq /path/to/runq \
  --task-spooler /path/to/ts --pueue /path/to/pueue --pueued /path/to/pueued \
  --memmux /path/to/memmuxd --parallel /path/to/parallel --trials 3 --seconds 2
```

To measure broker response costs, add `--status-queries 500` to the paired
runq command. After measuring eight real clients, this opens 55 synthetic queued
leases. Each of 500 status responses must describe all 63 jobs. It records only
broker CPU and memory for this probe. It does not simulate 63 foreground
processes or measure task throughput. No additional workload starts.

[Recorded measurements](../results.md) · [Commands](../cli.md) · [README](../../README.md)
