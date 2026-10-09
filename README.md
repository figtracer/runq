# runq

**A small Rust queue for memory-heavy validation commands.**

[Commands](docs/cli.md) · [Comparisons](docs/results.md) · [Development](docs/dev/README.md) · [Agent guide](AGENTS.md)

runq reserves memory and CPU capacity before starting a command. It caps Cargo
and test workers, then stops process groups that exceed a sampled memory budget.
Every caller shares one broker.

## Quick start

Build with Rust 1.88 or newer:

```sh
git clone https://github.com/figtracer/runq.git
cd runq
CARGO_BUILD_JOBS=1 cargo build --release --locked
./target/release/runq daemon --memory 8GiB --cpu-slots 2 --max-running 1
```

In another terminal:

```sh
./target/release/runq exec -- cargo check --workspace
./target/release/runq exec -- cargo test --workspace
./target/release/runq status
./target/release/runq shutdown
```

Use the executable's absolute path when working in another repository. All callers
must use the same socket. By default, each command reserves the whole budget,
so builds and tests stay serialized until the command finishes.

## Small by design

One executable, one dependency (`libc`), no async runtime, and no worker threads.
Commands inherit their IO. The broker keeps no output or completed-job history.
The optional compact build measured **152 KiB**, **0.91 MiB idle**, and **8.19 MiB
with eight clients** on Apple Silicon macOS ([method and limits](docs/results.md)).

[Compare with Task Spooler, Pueue, MemMux, and GNU Parallel](docs/results.md#existing-tools).
Foreground and detached modes retain different numbers of processes.

## Limits

Memory enforcement is sampled and best-effort. A job can overshoot between
samples, and descendants can escape by changing process groups. Only commands
launched through runq are managed; the Codex app itself is not limited.
macOS is tested. The Linux backend is experimental and has no cgroup support.

[Command reference and behavior](docs/cli.md) · [Builds and checks](docs/dev/README.md)

## License

[MIT](LICENSE).
