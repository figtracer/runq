#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("runq currently supports macOS and Linux");

mod client;
mod daemon;
mod os;
mod wire;

use std::{ffi::OsString, io, os::unix::fs::DirBuilderExt, path::PathBuf};

const HELP: &str = "runq: bounded command queue with sampled memory supervision

  runq [--socket PATH] daemon [--memory 8GiB] [--cpu-slots 2]
                             [--max-running 1] [--sample-ms 250]
  runq [--socket PATH] exec [--memory SIZE] [--cpu-slots N]
                           [--timeout SECONDS] -- COMMAND [ARG...]
  runq [--socket PATH] status
  runq [--socket PATH] cancel JOB_ID
  runq [--socket PATH] shutdown
  runq inspect PID

RUNQ_SOCKET selects a shared socket. Default: $TMPDIR/runq-UID/control.sock.
Unspecified job resources reserve the daemon's entire capacity.
Memory is sampled, NOT a kernel-enforced ceiling. Exit 125 means supervision failed.
macOS measures physical footprint; Linux measures aggregate RSS (shared pages may be counted twice).
";

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("runq: {error}");
            125
        }
    };
    std::process::exit(code);
}

fn run() -> io::Result<i32> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() || matches!(args[0].to_str(), Some("--help" | "-h" | "help")) {
        print!("{HELP}");
        return Ok(0);
    }
    if args[0] == "--version" {
        println!("runq {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if args[0] == "_launch" {
        if args.len() < 4 || args[2] != "--" {
            return Err(io::Error::other("invalid internal launch"));
        }
        return client::launch(
            text(&args[1])?.parse().map_err(io::Error::other)?,
            &args[3..],
        );
    }
    if args[0] == "inspect" {
        if args.len() != 2 {
            return Err(io::Error::other("inspect needs one PID"));
        }
        let pid: i32 = text(&args[1])?.parse().map_err(io::Error::other)?;
        if pid <= 0 {
            return Err(io::Error::other("PID must be positive"));
        }
        let usage = os::usage(pid)?;
        println!(
            "pid={pid} memory_bytes={} rss_bytes={} cpu_ns={}",
            usage.memory, usage.rss, usage.cpu_ns
        );
        return Ok(0);
    }
    let mut index = 0;
    let socket = if args[0] == "--socket" {
        index = 2;
        PathBuf::from(
            args.get(1)
                .ok_or_else(|| io::Error::other("missing socket path"))?,
        )
    } else if let Some(path) = std::env::var_os("RUNQ_SOCKET") {
        PathBuf::from(path)
    } else {
        // SAFETY: geteuid has no pointer arguments.
        std::env::temp_dir()
            .join(format!("runq-{}", unsafe { libc::geteuid() }))
            .join("control.sock")
    };
    let subcommand = args
        .get(index)
        .ok_or_else(|| io::Error::other("missing subcommand"))?;
    index += 1;
    match text(subcommand)? {
        "daemon" => {
            os::configure_allocator()?;
            let mut config = daemon::Config {
                memory: 8 * 1024 * 1024 * 1024,
                slots: 2,
                max_running: 1,
                sample_ms: 250,
            };
            while index < args.len() {
                let key = text(&args[index])?;
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| io::Error::other("missing option value"))?;
                match key {
                    "--memory" => config.memory = memory(text(value)?)?,
                    "--cpu-slots" => config.slots = positive(value)?,
                    "--max-running" => {
                        config.max_running =
                            positive(value)?.try_into().map_err(io::Error::other)?
                    }
                    "--sample-ms" => config.sample_ms = positive(value)?,
                    _ => return Err(io::Error::other(format!("unknown option: {key}"))),
                }
                index += 2;
            }
            if config.max_running > 64
                || config.slots > 4096
                || !(25..=5000).contains(&config.sample_ms)
            {
                return Err(io::Error::other(
                    "max-running must be 1..64, cpu-slots 1..4096, sample-ms 25..5000",
                ));
            }
            if let Some(parent) = socket.parent() {
                match std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            daemon::run(&socket, config)?;
            Ok(0)
        }
        "exec" => {
            os::configure_allocator()?;
            let (mut mem, mut slots, mut timeout) = (0, 0, 0);
            while index < args.len() && args[index] != "--" {
                let key = text(&args[index])?;
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| io::Error::other("missing option value"))?;
                match key {
                    "--memory" => mem = memory(text(value)?)?,
                    "--cpu-slots" => slots = positive(value)?,
                    "--timeout" => {
                        timeout = positive(value)?
                            .checked_mul(1000)
                            .ok_or_else(|| io::Error::other("timeout overflow"))?
                    }
                    _ => return Err(io::Error::other(format!("unknown option: {key}"))),
                }
                index += 2;
            }
            if args.get(index).is_none_or(|s| s != "--") || args.len() <= index + 1 {
                return Err(io::Error::other("exec requires -- COMMAND [ARG...]"));
            }
            client::execute(&socket, mem, slots, timeout, &args[index + 1..])
        }
        "status" | "shutdown" if index == args.len() => client::control(
            &socket,
            [
                if subcommand == "status" {
                    wire::STATUS
                } else {
                    wire::SHUTDOWN
                },
                0,
                0,
                0,
            ],
        ),
        "cancel" if index + 1 == args.len() => {
            client::control(&socket, [wire::CANCEL, positive(&args[index])?, 0, 0])
        }
        _ => Err(io::Error::other("invalid command; see runq --help")),
    }
}

fn text(value: &OsString) -> io::Result<&str> {
    value
        .to_str()
        .ok_or_else(|| io::Error::other("option must be valid UTF-8"))
}

fn positive(value: &OsString) -> io::Result<u64> {
    let number: u64 = text(value)?.parse().map_err(io::Error::other)?;
    if number == 0 {
        return Err(io::Error::other("value must be positive"));
    }
    Ok(number)
}

fn memory(value: &str) -> io::Result<u64> {
    let (number, multiplier) = [
        ("GiB", 1u64 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
        ("B", 1),
    ]
    .into_iter()
    .find_map(|(suffix, factor)| value.strip_suffix(suffix).map(|n| (n, factor)))
    .unwrap_or((value, 1));
    let bytes = number
        .parse::<u64>()
        .map_err(io::Error::other)?
        .checked_mul(multiplier)
        .filter(|n| *n > 0)
        .ok_or_else(|| io::Error::other("memory must be positive and fit in u64"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_memory_units_and_overflow() {
        assert_eq!(memory("8GiB").unwrap(), 8 << 30);
        assert_eq!(memory("64MiB").unwrap(), 64 << 20);
        for bad in ["0", "-1GiB", "8GB", "18446744073709551615GiB"] {
            assert!(memory(bad).is_err());
        }
    }
}
