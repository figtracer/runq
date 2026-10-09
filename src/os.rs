use std::{
    io,
    os::{fd::AsRawFd, unix::net::UnixStream},
    process::Command,
    sync::atomic::{AtomicI32, Ordering},
};

#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;

pub static SIGNAL: AtomicI32 = AtomicI32::new(0);
static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

pub struct Signals {
    read: UnixStream,
    _write: UnixStream,
}

impl AsRawFd for Signals {
    fn as_raw_fd(&self) -> i32 {
        self.read.as_raw_fd()
    }
}

impl Signals {
    pub fn drain(&self) {
        let mut bytes = [0u8; 128];
        // SAFETY: read writes at most the buffer length. The socket is nonblocking.
        while unsafe { libc::read(self.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) } > 0 {}
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        SIGNAL_FD.store(-1, Ordering::Relaxed);
    }
}

pub struct Usage {
    pub memory: u64,
    pub rss: u64,
    pub cpu_ns: u64,
}

// libmalloc reads these settings before main. One cache set without a nano
// zone uses less memory for our small, single-threaded processes. Re-exec once
// and restore the caller's settings before executing the workload.
#[cfg(target_os = "macos")]
const ALLOCATOR_READY: &str = "_RUNQ_ALLOCATOR_READY";
#[cfg(target_os = "macos")]
const ALLOCATOR_POLICY: [(&str, &str, &str); 2] = [
    (
        "MallocMaxMagazines",
        "1",
        "_RUNQ_ORIGINAL_MALLOC_MAX_MAGAZINES",
    ),
    ("MallocNanoZone", "0", "_RUNQ_ORIGINAL_MALLOC_NANO_ZONE"),
];

pub fn configure_allocator() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        if std::env::var_os(ALLOCATOR_READY).is_some()
            || ALLOCATOR_POLICY.iter().all(|&(key, value, _)| {
                std::env::var_os(key).is_some_and(|current| current == value)
            })
        {
            return Ok(());
        }
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(std::env::args_os().skip(1))
            .env(ALLOCATOR_READY, "1");
        for (key, value, original) in ALLOCATOR_POLICY {
            command.env(key, value);
            match std::env::var_os(key) {
                Some(value) => command.env(original, value),
                None => command.env_remove(original),
            };
        }
        Err(command.exec())
    }
    #[cfg(not(target_os = "macos"))]
    Ok(())
}

pub fn restore_workload_allocator(command: &mut Command) {
    #[cfg(target_os = "macos")]
    if std::env::var_os(ALLOCATOR_READY).is_some() {
        for (key, _, original) in ALLOCATOR_POLICY {
            match std::env::var_os(original) {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
            command.env_remove(original);
        }
        command.env_remove(ALLOCATOR_READY);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = command;
}

// One reusable 16 KiB buffer. Refuse to undercount a larger process group.
pub struct Sampler {
    #[cfg(target_os = "macos")]
    pids: [i32; 4096],
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            pids: [0; 4096],
        }
    }

    #[cfg(target_os = "macos")]
    pub fn group(&mut self, pgid: i32) -> io::Result<u64> {
        // SAFETY: the buffer is writable for the supplied byte count.
        let count = unsafe {
            libc::proc_listpids(
                2,
                pgid as u32,
                self.pids.as_mut_ptr().cast(),
                std::mem::size_of_val(self.pids.as_ref()) as i32,
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = count as usize / std::mem::size_of::<i32>();
        if count == 0 && group_exists(pgid) {
            return Err(io::Error::other("cannot enumerate a live process group"));
        }
        if count >= self.pids.len() {
            return Err(io::Error::other(
                "process group exceeds accounting capacity",
            ));
        }
        let mut total = 0u64;
        for &pid in &self.pids[..count] {
            if pid <= 0 {
                continue;
            }
            match usage(pid) {
                Ok(value) => total = total.saturating_add(value.memory),
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) || !process_exists(pid) => {
                }
                Err(error) => return Err(error),
            }
        }
        Ok(total)
    }

    #[cfg(target_os = "linux")]
    pub fn group(&mut self, pgid: i32) -> io::Result<u64> {
        let mut total = 0u64;
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
            else {
                continue;
            };
            // SAFETY: getpgid has no pointer arguments.
            if unsafe { libc::getpgid(pid) } != pgid {
                continue;
            }
            match usage(pid) {
                Ok(value) => total = total.saturating_add(value.memory),
                Err(_) if !process_exists(pid) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(total)
    }
}

#[cfg(target_os = "macos")]
pub fn usage(pid: i32) -> io::Result<Usage> {
    // SAFETY: rusage_info_v0 is a C POD; proc_pid_rusage expects a pointer to
    // the flavor-specific structure, despite its void** signature.
    let mut value: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    let result =
        unsafe { libc::proc_pid_rusage(pid, 0, (&mut value as *mut libc::rusage_info_v0).cast()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // rusage_info CPU counters are Mach ticks, not nanoseconds on Apple Silicon.
    static TIMEBASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    #[allow(
        deprecated,
        reason = "Use libc's existing stable Mach ABI binding without a second FFI crate"
    )]
    let &(numer, denom) = TIMEBASE.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: info is a valid writable timebase structure.
        unsafe {
            libc::mach_timebase_info(&mut info);
        }
        (info.numer, info.denom)
    });
    if denom == 0 {
        return Err(io::Error::other("mach_timebase_info failed"));
    }
    let cpu = u128::from(value.ri_user_time.saturating_add(value.ri_system_time))
        * u128::from(numer)
        / u128::from(denom);
    Ok(Usage {
        memory: value.ri_phys_footprint,
        rss: value.ri_resident_size,
        cpu_ns: cpu.min(u128::from(u64::MAX)) as u64,
    })
}

