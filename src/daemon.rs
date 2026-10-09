use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use crate::{
    os,
    wire::{self, *},
};

// Bound both queued clients and status output. This is a local validation queue.
const MAX_CLIENTS: usize = 64;
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(5);
// Give ordinary commands a brief chance to clean up before forcing termination.
const KILL_GRACE: Duration = Duration::from_secs(2);

pub struct Config {
    pub memory: u64,
    pub slots: u64,
    pub max_running: usize,
    pub sample_ms: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Queued = 0,
    Granted = 1,
    Running = 2,
    Stopping = 3,
}

struct Job {
    id: u64,
    memory: u64,
    slots: u64,
    pgid: i32,
    phase: Phase,
    since: Instant,
    timeout: Duration,
    measured: u64,
    peak: u64,
    failure: u64,
}

struct Client {
    stream: UnixStream,
    input: [u8; SIZE],
    read: usize,
    output: VecDeque<[u8; SIZE]>,
    written: usize,
    job: Option<Job>,
    connected: bool,
    closing: bool,
    arrived: Instant,
}

impl Client {
    fn reply(&mut self, frame: Frame) {
        self.output.push_back(encode(frame));
    }

    fn stop(&mut self, reason: u64) {
        let Some(job) = self.job.as_mut() else {
            self.closing = true;
            return;
        };
        if job.phase == Phase::Stopping {
            return;
        }
        job.phase = Phase::Stopping;
        job.since = Instant::now();
        job.failure = reason;
        // Do not let an over-budget job allocate for another grace period.
        let signal = if reason == OVER_MEMORY || reason == MONITOR_ERROR {
            libc::SIGKILL
        } else {
            libc::SIGTERM
        };
        os::signal_group(job.pgid, signal);
        if reason != 0 {
            let frame = [FAILED, reason, job.id, job.peak];
            if self.connected {
                self.reply(frame);
            }
        }
    }
}

pub fn run(socket: &Path, config: Config) -> io::Result<()> {
    let parent = socket
        .parent()
        .ok_or_else(|| io::Error::other("socket needs a parent directory"))?;
    fs::create_dir_all(parent)?;
    let metadata = fs::symlink_metadata(parent)?;
    // This directory is private to one user; the daemon is not a security boundary
    // against other programs already running as that user.
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other(
            "socket directory must be owned by this user and not be a symlink",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "socket directory must have mode 0700 (use a dedicated directory)",
        ));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(socket.with_extension("lock"))?;
    // SAFETY: lock is an open descriptor retained for the daemon lifetime.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other("a daemon already owns this socket"));
    }
    match fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let signals = os::install_signals()?;
    eprintln!(
        "runq: ready at {} ({} MiB, {} CPU slots, {} running; sampled enforcement)",
        socket.display(),
        config.memory / 1024 / 1024,
        config.slots,
        config.max_running
    );
    let result = serve(listener, config, signals);
    let _ = fs::remove_file(socket);
    result
}

