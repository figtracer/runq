# Measurements

Measured on Apple Silicon macOS 26.5.1, 2026-10-09. These results describe the
compact build and sleeping workloads, not compiler performance or Linux.

| Metric | Result |
|---|---:|
| Executable | 155,936 bytes / 152.3 KiB |
| Idle broker, three-trial median | 0.907 MiB |
| Broker + one foreground supervisor | 1.844 MiB |
| Broker + eight foreground clients | 8.190 MiB |
| Eight-client range | 8.174–8.206 MiB |
| Broker CPU monitoring one sleeping job | 0.044% of one core |
| Idle CPU observed over 10 seconds | 0 ms |

Memory is the sum of manager physical-footprint charges, excluding the workload.
RSS is recorded separately. Zero observed idle CPU is limited to that sample
window. Measurements are checkpoints, not lifetime peaks.

## Comparison

The same three-trial foreground experiment measured Task Spooler 1.0.4 at
0.891 MiB idle and 9.159 MiB with eight clients. Its executable was 92.4 KiB.
Task Spooler has the smaller idle broker; runq used less aggregate memory in this
specific foreground test. Feature coverage differs. Detached submission modes
can retain fewer processes and should be compared separately.

The previous runq build measured 347.2 KiB, 1.047 MiB idle, and 9.424 MiB with
eight clients. Rebuilding the standard library reduced the file. Limiting
allocator caches reduced steady memory. Wrapped `true` startup increased from
17.2 to 26.5 ms median in 20-sample runs; direct `true` also varied. Do not treat
that small timing sample as a precise isolation of allocator startup cost.

## Validation and reproduction

The recorded build passed 20 process integration tests, two Rust unit tests,
Clippy, and formatting. Its SHA-256 is:

```text
395fa97780f728638f376566648cb705b7ad9b79eb86d05f5926446a24e6cc76
```

See [development](dev/README.md#measure) for reproduction commands. Generated
reports and binaries are distributed separately from the source repo. Resource
limits remain sampled and best-effort; see [the command reference](cli.md).