#[cfg(target_os = "linux")]
pub fn usage(pid: i32) -> io::Result<Usage> {
    use std::io::Read;
    let mut buf = [0u8; 4096];
    let mut file = std::fs::File::open(format!("/proc/{pid}/stat"))?;
    let len = file.read(&mut buf)?;
    let stat = std::str::from_utf8(&buf[..len]).map_err(io::Error::other)?;
    let (_, tail) = stat
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("invalid proc stat"))?;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let number = |index: usize| -> io::Result<u64> {
        fields
            .get(index)
            .ok_or_else(|| io::Error::other("short proc stat"))?
            .parse()
            .map_err(io::Error::other)
    };
    // SAFETY: sysconf has no pointer arguments.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if page <= 0 || ticks <= 0 {
        return Err(io::Error::other("sysconf failed"));
    }
    let rss = number(21)?.saturating_mul(page as u64);
    Ok(Usage {
        memory: rss,
        rss,
        cpu_ns: (number(11)? + number(12)?) * 1_000_000_000 / ticks as u64,
    })
}

pub fn process_exists(pid: i32) -> bool {
    // SAFETY: signal 0 only checks process existence/permission.
    unsafe {
        libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

pub fn group_exists(pgid: i32) -> bool {
    pgid > 1 && process_exists(-pgid)
}

pub fn signal_group(pgid: i32, signal: i32) {
    if pgid > 1 {
        // SAFETY: callers pass a registered, owned process group, never 0 or -1.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}

extern "C" fn on_signal(signal: i32) {
    if signal != libc::SIGCHLD {
        SIGNAL.store(signal, Ordering::Relaxed);
    }
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: errno belongs to this thread; write is async-signal-safe. A full
        // socket already contains a wakeup, so EAGAIN needs no retry.
        unsafe {
            #[cfg(target_os = "macos")]
            let errno = libc::__error();
            #[cfg(target_os = "linux")]
            let errno = libc::__errno_location();
            let saved = *errno;
            libc::write(fd, [1u8].as_ptr().cast(), 1);
            *errno = saved;
        }
    }
}

pub fn install_signals() -> io::Result<Signals> {
    let (read, write) = UnixStream::pair()?;
    read.set_nonblocking(true)?;
    write.set_nonblocking(true)?;
    SIGNAL_FD.store(write.as_raw_fd(), Ordering::Relaxed);
    let signals = Signals {
        read,
        _write: write,
    };
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGCHLD] {
        // SAFETY: the handler uses atomics and an async-signal-safe write.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_signal as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(signals)
}

pub fn poll(fds: &mut [libc::pollfd], timeout: i32) -> io::Result<()> {
    // SAFETY: fds contains the stated number of initialized pollfd structures.
    let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
    if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
