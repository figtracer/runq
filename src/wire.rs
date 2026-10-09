use std::io::{self, Read, Write};

// Fixed-size messages keep malformed clients from requesting unbounded buffers.
pub const SIZE: usize = 32;
pub const SUBMIT: u64 = 1;
pub const START: u64 = 2;
pub const FINISH: u64 = 3;
pub const STATUS: u64 = 4;
pub const CANCEL: u64 = 5;
pub const SHUTDOWN: u64 = 6;
pub const QUEUED: u64 = 10;
pub const GRANT: u64 = 11;
pub const READY: u64 = 12;
pub const DONE: u64 = 13;
pub const FAILED: u64 = 14;
pub const HEADER: u64 = 15;
pub const ROW: u64 = 16;
pub const DETAIL: u64 = 17;
pub const END: u64 = 18;

pub const OVER_MEMORY: u64 = 1;
pub const CANCELLED: u64 = 2;
pub const MONITOR_ERROR: u64 = 3;
pub const BAD_REQUEST: u64 = 4;
pub const STOPPING: u64 = 5;
pub const EXPIRED: u64 = 6;

pub type Frame = [u64; 4];

pub fn encode(frame: Frame) -> [u8; SIZE] {
    let mut bytes = [0; SIZE];
    for (part, value) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(frame) {
        part.copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

pub fn decode(bytes: &[u8; SIZE]) -> Frame {
    let mut frame = [0; 4];
    for (value, part) in frame.iter_mut().zip(bytes.as_chunks::<8>().0.iter()) {
        *value = u64::from_le_bytes(*part);
    }
    frame
}

pub fn send(stream: &mut impl Write, frame: Frame) -> io::Result<()> {
    stream.write_all(&encode(frame))
}

pub fn receive(stream: &mut impl Read) -> io::Result<Frame> {
    let mut bytes = [0; SIZE];
    stream.read_exact(&mut bytes)?;
    Ok(decode(&bytes))
}

pub fn reason(code: u64) -> &'static str {
    match code {
        OVER_MEMORY => "sampled memory budget exceeded",
        CANCELLED => "cancelled",
        MONITOR_ERROR => "memory accounting failed; stopping workload",
        BAD_REQUEST => "invalid request or request exceeds daemon capacity",
        STOPPING => "daemon shutting down",
        EXPIRED => "launch or command deadline exceeded",
        _ => "protocol failure",
    }
}
