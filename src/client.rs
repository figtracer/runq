use std::{
    ffi::OsString,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            net::UnixStream,
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::Path,
    process::{Child, Command},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use crate::{
    os,
    wire::{self, *},
};

struct Receiver {
    bytes: [u8; SIZE],
    filled: usize,
    signals: os::Signals,
}

impl Receiver {
    fn new(signals: os::Signals) -> Self {
        Self {
            bytes: [0; SIZE],
            filled: 0,
            signals,
        }
    }

    fn tick(&mut self, stream: &mut UnixStream, timeout: i32) -> io::Result<Option<Frame>> {
        let mut fds = [
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.signals.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        os::poll(&mut fds, timeout)?;
        self.signals.drain();
        if fds[0].revents == 0 {
            return Ok(None);
        }
        match stream.read(&mut self.bytes[self.filled..]) {
            Ok(0) => Err(io::Error::other("daemon disconnected")),
            Ok(count) => {
                self.filled += count;
                if self.filled == SIZE {
                    self.filled = 0;
                    Ok(Some(decode(&self.bytes)))
                } else {
                    Ok(None)
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

struct OwnedChild {
    child: Child,
    pgid: i32,
    reaped: bool,
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Error paths, including broker death, must not leave the workload running.
        os::signal_group(self.pgid, libc::SIGKILL);
        if !self.reaped {
            let _ = self.child.wait();
        }
    }
}

pub fn execute(
    socket: &Path,
    memory: u64,
    slots: u64,
    timeout_ms: u64,
    args: &[OsString],
) -> io::Result<i32> {
    if std::env::var_os("RUNQ_JOB_ID").is_some() {
        return Err(io::Error::other(
            "nested runq exec would acquire a second reservation; run the child command directly",
        ));
    }
    let signals = os::install_signals()?;
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| io::Error::new(e.kind(), format!("{e}; start runq daemon first")))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    wire::send(&mut stream, [SUBMIT, memory, slots, timeout_ms])?;
    let mut receiver = Receiver::new(signals);
    let (id, slots) = loop {
        let signal = os::SIGNAL.load(Ordering::Relaxed);
        if signal != 0 {
            return Ok(128 + signal);
        }
        if let Some(frame) = receiver.tick(&mut stream, -1)? {
            match frame {
                [QUEUED, id, _, _] => eprintln!("runq: job {id} queued"),
                [GRANT, id, slots, _] => break (id, slots),
                [FAILED, code, _, _] => return Err(io::Error::other(wire::reason(code))),
                _ => return Err(io::Error::other("unexpected admission response")),
            }
        }
    };
    validate_workers(args, slots)?;

    // The helper cannot execute the workload until the daemon registers its group.
    // This closes the spawn-before-registration race on cancellation/disconnect.
    let (mut gate, child_gate) = UnixStream::pair()?;
    let gate_fd = child_gate.as_raw_fd();
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("_launch")
        .arg(gate_fd.to_string())
        .arg("--")
        .args(args)
        .env("RUNQ_JOB_ID", id.to_string())
        .env("CARGO_BUILD_JOBS", slots.to_string())
        .env("NEXTEST_TEST_THREADS", slots.min(2).to_string())
        .env("RUST_TEST_THREADS", slots.min(2).to_string())
        .process_group(0);
    // This single-threaded supervisor spawns only this helper. Clear CLOEXEC
    // here so Command can use posix_spawn instead of a pre_exec/fork path.
    // The parent closes child_gate immediately after spawn, including on errors.
    // SAFETY: child_gate owns this descriptor throughout the call.
    if unsafe { libc::fcntl(gate_fd, libc::F_SETFD, 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let child = command.spawn()?;
    drop(child_gate);
    drop(command);
    let mut owned = OwnedChild {
        pgid: child.id() as i32,
        child,
        reaped: false,
    };
    wire::send(&mut stream, [START, id, owned.pgid as u64, 0])?;
    let handshake = Instant::now();
    loop {
        let signal = os::SIGNAL.load(Ordering::Relaxed);
        if signal != 0 {
            return Ok(128 + signal);
        }
        if handshake.elapsed() > Duration::from_secs(5) {
            return Err(io::Error::other("daemon did not acknowledge launch"));
        }
        if let Some(frame) = receiver.tick(&mut stream, 5000)? {
            match frame {
                [READY, job_id, _, _] if job_id == id => break,
                [FAILED, code, _, _] => return Err(io::Error::other(wire::reason(code))),
                _ => return Err(io::Error::other("unexpected launch response")),
            }
        }
    }
    gate.write_all(&[1])?;
    drop(gate);
    eprintln!("runq: job {id} running (pid {})", owned.pgid);

    let mut exit_code = None;
    let mut failure = 0;
    let mut interrupted = 0;
    let mut signal_time = None;
    loop {
        let signal = os::SIGNAL.swap(0, Ordering::Relaxed);
        if signal != 0 {
            interrupted = signal;
            os::signal_group(owned.pgid, signal);
            signal_time.get_or_insert_with(Instant::now);
        }
        if signal_time.is_some_and(|t: Instant| t.elapsed() >= Duration::from_secs(2)) {
            os::signal_group(owned.pgid, libc::SIGKILL);
            signal_time = None;
        }
        if exit_code.is_none()
            && let Some(status) = owned.child.try_wait()?
        {
            owned.reaped = true;
            exit_code = Some(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
            );
            wire::send(&mut stream, [FINISH, id, 0, 0])?;
        }
        let timeout = signal_time.map_or(-1, |t: Instant| {
            Duration::from_secs(2)
                .saturating_sub(t.elapsed())
                .as_millis()
                .max(1) as i32
        });
        if let Some(frame) = receiver.tick(&mut stream, timeout)? {
            match frame {
                [FAILED, code, _, peak] => {
                    failure = code;
                    eprintln!(
                        "runq: job {id}: {} (sampled peak {} KiB)",
                        wire::reason(code),
                        peak / 1024
                    );
                    os::signal_group(owned.pgid, libc::SIGTERM);
                    signal_time.get_or_insert_with(Instant::now);
                }
                [DONE, _, peak, code] => {
                    eprintln!("runq: job {id} finished; sampled peak {} KiB", peak / 1024);
                    if exit_code.is_none() {
                        let status = owned.child.wait()?;
                        owned.reaped = true;
                        exit_code =
                            Some(status.code().unwrap_or(128 + status.signal().unwrap_or(1)));
                    }
                    // The daemon confirmed the group is gone. Do not signal a
                    // potentially reused group ID from the guard's destructor.
                    owned.pgid = 0;
                    if interrupted != 0 {
                        return Ok(128 + interrupted);
                    }
                    if code != 0 || failure != 0 {
                        return Ok(125);
                    }
                    return Ok(exit_code.unwrap());
                }
                _ => return Err(io::Error::other("unexpected execution response")),
            }
        }
    }
}

pub fn launch(fd: i32, args: &[OsString]) -> io::Result<i32> {
    if fd < 3 || args.is_empty() {
        return Err(io::Error::other("invalid launch gate"));
    }
    // SAFETY: this is the inherited descriptor explicitly passed by execute().
    let mut gate = unsafe { UnixStream::from_raw_fd(fd) };
    let mut fds = [libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }];
    os::poll(&mut fds, 5000)?;
    if fds[0].revents == 0 {
        return Err(io::Error::other("launch gate timed out"));
    }
    let mut token = [0];
    gate.read_exact(&mut token)
        .map_err(|e| io::Error::other(format!("launch gate read (fd {fd}): {e}")))?;
    if token[0] != 1 {
        return Err(io::Error::other("invalid launch token"));
    }
    drop(gate);
    let mut command = Command::new(&args[0]);
    command.args(&args[1..]);
    os::restore_workload_allocator(&mut command);
    let error = command.exec();
    eprintln!("runq: {}: {error}", args[0].to_string_lossy());
    Ok(if error.kind() == io::ErrorKind::NotFound {
        127
    } else {
        126
    })
}

pub fn control(socket: &Path, frame: Frame) -> io::Result<i32> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    wire::send(&mut stream, frame)?;
    loop {
        match wire::receive(&mut stream)? {
            [HEADER, memory, slots, count] => println!(
                "budget={}MiB cpu_slots={slots} jobs={count} enforcement=sampled",
                memory / 1024 / 1024
            ),
            [ROW, id, state, measured] => {
                let phase = ["queued", "starting", "running", "stopping"]
                    .get(state as usize)
                    .copied()
                    .unwrap_or("unknown");
                print!("job={id} state={phase} memory={}KiB ", measured / 1024);
            }
            [DETAIL, _, peak, reserved] => println!(
                "peak={}KiB reserved={}MiB",
                peak / 1024,
                reserved / 1024 / 1024
            ),
            [DONE | END, ..] => return Ok(0),
            [FAILED, code, ..] => return Err(io::Error::other(wire::reason(code))),
            _ => return Err(io::Error::other("unexpected control response")),
        }
    }
}

fn validate_workers(args: &[OsString], slots: u64) -> io::Result<()> {
    if Path::new(&args[0]).file_name().is_none_or(|n| n != "cargo") {
        return Ok(());
    }
    let mut iter = args[1..].iter();
    while let Some(arg) = iter.next() {
        let Some(arg) = arg.to_str() else {
            continue;
        };
        let test_flag = arg == "--test-threads" || arg.starts_with("--test-threads=");
        let value = if matches!(arg, "-j" | "--jobs" | "--test-threads") {
            Some(
                iter.next()
                    .and_then(|v| v.to_str())
                    .ok_or_else(|| io::Error::other("missing worker count"))?,
            )
        } else if let Some(v) = arg
            .strip_prefix("--jobs=")
            .or_else(|| arg.strip_prefix("--test-threads="))
        {
            Some(v)
        } else {
            arg.strip_prefix("-j").filter(|v| !v.is_empty())
        };
        if let Some(value) = value {
            let cap = if test_flag { slots.min(2) } else { slots };
            if !value.parse::<u64>().is_ok_and(|n| n > 0 && n <= cap) {
                return Err(io::Error::other(format!(
                    "worker option {arg} exceeds reserved limit {cap}"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_worker_overrides() {
        for args in [
            vec!["cargo", "test", "-j8"],
            vec!["cargo", "+nightly", "clippy", "--jobs", "3"],
            vec!["cargo", "nextest", "run", "--test-threads=8"],
            vec!["cargo", "check", "-j", "-1"],
        ] {
            assert!(
                validate_workers(&args.iter().map(OsString::from).collect::<Vec<_>>(), 2).is_err()
            );
        }
        assert!(validate_workers(&["cargo".into(), "test".into(), "-j2".into()], 2).is_ok());
    }
}
