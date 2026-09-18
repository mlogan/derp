//! Guest end of the run's launcher socket (`shared::COORD_FD`). Only the
//! baton holder may call in here, which is what keeps frames from
//! different processes from interleaving on the one shared stream.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::shared::{self, COORD_FD};

static CONNECTED: AtomicBool = AtomicBool::new(false);

/// Called once the process has joined a launcher's run.
pub fn connect() {
    let is_socket = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(COORD_FD, &raw mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFSOCK
    };
    CONNECTED.store(is_socket, Ordering::Relaxed);
}

pub fn connected() -> bool {
    CONNECTED.load(Ordering::Relaxed)
}

fn send(kind: u8, payload: &[u8]) -> bool {
    let mut frame = ((payload.len() + 5) as u32).to_le_bytes().to_vec();
    frame.push(kind);
    frame.extend(crate::sched::pid().to_le_bytes());
    frame.extend(payload);
    let mut sent = 0;
    while sent < frame.len() {
        let n = unsafe { libc::write(COORD_FD, frame[sent..].as_ptr().cast(), frame.len() - sent) };
        if n <= 0 {
            return false;
        }
        sent += n as usize;
    }
    true
}

fn read_exact(buf: &mut [u8]) -> bool {
    let mut got = 0;
    while got < buf.len() {
        let n = unsafe { libc::read(COORD_FD, buf[got..].as_mut_ptr().cast(), buf.len() - got) };
        if n <= 0 {
            return false;
        }
        got += n as usize;
    }
    true
}

/// Send a request and wait for the reply: the payload, or an errno.
fn call(kind: u8, payload: &[u8]) -> Result<Vec<u8>, i32> {
    if !connected() || !send(kind, payload) {
        return Err(libc::EIO);
    }
    let mut head = [0u8; 8];
    if !read_exact(&mut head) {
        return Err(libc::EIO);
    }
    let errno = i32::from_le_bytes(head[..4].try_into().unwrap());
    let mut body = vec![0u8; u32::from_le_bytes(head[4..].try_into().unwrap()) as usize];
    if !read_exact(&mut body) {
        return Err(libc::EIO);
    }
    if errno == 0 {
        Ok(body)
    } else {
        Err(errno)
    }
}

/// Path of the rewritten form of the program at `path`.
pub fn rewritten(path: &[u8]) -> Result<Vec<u8>, i32> {
    call(shared::MSG_SPAWN, path)
}

/// Tell the launcher which real pid process `child` of the run has, so it
/// can watch it. Returns once the watch is in place.
pub fn spawned(child: u32, real_pid: i32) {
    let mut payload = child.to_le_bytes().to_vec();
    payload.extend(real_pid.to_le_bytes());
    let _ = call(shared::MSG_SPAWNED, &payload);
}

pub fn report(text: &str) {
    if connected() {
        send(shared::MSG_REPORT, text.as_bytes());
    }
}