fn serve(listener: UnixListener, config: Config, signals: os::Signals) -> io::Result<()> {
    let mut clients: Vec<Client> = Vec::new();
    let mut pollfds = Vec::with_capacity(MAX_CLIENTS + 2);
    let mut sampler = os::Sampler::new();
    let mut next_id = 1u64;
    let mut stopping = false;
    let mut last_sample = Instant::now();
    let interval = Duration::from_millis(config.sample_ms);

    loop {
        if os::SIGNAL.swap(0, Ordering::Relaxed) != 0 {
            stopping = true;
        }
        if stopping {
            for client in &mut clients {
                client.stop(STOPPING);
            }
        }

        let now = Instant::now();
        let sample = now.duration_since(last_sample) >= interval;
        if sample {
            last_sample = now;
        }
        for client in &mut clients {
            let mut stop_reason = None;
            if let Some(job) = &mut client.job {
                match job.phase {
                    Phase::Granted if now.duration_since(job.since) >= LAUNCH_TIMEOUT => {
                        stop_reason = Some(EXPIRED)
                    }
                    Phase::Running => {
                        if !job.timeout.is_zero() && now.duration_since(job.since) >= job.timeout {
                            stop_reason = Some(EXPIRED);
                        } else if sample {
                            match sampler.group(job.pgid) {
                                Ok(bytes) => {
                                    job.measured = bytes;
                                    job.peak = job.peak.max(bytes);
                                    if bytes > job.memory {
                                        stop_reason = Some(OVER_MEMORY);
                                    }
                                }
                                Err(error) => {
                                    eprintln!("runq: job {}: {error}", job.id);
                                    stop_reason = Some(MONITOR_ERROR);
                                }
                            }
                        }
                    }
                    Phase::Stopping => {
                        if now.duration_since(job.since) >= KILL_GRACE {
                            os::signal_group(job.pgid, libc::SIGKILL);
                        }
                        // Keep the reservation until the owned group has disappeared.
                        if !os::group_exists(job.pgid) {
                            let frame = [DONE, job.id, job.peak, job.failure];
                            client.job = None;
                            if client.connected {
                                client.reply(frame);
                            }
                            client.closing = true;
                        }
                    }
                    _ => {}
                }
            } else if now.duration_since(client.arrived) >= LAUNCH_TIMEOUT {
                client.closing = true;
            }
            if let Some(reason) = stop_reason {
                client.stop(reason);
            }
        }
        clients
            .retain(|c| !(c.job.is_none() && (!c.connected || (c.closing && c.output.is_empty()))));
        if stopping && clients.is_empty() {
            return Ok(());
        }

        if !stopping {
            admit(&mut clients, &config);
        }
        pollfds.clear();
        pollfds.push(libc::pollfd {
            fd: listener.as_raw_fd(),
            events: if stopping { 0 } else { libc::POLLIN },
            revents: 0,
        });
        pollfds.push(libc::pollfd {
            fd: signals.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for client in &clients {
            pollfds.push(libc::pollfd {
                fd: if client.connected {
                    client.stream.as_raw_fd()
                } else {
                    -1
                },
                events: (if client.closing { 0 } else { libc::POLLIN })
                    | (if client.output.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    }),
                revents: 0,
            });
        }
        // Idle daemon blocks in the kernel. No timer, process scan, or busy loop.
        let timeout = if clients.is_empty() {
            -1
        } else {
            interval
                .saturating_sub(last_sample.elapsed())
                .as_millis()
                .max(1) as i32
        };
        if let Err(error) = os::poll(&mut pollfds, timeout) {
            // Fatal broker failure: stop owned groups before returning.
            for c in &clients {
                if let Some(j) = &c.job {
                    os::signal_group(j.pgid, libc::SIGKILL);
                }
            }
            return Err(error);
        }
        signals.drain();
        let existing = clients.len();
        for index in 0..existing {
            let events = pollfds[index + 2].revents;
            if events == 0 {
                continue;
            }
            if events & libc::POLLOUT != 0 {
                flush(&mut clients[index]);
            }
            if events & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
                && clients[index].connected
            {
                let result = read_frame(&mut clients[index]);
                match result {
                    Ok(Some(frame)) => handle(
                        frame,
                        index,
                        &mut clients,
                        &config,
                        &mut next_id,
                        &mut stopping,
                    ),
                    Ok(None) => {}
                    Err(_) => {
                        clients[index].connected = false;
                        clients[index].output.clear();
                        clients[index].stop(CANCELLED);
                    }
                }
            }
        }
        if !stopping && pollfds[0].revents & libc::POLLIN != 0 {
            // Bound accepts per iteration so a client flood cannot starve sampling.
            for _ in 0..MAX_CLIENTS {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if clients.len() >= MAX_CLIENTS {
                            continue;
                        }
                        stream.set_nonblocking(true)?;
                        clients.push(Client {
                            stream,
                            input: [0; SIZE],
                            read: 0,
                            output: VecDeque::new(),
                            written: 0,
                            job: None,
                            connected: true,
                            closing: false,
                            arrived: Instant::now(),
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

fn admit(clients: &mut [Client], config: &Config) {
    let (mut memory, mut slots, mut running) = (0u64, 0u64, 0usize);
    for job in clients.iter().filter_map(|c| c.job.as_ref()) {
        if job.phase != Phase::Queued {
            memory += job.memory;
            slots += job.slots;
            running += 1;
        }
    }
    // FIFO by submission ID, even when clients connected in another order.
    loop {
        let next = clients
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                c.job
                    .as_ref()
                    .filter(|j| j.phase == Phase::Queued)
                    .map(|j| (i, j.id))
            })
            .min_by_key(|(_, id)| *id);
        let Some((index, _)) = next else {
            break;
        };
        let job = clients[index].job.as_mut().unwrap();
        if running >= config.max_running
            || job.memory > config.memory - memory
            || job.slots > config.slots - slots
        {
            break;
        }
        memory += job.memory;
        slots += job.slots;
        running += 1;
        job.phase = Phase::Granted;
        job.since = Instant::now();
        let frame = [GRANT, job.id, job.slots, job.memory];
        clients[index].reply(frame);
    }
}

fn handle(
    frame: Frame,
    index: usize,
    clients: &mut [Client],
    config: &Config,
    next_id: &mut u64,
    stopping: &mut bool,
) {
    let [op, a, b, c] = frame;
    if clients[index].closing {
        return;
    }
    match op {
        SUBMIT if clients[index].job.is_none() && !*stopping => {
            let memory = if a == 0 { config.memory } else { a };
            let slots = if b == 0 { config.slots } else { b };
            if memory > config.memory || slots > config.slots || c > 86_400_000 {
                clients[index].reply([FAILED, BAD_REQUEST, 0, 0]);
                clients[index].closing = true;
                return;
            }
            let id = *next_id;
            *next_id += 1;
            clients[index].job = Some(Job {
                id,
                memory,
                slots,
                pgid: 0,
                phase: Phase::Queued,
                since: Instant::now(),
                timeout: Duration::from_millis(c),
                measured: 0,
                peak: 0,
                failure: 0,
            });
            clients[index].reply([QUEUED, id, 0, 0]);
        }
        START
            if clients[index]
                .job
                .as_ref()
                .is_some_and(|j| j.id == a && j.phase == Phase::Granted) =>
        {
            let pid = i32::try_from(b).unwrap_or(0);
            // SAFETY: getpgid only reads process metadata. Launchers make a new group.
            if pid <= 1 || unsafe { libc::getpgid(pid) } != pid {
                clients[index].stop(BAD_REQUEST);
                return;
            }
            let job = clients[index].job.as_mut().unwrap();
            job.pgid = pid;
            job.phase = Phase::Running;
            job.since = Instant::now();
            clients[index].reply([READY, a, 0, 0]);
        }
        FINISH if clients[index].job.as_ref().is_some_and(|j| j.id == a) => clients[index].stop(0),
        STATUS if clients[index].job.is_none() => {
            let count = clients.iter().filter(|c| c.job.is_some()).count();
            clients[index].reply([HEADER, config.memory, config.slots, count as u64]);
            for i in 0..clients.len() {
                if let Some(job) = &clients[i].job {
                    let row = [ROW, job.id, job.phase as u64, job.measured];
                    let detail = [DETAIL, job.id, job.peak, job.memory];
                    clients[index].reply(row);
                    clients[index].reply(detail);
                }
            }
            clients[index].reply([END, 0, 0, 0]);
            clients[index].closing = true;
        }
        CANCEL if clients[index].job.is_none() => {
            let found = clients
                .iter()
                .position(|c| c.job.as_ref().is_some_and(|j| j.id == a));
            if let Some(i) = found {
                clients[i].stop(CANCELLED);
            }
            clients[index].reply(if found.is_some() {
                [DONE, a, 0, 0]
            } else {
                [FAILED, BAD_REQUEST, a, 0]
            });
            clients[index].closing = true;
        }
        SHUTDOWN if clients[index].job.is_none() => {
            *stopping = true;
            clients[index].reply([DONE, 0, 0, 0]);
            clients[index].closing = true;
        }
        _ => {
            if clients[index].job.is_some() {
                clients[index].stop(BAD_REQUEST);
            } else {
                clients[index].reply([FAILED, BAD_REQUEST, 0, 0]);
                clients[index].closing = true;
            }
        }
    }
}

fn read_frame(client: &mut Client) -> io::Result<Option<Frame>> {
    match client.stream.read(&mut client.input[client.read..]) {
        Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
        Ok(count) => {
            client.read += count;
            if client.read == SIZE {
                client.read = 0;
                Ok(Some(wire::decode(&client.input)))
            } else {
                Ok(None)
            }
        }
        Err(error)
            if error.kind() == io::ErrorKind::WouldBlock
                || error.kind() == io::ErrorKind::Interrupted =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn flush(client: &mut Client) {
    while let Some(frame) = client.output.front() {
        match client.stream.write(&frame[client.written..]) {
            Ok(0) => break,
            Ok(count) => {
                client.written += count;
                if client.written == SIZE {
                    client.output.pop_front();
                    client.written = 0;
                }
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted =>
            {
                break;
            }
            Err(_) => {
                client.connected = false;
                client.output.clear();
                client.stop(CANCELLED);
                break;
            }
        }
    }
}
