# Commands

```text
runq [--socket PATH] daemon [--memory 8GiB] [--cpu-slots 2]
                          [--max-running 1] [--sample-ms 250]
runq [--socket PATH] exec [--memory SIZE] [--cpu-slots N]
                        [--timeout SECONDS] -- COMMAND [ARG...]
runq [--socket PATH] status
runq [--socket PATH] cancel JOB_ID
runq [--socket PATH] shutdown
runq inspect PID
```

Memory sizes accept bytes or `B`, `KiB`, `MiB`, and `GiB` suffixes.
`RUNQ_SOCKET` selects a socket unless `--socket` is supplied. The default is
`$TMPDIR/runq-UID/control.sock`. A custom socket's parent must be a dedicated
directory owned by the current user with mode `0700`.

## Admission and workers

Each command defaults to the broker's entire memory and CPU reservation. Jobs
enter in FIFO order when their memory reservations, CPU slots, and running-job
count fit. Reservations are released after the whole process group disappears.
CPU slots control admission and worker counts; they are not a CPU-time quota.

Smaller reservations can share a broker configured for concurrency:

```sh
runq exec --memory 2GiB --cpu-slots 1 --timeout 300 -- cargo check -p some-crate
```

The supervisor sets `CARGO_BUILD_JOBS` to the granted slot count. It sets
`NEXTEST_TEST_THREADS` and `RUST_TEST_THREADS` to at most two, bounded by that
count. Direct Cargo options `-j`, `--jobs`, and `--test-threads` above those
limits are rejected. Nested `runq exec` is rejected to avoid a reservation deadlock.

Shell programs and Cargo configuration can override worker defaults. Threads
created inside tests are not capped by this adapter. The process-group memory
monitor still applies to wrapped shells.

## Execution and cleanup

Commands run in the caller's working directory and inherit stdin, stdout, and
stderr. Diagnostics go to stderr. Each caller keeps one supervisor, including
while queued. The broker holds at most 64 client connections.

A gated launch registers the process group before executing the workload.
Memory breaches and accounting failures trigger SIGKILL on detection.
Cancellation and deadlines send SIGTERM, followed by SIGKILL after two seconds.
The broker cleans up disconnected supervisors; a supervisor cleans up its group
if the broker dies. Successful commands also have remaining group members stopped.

Completed jobs are discarded. Queue state is ephemeral; a broker restart recovers
its socket but does not replay commands. Piped input works. Interactive PTY and
terminal job-control support is not implemented.

## Exit status

Normal exit codes pass through. Signals use `128 + signal`. Supervision errors
return `125`; missing executables return `127`; executables that cannot start
return `126`. Workloads can also return these values, so retain stderr.

## Accounting limits

macOS measures process-group physical footprint; Linux sums RSS and can count
shared pages more than once. `inspect PID` prints per-process memory, RSS, and
CPU nanoseconds. Sampling defaults to 250 ms and accepts 25–5,000 ms. Short jobs
can finish between samples; a zero sampled peak can mean no sample was taken.
The fixed PID buffer covers up to 4,095 members and stops jobs on overflow.

Budgets exclude broker/client overhead and unrelated applications. This is not
a host-memory limit or security sandbox. Descendants can escape their process
group; simultaneous loss of broker and supervisor can leave work running.
The private socket serves trusted programs of the same user.

[Back to README](../README.md)
