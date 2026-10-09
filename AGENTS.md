# runq agent guide

runq reserves capacity before starting validation commands and monitors their
process groups. Read [docs/cli.md](docs/cli.md) for the behavior and limits.

## Rules

- Keep this a small single-crate command supervisor. Preserve inherited IO and bound broker state.
- Never describe sampled monitoring as a hard memory limit or security sandbox.
- Keep macOS physical footprint, Linux RSS, and CPU time units explicit.
- Keep binaries, generated measurements, and experiment reports out of the source repo.
- Keep README.md short. Put command details in docs/cli.md and build details in docs/dev/README.md.

## Develop

Read [docs/dev/README.md](docs/dev/README.md). Run `./scripts/check.sh` after
behavior changes. Keep validation serialized: one Cargo build job, one test suite
at a time, and small allocations in resource tests. State which OS was tested.

Measure after changes to the process model, dependencies, or sampling. Preserve
the previous binary for paired measurements and use the documented compact-build
budgets. Change budgets only with fresh measurement evidence.
