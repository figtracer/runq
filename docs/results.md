# Measurements and alternatives

Measured on Apple Silicon macOS 26.5.1, 2026-10-09. Memory is the sum of manager
physical-footprint charges, excluding the sleeping workload. These are steady
checkpoints, not lifetime peaks, compiler workloads, or Linux results.

## Existing tools

Three valid trials per tool, with one running job and seven queued jobs. Values
are medians in MiB. Each trial ran separately with private sockets and settings.

| Tool and measured mode | Idle | One job | Eight jobs |
|---|---:|---:|---:|
| runq, foreground, compact build at `6f60cc4` | 0.907 | 1.860 | 8.221 |
| [Task Spooler 1.0.4](https://viric.name/soft/ts/), foreground `-nf` | 0.891 | 1.922 | 9.143 |
| [Pueue 4.0.4](https://github.com/Nukesor/pueue/tree/v4.0.4), detached | 2.047 | 2.375 | 2.735 |
| Pueue, also retaining eight `wait --quiet` clients | — | — | 23.003 |
| [MemMux 0.9.0 prerelease](https://github.com/sumanyumuku98/MemMux/tree/v0.9.0), generic provider, no TUI | 2.188 | 2.797 | 3.157 |
| [GNU Parallel 20260922](https://www.gnu.org/software/parallel/), foreground semaphore per caller | — | 25.953 | 173.424 |

runq and Task Spooler retain a foreground process per caller. Pueue and MemMux
submit work to a daemon and let submission clients exit. Their detached modes
use less aggregate RAM here. Pueue's optional waiting clients do not reproduce
runq's inherited terminal IO. GNU Parallel was measured with
`--plain --semaphore --fg --jobs 1`; this does not represent its normal single
batch invocation. There is no universal memory winner across these modes.

| Tool | What it offers beyond this sleep experiment | Measured executable size |
|---|---|---:|
| runq | Reservations, Cargo/test worker caps, sampled process-group enforcement; no job history | 152.3 KiB |
| Task Spooler | Small C queue, concurrency slots, output files, job dependencies | 92.4 KiB |
| Pueue | Persistent jobs and logs, groups, dependencies, pause/resume | 4.84 MiB daemon + 5.04 MiB client |
| MemMux | Memory-aware agent scheduling, provider and worktree integration, terminal sessions | 3.93 MiB daemon |
| GNU Parallel | General shell parallelism and a counting semaphore | 533.5 KiB script, excluding Perl |

Sizes cover the measured files, not complete installations. Task Spooler was
built with Apple clang 21 and `-Os -DNDEBUG`, without its platform-specific
`GLIBCFLAGS`. Pueue and MemMux used their macOS arm64 release executables.
One GNU Parallel trial had an unreadable process counter and was discarded;
the missing final samples were rerun with the same settings. CPU snapshots
exclude transient helpers, so they are not used to rank competitors.

## Optimization record

The first compact build measured 155,936 bytes, compared with 347.2 KiB for an
earlier build. Rebuilding the standard library reduced the file. Limiting
allocator caches reduced the earlier eight-client result from 9.424 to 8.190 MiB.
Wrapped `true` startup increased from 17.2 to 26.5 ms in 20-sample runs; direct
`true` also varied. That sample does not isolate allocator startup cost.

The response-buffer change replaces a deque of frames with one byte buffer.
It sends consecutive replies together and preserves partial-write offsets. Job
state still disappears after process-group cleanup; the protocol, CLI, inherited
IO, and cancellation behavior are unchanged.

A separate paired three-trial comparison against `6f60cc4` measured:

| Metric | Before | After |
|---|---:|---:|
| Executable | 155,936 bytes | 155,936 bytes |
| Idle broker | 0.907 MiB | 0.907 MiB |
| Broker + one supervisor | 1.860 MiB | 1.844 MiB |
| Broker + eight clients | 8.206 MiB | 8.190 MiB |
| Eight-client range | 8.127–8.237 MiB | 8.143–8.190 MiB |
| Broker CPU for 500 full-queue status queries | 86.9 ms | 54.9 ms |
| Elapsed time for those queries | 0.228 s | 0.311 s |

The CPU reduction was 37% in this synthetic status probe. It used eight real
clients and 55 queued socket leases, with one sleeping workload. Every response
was checked for all 63 jobs. Elapsed time did not improve, and the memory ranges
overlap; neither is evidence of a reliable gain. This does not measure task
throughput or promise a general speedup.

Pre-sizing reply buffers increased the eight-client median by about 0.12 MiB in
another paired run, so that variant was dropped. A smaller initial PID buffer
also failed to show a reliable aggregate memory reduction and was dropped.
Assembly was not needed for the retained change.

The compact executable passed 20 process integration tests on macOS. Formatting,
Clippy, two Rust unit tests, and the standard release build also passed. Its
SHA-256 is:

```text
4b4a66be728b93aca54406ed15a440ad2ea1a3625a78c7cef875881956289719
```

## Reproduce

The comparison uses `scripts/compare.py`. It verifies one running and seven
queued workloads, excludes their memory, and reverses the order on alternate
trials. See [development](dev/README.md#measure) for build and benchmark commands.
Generated JSON and binaries stay outside the source repo.

All resource limits remain sampled and best-effort. See [the command
reference](cli.md) for cleanup behavior and enforcement limits.
