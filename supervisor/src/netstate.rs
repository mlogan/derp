//! The virtual network, as state in the shared scheduler file. Sockets
//! between guests never touch the kernel's network stack: a connection is
//! two `Sock` records pointing at each other, each with a receive ring.
//! Because only the baton holder runs, every operation here is atomic with
//! respect to every guest, and nothing is asynchronous.
//!
//! Every byte between guests passes through `Net::deliver`, which is the
//! seam for a network simulator.
//!
//! A socket's descriptor in a guest is a real `AF_UNIX` socket used only as
//! a placeholder. Its `st_ino` identifies the virtual socket, so `dup`,
//! `fork`, `execve` and spawn file actions carry the identity along
//! without any bookkeeping here; what is tracked is how many descriptors
//! each process holds, to know when the last one goes.

pub const MAX_HOSTS: usize = 32;
pub const MAX_SOCKETS: usize = 256;
pub const MAX_FDREFS: usize = 1024;
pub const RING: usize = 64 * 1024;
pub const BACKLOG: usize = 32;
pub const NAME_MAX: usize = 64;
pub const UNIX_PATH_MAX: usize = 104;
/// Virtual subnet: host `i` is `10.0.0.(i + 1)`
pub const SUBNET: u32 = 0x0A00_0000;
pub const SUBNET_MASK: u32 = 0xFFFF_FF00;
pub const LOOPBACK: u32 = 0x7F00_0001;
const FIRST_EPHEMERAL: u16 = 49152;
pub const NO_SOCK: u32 = u32::MAX;

pub const FAMILY_NONE: u8 = 0;
pub const FAMILY_INET: u8 = 1;
pub const FAMILY_UNIX: u8 = 2;

pub const KIND_STREAM: u8 = 1;
pub const KIND_DGRAM: u8 = 2;

pub const S_FREE: u8 = 0;
pub const S_NEW: u8 = 1;
pub const S_BOUND: u8 = 2;
pub const S_LISTENING: u8 = 3;
pub const S_CONNECTED: u8 = 4;
/// Every descriptor is gone but the peer still refers to the record
pub const S_CLOSED: u8 = 5;

#[repr(C)]
pub struct Host {
    pub name: [u8; NAME_MAX],
    pub name_len: u8,
    /// Host byte order
    pub addr: u32,
    next_port: u16,
}

impl Host {
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addr {
    pub family: u8,
    pub path_len: u8,
    pub port: u16,
    /// Host byte order
    pub ip: u32,
    pub path: [u8; UNIX_PATH_MAX],
}

impl Addr {
    pub const NONE: Addr = Addr {
        family: FAMILY_NONE,
        path_len: 0,
        port: 0,
        ip: 0,
        path: [0; UNIX_PATH_MAX],
    };

    pub fn inet(ip: u32, port: u16) -> Addr {
        Addr {
            family: FAMILY_INET,
            ip,
            port,
            ..Addr::NONE
        }
    }

    pub fn unix(path: &[u8]) -> Addr {
        let mut a = Addr {
            family: FAMILY_UNIX,
            ..Addr::NONE
        };
        let n = path.len().min(UNIX_PATH_MAX);
        a.path[..n].copy_from_slice(&path[..n]);
        a.path_len = n as u8;
        a
    }

    pub fn path(&self) -> &[u8] {
        &self.path[..self.path_len as usize]
    }

    /// Same endpoint name within one host's tables
    fn same_name(&self, other: &Addr) -> bool {
        self.family == other.family
            && match self.family {
                FAMILY_INET => self.port == other.port,
                FAMILY_UNIX => self.path() == other.path(),
                _ => false,
            }
    }
}

#[repr(C)]
pub struct Sock {
    pub state: u8,
    pub kind: u8,
    pub family: u8,
    pub nonblocking: bool,
    /// The peer sent FIN (closed or shut down its sending side)
    pub fin: bool,
    /// We shut down our sending side
    pub shut_wr: bool,
    pub shut_rd: bool,
    /// Created by a `connect` and not yet returned by `accept`
    pub pending: bool,
    /// `SO_NOSIGPIPE`: a write to a closed peer is `EPIPE` without the signal
    pub nosigpipe: bool,
    /// A datagram socket with a default destination, which also filters
    /// what it receives
    pub has_peer: bool,
    /// `SO_RCVTIMEO` and `SO_SNDTIMEO` in nanoseconds of virtual time (0:
    /// wait forever)
    pub rcv_timeout_ns: u64,
    pub snd_timeout_ns: u64,
    pub host: u32,
    /// Open descriptors, over all processes
    pub refs: u32,
    pub far_end: u32,
    pub local: Addr,
    pub peer: Addr,
    backlog: [u32; BACKLOG],
    backlog_len: u32,
    backlog_max: u32,
    rx_head: u32,
    rx_len: u32,
    pub bytes_in: u64,
    /// Datagrams lost because the ring was full or nobody was bound
    pub dropped: u64,
    fl_head: u32,
    fl_len: u32,
    /// Stream bytes in flight to this socket; they hold part of its window
    fl_stream: u32,
    rx: [u8; RING],
    /// Payloads on their way here, oldest first
    flight: [u8; RING],
}

#[repr(C)]
pub struct FdRef {
    pub pid: u32,
    pub sock: u32,
    pub count: u32,
}

#[repr(C)]
pub struct Net {
    pub nhosts: u32,
    pub hosts: [Host; MAX_HOSTS],
    /// `st_ino` of each socket's placeholder descriptor (0: none yet).
    /// Kept apart from the records so that looking a descriptor up, which
    /// every `read` and `write` does, does not walk 64 KB strides.
    idents: [u64; MAX_SOCKETS],
    pub socks: [Sock; MAX_SOCKETS],
    fdrefs: [FdRef; MAX_FDREFS],
    /// Totals for the report
    /// The run's virtual clock, mirrored here by `State` whenever it moves
    pub now: u64,
    /// Delay between different hosts; traffic within a host is immediate
    pub latency_ns: u64,
    in_flight: u32,
    next_due: u64,
    pub connections: u64,
    pub datagrams: u64,
    pub dropped: u64,
    pub bytes: u64,
    /// Connections that left the virtual network for the kernel's
    pub passthrough: u64,
}

/// Largest datagram payload; with its header it must fit an empty ring
pub const MAX_DGRAM: usize = 60 * 1024;
/// In the ring a datagram is `len: u32, ip: u32, port: u16, family: u8,
/// path_len: u8`, the source path, then the payload.
const DGRAM_HEADER: usize = 12;

/// A record in a socket's flight ring: due time, kind, payload length
const FLIGHT_HEADER: usize = 13;
const K_BYTES: u8 = 1;
const K_DGRAM: u8 = 2;
const K_FIN: u8 = 3;

fn dgram_header(from: &Addr, data_len: usize) -> [u8; DGRAM_HEADER] {
    let mut header = [0u8; DGRAM_HEADER];
    header[..4].copy_from_slice(&(data_len as u32).to_le_bytes());
    header[4..8].copy_from_slice(&from.ip.to_le_bytes());
    header[8..10].copy_from_slice(&from.port.to_le_bytes());
    header[10] = from.family;
    header[11] = from.path_len;
    header
}

/// What travels between two sockets
enum Payload<'a> {
    Bytes(&'a [u8]),
    Datagram {
        from: &'a Addr,
        data: &'a [u8],
    },
    /// The sender closed or shut down its sending side
    Fin,
}

/// Why an operation cannot complete now
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// Not ready; park and retry, or `EAGAIN` for a non-blocking socket
    WouldBlock,
    Errno(i32),
}

use NetError::{Errno, WouldBlock};

impl Net {
    pub fn add_host(&mut self, name: &[u8]) -> u32 {
        let i = self.nhosts as usize;
        assert!(i < MAX_HOSTS, "too many hosts in one run");
        self.nhosts += 1;
        let h = &mut self.hosts[i];
        let n = name.len().min(NAME_MAX);
        h.name[..n].copy_from_slice(&name[..n]);
        h.name_len = n as u8;
        h.addr = SUBNET + i as u32 + 1;
        h.next_port = FIRST_EPHEMERAL;
        i as u32
    }

    pub fn host_by_name(&self, name: &[u8]) -> Option<u32> {
        (0..self.nhosts).find(|&i| self.hosts[i as usize].name() == name)
    }

    /// Which host an address names, seen from `from`: loopback never
    /// leaves the caller's host. `Err(true)` means an address in the
    /// virtual subnet nobody owns, `Err(false)` one outside it.
    pub fn host_for_ip(&self, from: u32, ip: u32) -> Result<u32, bool> {
        if ip >> 24 == 127 {
            return Ok(from);
        }
        if let Some(i) = (0..self.nhosts).find(|&i| self.hosts[i as usize].addr == ip) {
            return Ok(i);
        }
        Err(ip & SUBNET_MASK == SUBNET)
    }

    pub fn socket(&mut self, host: u32, family: u8, kind: u8) -> Result<u32, NetError> {
        let i = self
            .socks
            .iter()
            .position(|s| s.state == S_FREE)
            .ok_or(Errno(libc::ENFILE))?;
        self.idents[i] = 0;
        let s = &mut self.socks[i];
        s.state = S_NEW;
        s.kind = kind;
        s.family = family;
        s.nonblocking = false;
        s.fin = false;
        s.shut_wr = false;
        s.shut_rd = false;
        s.pending = false;
        s.nosigpipe = false;
        s.has_peer = false;
        s.rcv_timeout_ns = 0;
        s.snd_timeout_ns = 0;
        s.dropped = 0;
        s.host = host;
        s.refs = 0;
        s.far_end = NO_SOCK;
        s.local = Addr::NONE;
        s.peer = Addr::NONE;
        s.backlog_len = 0;
        s.backlog_max = 0;
        s.rx_head = 0;
        s.rx_len = 0;
        s.bytes_in = 0;
        s.fl_head = 0;
        s.fl_len = 0;
        s.fl_stream = 0;
        Ok(i as u32)
    }

    pub fn by_ident(&self, ident: u64) -> Option<u32> {
        if ident == 0 {
            return None;
        }
        self.idents
            .iter()
            .position(|&i| i == ident)
            .map(|i| i as u32)
    }

    pub fn set_ident(&mut self, sock: u32, ident: u64) {
        self.idents[sock as usize] = ident;
    }

    /// A name is taken by bound and listening sockets, not by the
    /// connections a listener accepted on the same port.
    fn name_taken(&self, host: u32, kind: u8, addr: &Addr) -> bool {
        self.socks.iter().any(|s| {
            matches!(s.state, S_BOUND | S_LISTENING)
                && s.host == host
                && s.kind == kind
                && s.local.same_name(addr)
        })
    }

    fn ephemeral_port(&mut self, host: u32, kind: u8) -> Result<u16, NetError> {
        for _ in 0..=(u16::MAX - FIRST_EPHEMERAL) {
            let h = &mut self.hosts[host as usize];
            let port = h.next_port;
            h.next_port = if port == u16::MAX {
                FIRST_EPHEMERAL
            } else {
                port + 1
            };
            if !self.name_taken(host, kind, &Addr::inet(0, port)) {
                return Ok(port);
            }
        }
        Err(Errno(libc::EADDRINUSE))
    }

    pub fn bind(&mut self, sock: u32, mut addr: Addr) -> Result<(), NetError> {
        let (host, kind, family, state) = {
            let s = &self.socks[sock as usize];
            (s.host, s.kind, s.family, s.state)
        };
        if state != S_NEW {
            return Err(Errno(libc::EINVAL));
        }
        if addr.family != family {
            return Err(Errno(libc::EAFNOSUPPORT));
        }
        if family == FAMILY_INET {
            let own = self.hosts[host as usize].addr;
            if addr.ip != 0 && addr.ip >> 24 != 127 && addr.ip != own {
                return Err(Errno(libc::EADDRNOTAVAIL));
            }
            if addr.port == 0 {
                addr.port = self.ephemeral_port(host, kind)?;
            }
        }
        if self.name_taken(host, kind, &addr) {
            return Err(Errno(libc::EADDRINUSE));
        }
        let s = &mut self.socks[sock as usize];
        s.local = addr;
        s.state = S_BOUND;
        Ok(())
    }

    pub fn listen(&mut self, sock: u32, backlog: i32) -> Result<(), NetError> {
        if self.socks[sock as usize].state == S_NEW {
            if self.socks[sock as usize].family != FAMILY_INET {
                return Err(Errno(libc::EDESTADDRREQ));
            }
            self.bind(sock, Addr::inet(0, 0))?;
        }
        let s = &mut self.socks[sock as usize];
        if !matches!(s.state, S_BOUND | S_LISTENING) {
            return Err(Errno(libc::EINVAL));
        }
        s.state = S_LISTENING;
        s.backlog_max = backlog.clamp(1, BACKLOG as i32) as u32;
        Ok(())
    }

    /// Connect `sock` to the listener `dest` names on `dest_host`. As with
    /// TCP, the connection is complete once it is queued on the listener;
    /// `accept` only hands out the far end.
    pub fn connect(&mut self, sock: u32, dest_host: u32, dest: &Addr) -> Result<(), NetError> {
        let (host, kind, family, state) = {
            let s = &self.socks[sock as usize];
            (s.host, s.kind, s.family, s.state)
        };
        match state {
            S_NEW | S_BOUND => {}
            S_CONNECTED => return Err(Errno(libc::EISCONN)),
            _ => return Err(Errno(libc::EINVAL)),
        }
        let listener = self.socks.iter().position(|l| {
            l.state == S_LISTENING
                && l.host == dest_host
                && l.kind == kind
                && l.local.same_name(dest)
                && (family != FAMILY_INET
                    || l.local.ip == 0
                    || l.local.ip >> 24 == 127 && dest.ip >> 24 == 127
                    || l.local.ip == dest.ip)
        });
        let Some(listener) = listener else {
            return Err(Errno(libc::ECONNREFUSED));
        };
        if self.socks[listener].backlog_len >= self.socks[listener].backlog_max {
            return Err(Errno(libc::ECONNREFUSED));
        }
        if state == S_NEW && family == FAMILY_INET {
            let port = self.ephemeral_port(host, kind)?;
            let ip = if dest.ip >> 24 == 127 {
                LOOPBACK
            } else {
                self.hosts[host as usize].addr
            };
            self.socks[sock as usize].local = Addr::inet(ip, port);
        }
        let far = self.socket(dest_host, family, kind)?;
        let near_local = self.socks[sock as usize].local;
        let mut far_local = self.socks[listener].local;
        if family == FAMILY_INET {
            far_local.ip = dest.ip;
        }
        {
            let f = &mut self.socks[far as usize];
            f.state = S_CONNECTED;
            f.pending = true;
            f.local = far_local;
            f.peer = near_local;
            f.far_end = sock;
        }
        {
            let n = &mut self.socks[sock as usize];
            n.state = S_CONNECTED;
            n.peer = far_local;
            n.far_end = far;
        }
        let l = &mut self.socks[listener];
        l.backlog[l.backlog_len as usize] = far;
        l.backlog_len += 1;
        self.connections += 1;
        Ok(())
    }

    /// Oldest queued connection of a listener.
    pub fn accept(&mut self, listener: u32) -> Result<u32, NetError> {
        let l = &mut self.socks[listener as usize];
        if l.state != S_LISTENING {
            return Err(Errno(libc::EINVAL));
        }
        if l.backlog_len == 0 {
            return Err(WouldBlock);
        }
        let far = l.backlog[0];
        l.backlog.copy_within(1..l.backlog_len as usize, 0);
        l.backlog_len -= 1;
        self.socks[far as usize].pending = false;
        Ok(far)
    }

    pub fn readable(&self, sock: u32) -> bool {
        let s = &self.socks[sock as usize];
        match (s.kind, s.state) {
            (KIND_DGRAM, _) => s.rx_len > 0,
            (_, S_LISTENING) => s.backlog_len > 0,
            (_, S_CONNECTED) => s.rx_len > 0 || s.fin || s.shut_rd,
            _ => true,
        }
    }

    pub fn writable(&self, sock: u32) -> bool {
        let s = &self.socks[sock as usize];
        if s.state != S_CONNECTED || s.shut_wr {
            return true;
        }
        let p = &self.socks[s.far_end as usize];
        p.state != S_CONNECTED || p.shut_rd || self.window(s.far_end) > 0
    }

    /// Stream bytes `dst` can take now: what its receive ring has left
    /// after the bytes already in flight to it, and what the flight ring
    /// can hold while keeping room for a FIN's header.
    fn window(&self, dst: u32) -> usize {
        let d = &self.socks[dst as usize];
        let window = RING - d.rx_len as usize - d.fl_stream as usize;
        if d.fl_len == 0 {
            return window;
        }
        window.min((RING - d.fl_len as usize).saturating_sub(2 * FLIGHT_HEADER))
    }

    fn ring_push(&mut self, sock: u32, bytes: &[u8]) {
        let d = &mut self.socks[sock as usize];
        let mut at = (d.rx_head as usize + d.rx_len as usize) % RING;
        for &b in bytes {
            d.rx[at] = b;
            at = (at + 1) % RING;
        }
        d.rx_len += bytes.len() as u32;
    }

    /// Copy from `offset` bytes into the ring's contents without consuming.
    fn ring_peek(&self, sock: u32, offset: usize, out: &mut [u8]) {
        let d = &self.socks[sock as usize];
        let mut at = (d.rx_head as usize + offset) % RING;
        for b in out {
            *b = d.rx[at];
            at = (at + 1) % RING;
        }
    }

    fn ring_skip(&mut self, sock: u32, n: usize) {
        let d = &mut self.socks[sock as usize];
        d.rx_head = ((d.rx_head as usize + n) % RING) as u32;
        d.rx_len -= n as u32;
    }

    /// The single entry point for traffic between guests. Returns how many
    /// payload bytes were taken: a stream takes what fits in the receiver's
    /// window, a datagram is all or nothing.
    ///
    /// This is the seam for a network simulator. The policy here is a
    /// fixed latency between different hosts and none within one; a
    /// simulator would choose `deliver_at` from the two hosts and the
    /// clock, or not launch the payload at all.
    fn deliver(&mut self, src_host: u32, dst: u32, payload: Payload) -> usize {
        let d = &self.socks[dst as usize];
        let delay = if d.host == src_host {
            0
        } else {
            self.latency_ns
        };
        // Nothing may overtake what is already in flight to this socket
        if delay == 0 && d.fl_len == 0 {
            return self.arrive(dst, payload);
        }
        self.launch(dst, self.now + delay, payload)
    }

    /// The payload reaches `dst` now.
    fn arrive(&mut self, dst: u32, payload: Payload) -> usize {
        let free = RING - self.socks[dst as usize].rx_len as usize;
        let taken = match payload {
            Payload::Fin => {
                self.socks[dst as usize].fin = true;
                return 0;
            }
            Payload::Bytes(bytes) => {
                let n = bytes.len().min(free);
                self.ring_push(dst, &bytes[..n]);
                n
            }
            Payload::Datagram { from, data } => {
                let path = from.path();
                if DGRAM_HEADER + path.len() + data.len() > free {
                    return 0;
                }
                self.ring_push(dst, &dgram_header(from, data.len()));
                self.ring_push(dst, path);
                self.ring_push(dst, data);
                data.len()
            }
        };
        self.socks[dst as usize].bytes_in += taken as u64;
        self.bytes += taken as u64;
        taken
    }

    fn flight_push(&mut self, sock: u32, bytes: &[u8]) {
        let d = &mut self.socks[sock as usize];
        let mut at = (d.fl_head as usize + d.fl_len as usize) % RING;
        for &b in bytes {
            d.flight[at] = b;
            at = (at + 1) % RING;
        }
        d.fl_len += bytes.len() as u32;
    }

    fn flight_peek(&self, sock: u32, offset: usize, out: &mut [u8]) {
        let d = &self.socks[sock as usize];
        let mut at = (d.fl_head as usize + offset) % RING;
        for b in out {
            *b = d.flight[at];
            at = (at + 1) % RING;
        }
    }

    fn flight_skip(&mut self, sock: u32, n: usize) {
        let d = &mut self.socks[sock as usize];
        d.fl_head = ((d.fl_head as usize + n) % RING) as u32;
        d.fl_len -= n as u32;
    }

    /// Put the payload in flight to `dst`, to arrive at `deliver_at`. In
    /// flight a record is `deliver_at: u64, kind: u8, len: u32` and the
    /// payload as it will sit in the receive ring. Stream bytes in flight
    /// count against the receiver's window, so what is launched always
    /// fits on arrival; room for one more header is kept so a FIN can
    /// always follow.
    fn launch(&mut self, dst: u32, deliver_at: u64, payload: Payload) -> usize {
        let room =
            (RING - self.socks[dst as usize].fl_len as usize).saturating_sub(2 * FLIGHT_HEADER);
        let (kind, len, taken) = match &payload {
            Payload::Fin => (K_FIN, 0, 0),
            Payload::Bytes(bytes) => {
                let n = bytes.len().min(self.window(dst)).min(room);
                if n == 0 {
                    return 0;
                }
                (K_BYTES, n, n)
            }
            Payload::Datagram { from, data } => {
                let len = DGRAM_HEADER + from.path().len() + data.len();
                if len > room {
                    return 0;
                }
                (K_DGRAM, len, data.len())
            }
        };
        let mut header = [0u8; FLIGHT_HEADER];
        header[..8].copy_from_slice(&deliver_at.to_le_bytes());
        header[8] = kind;
        header[9..].copy_from_slice(&(len as u32).to_le_bytes());
        self.flight_push(dst, &header);
        match payload {
            Payload::Fin => {}
            Payload::Bytes(bytes) => {
                self.flight_push(dst, &bytes[..len]);
                self.socks[dst as usize].fl_stream += len as u32;
            }
            Payload::Datagram { from, data } => {
                self.flight_push(dst, &dgram_header(from, data.len()));
                self.flight_push(dst, from.path());
                self.flight_push(dst, data);
            }
        }
        self.in_flight += 1;
        self.next_due = if self.in_flight == 1 {
            deliver_at
        } else {
            self.next_due.min(deliver_at)
        };
        taken
    }

    /// When the next payload in flight is due, if any is
    pub fn next_due(&self) -> Option<u64> {
        (self.in_flight > 0).then_some(self.next_due)
    }

    /// Move `n` bytes from the head of `sock`'s flight ring to its receive
    /// ring, without allocating.
    fn land(&mut self, sock: u32, mut n: usize) {
        let mut chunk = [0u8; 256];
        while n > 0 {
            let step = n.min(chunk.len());
            self.flight_peek(sock, 0, &mut chunk[..step]);
            self.flight_skip(sock, step);
            self.ring_push(sock, &chunk[..step]);
            n -= step;
        }
    }

    /// The clock is now `now`: everything due arrives. Returns whether
    /// anything did, in which case I/O waiters should look again.
    pub fn advance(&mut self, now: u64) -> bool {
        self.now = now;
        if self.in_flight == 0 || self.next_due > now {
            return false;
        }
        let mut next = u64::MAX;
        for sock in 0..MAX_SOCKETS as u32 {
            while self.socks[sock as usize].fl_len > 0 {
                let mut header = [0u8; FLIGHT_HEADER];
                self.flight_peek(sock, 0, &mut header);
                let due = u64::from_le_bytes(header[..8].try_into().unwrap());
                if due > now {
                    next = next.min(due);
                    break;
                }
                let len = u32::from_le_bytes(header[9..].try_into().unwrap()) as usize;
                self.flight_skip(sock, FLIGHT_HEADER);
                self.in_flight -= 1;
                match header[8] {
                    K_FIN => self.socks[sock as usize].fin = true,
                    K_BYTES => {
                        self.land(sock, len);
                        let d = &mut self.socks[sock as usize];
                        d.fl_stream -= len as u32;
                        d.bytes_in += len as u64;
                        self.bytes += len as u64;
                    }
                    _ => {
                        let mut data_len = [0u8; 4];
                        self.flight_peek(sock, 0, &mut data_len);
                        let data_len = u64::from(u32::from_le_bytes(data_len));
                        if len <= RING - self.socks[sock as usize].rx_len as usize {
                            self.land(sock, len);
                            self.socks[sock as usize].bytes_in += data_len;
                            self.bytes += data_len;
                        } else {
                            self.flight_skip(sock, len);
                            self.dropped += 1;
                        }
                    }
                }
            }
        }
        self.next_due = next;
        true
    }

    /// Send on a stream. Returns how many bytes were taken; `WouldBlock`
    /// when the peer's ring is full, which is the backpressure.
    pub fn send(&mut self, sock: u32, bytes: &[u8]) -> Result<usize, NetError> {
        let s = &self.socks[sock as usize];
        if s.state != S_CONNECTED {
            return Err(Errno(libc::ENOTCONN));
        }
        if s.shut_wr {
            return Err(Errno(libc::EPIPE));
        }
        let (host, peer) = (s.host, s.far_end);
        let p = &self.socks[peer as usize];
        if p.state != S_CONNECTED || p.shut_rd {
            return Err(Errno(libc::EPIPE));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        match self.deliver(host, peer, Payload::Bytes(bytes)) {
            0 => Err(WouldBlock),
            n => Ok(n),
        }
    }

    /// Receive from a stream; 0 is end of stream.
    pub fn recv(&mut self, sock: u32, out: &mut [u8], peek: bool) -> Result<usize, NetError> {
        let s = &self.socks[sock as usize];
        if s.state != S_CONNECTED {
            return Err(Errno(libc::ENOTCONN));
        }
        if s.rx_len == 0 {
            return if s.fin || s.shut_rd || out.is_empty() {
                Ok(0)
            } else {
                Err(WouldBlock)
            };
        }
        let n = out.len().min(s.rx_len as usize);
        self.ring_peek(sock, 0, &mut out[..n]);
        if !peek {
            self.ring_skip(sock, n);
        }
        Ok(n)
    }

    /// Bytes `FIONREAD` reports: everything buffered on a stream, the
    /// first datagram's payload on a datagram socket.
    pub fn pending_bytes(&self, sock: u32) -> usize {
        let s = &self.socks[sock as usize];
        if s.kind != KIND_DGRAM || s.rx_len == 0 {
            return s.rx_len as usize;
        }
        let mut len = [0u8; 4];
        self.ring_peek(sock, 0, &mut len);
        u32::from_le_bytes(len) as usize
    }

    /// Give a datagram socket a default destination. It then only
    /// receives from that address.
    pub fn connect_dgram(&mut self, sock: u32, dest: Addr) -> Result<(), NetError> {
        self.bind_if_new(sock)?;
        let s = &mut self.socks[sock as usize];
        s.peer = dest;
        s.has_peer = true;
        Ok(())
    }

    /// An unbound inet datagram socket gets a port when it first sends, so
    /// replies can find it. UNIX-domain senders may stay nameless.
    fn bind_if_new(&mut self, sock: u32) -> Result<(), NetError> {
        let s = &self.socks[sock as usize];
        if s.state == S_NEW && s.family == FAMILY_INET {
            self.bind(sock, Addr::inet(0, 0))?;
        }
        Ok(())
    }

    /// Send one datagram to `dest` on `dest_host` (or to the default
    /// destination). An inet datagram nobody is bound to receive, or that
    /// does not fit, is lost, as UDP allows; a UNIX-domain one reports it.
    pub fn send_dgram(
        &mut self,
        sock: u32,
        dest: Option<(u32, Addr)>,
        data: &[u8],
    ) -> Result<usize, NetError> {
        if data.len() > MAX_DGRAM {
            return Err(Errno(libc::EMSGSIZE));
        }
        self.bind_if_new(sock)?;
        let s = &self.socks[sock as usize];
        let (host, family) = (s.host, s.family);
        let (dest_host, dest) = match dest {
            Some(d) => d,
            None if s.has_peer => {
                let host = match self.host_for_ip(host, s.peer.ip) {
                    Ok(h) if family == FAMILY_INET => h,
                    _ => host,
                };
                (host, s.peer)
            }
            None => return Err(Errno(libc::EDESTADDRREQ)),
        };
        // What the receiver sees as the source: our name, with the address
        // the destination would reply to
        let mut from = s.local;
        if family == FAMILY_INET {
            from.family = FAMILY_INET;
            from.ip = if dest.ip >> 24 == 127 {
                LOOPBACK
            } else {
                self.hosts[host as usize].addr
            };
        }
        let target = self.socks.iter().position(|t| {
            t.kind == KIND_DGRAM
                && t.state == S_BOUND
                && t.host == dest_host
                && t.local.same_name(&dest)
                && (!t.has_peer || t.peer.same_name(&from) && t.peer.ip == from.ip)
        });
        let taken = match target {
            Some(t) => {
                let payload = Payload::Datagram { from: &from, data };
                match self.deliver(host, t as u32, payload) {
                    0 if !data.is_empty() => None,
                    _ => Some(()),
                }
            }
            None if family == FAMILY_UNIX => return Err(Errno(libc::ECONNREFUSED)),
            None => None,
        };
        if taken.is_none() {
            if family == FAMILY_UNIX {
                return Err(WouldBlock);
            }
            self.socks[sock as usize].dropped += 1;
            self.dropped += 1;
        }
        self.datagrams += 1;
        Ok(data.len())
    }

    /// Receive one datagram and its source. What does not fit in `out` is
    /// discarded with the datagram.
    pub fn recv_dgram(
        &mut self,
        sock: u32,
        out: &mut [u8],
        peek: bool,
    ) -> Result<(usize, Addr), NetError> {
        if self.socks[sock as usize].rx_len == 0 {
            return Err(WouldBlock);
        }
        let mut header = [0u8; DGRAM_HEADER];
        self.ring_peek(sock, 0, &mut header);
        let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let mut from = Addr::NONE;
        from.ip = u32::from_le_bytes(header[4..8].try_into().unwrap());
        from.port = u16::from_le_bytes(header[8..10].try_into().unwrap());
        from.family = header[10];
        from.path_len = header[11];
        let path_len = from.path_len as usize;
        self.ring_peek(sock, DGRAM_HEADER, &mut from.path[..path_len]);
        let n = out.len().min(len);
        self.ring_peek(sock, DGRAM_HEADER + path_len, &mut out[..n]);
        if !peek {
            self.ring_skip(sock, DGRAM_HEADER + path_len + len);
        }
        Ok((n, from))
    }

    pub fn shutdown(&mut self, sock: u32, read: bool, write: bool) -> Result<(), NetError> {
        let s = &mut self.socks[sock as usize];
        if s.state != S_CONNECTED {
            return Err(Errno(libc::ENOTCONN));
        }
        if read {
            s.shut_rd = true;
            s.rx_len = 0;
        }
        if write && !s.shut_wr {
            s.shut_wr = true;
            let (host, peer) = (s.host, s.far_end);
            self.deliver(host, peer, Payload::Fin);
        }
        Ok(())
    }

    /// The last descriptor of `sock` is gone.
    fn release(&mut self, sock: u32) {
        let state = self.socks[sock as usize].state;
        match state {
            S_LISTENING => {
                // Queued connections are reset
                let n = self.socks[sock as usize].backlog_len as usize;
                for i in 0..n {
                    let far = self.socks[sock as usize].backlog[i];
                    let near = self.socks[far as usize].far_end;
                    if self.socks[near as usize].state == S_CLOSED {
                        self.socks[near as usize].state = S_FREE;
                        self.socks[far as usize].state = S_FREE;
                    } else {
                        let host = self.socks[far as usize].host;
                        self.deliver(host, near, Payload::Fin);
                        self.socks[far as usize].state = S_CLOSED;
                    }
                }
                self.socks[sock as usize].state = S_FREE;
            }
            S_CONNECTED => {
                let peer = self.socks[sock as usize].far_end;
                if self.socks[peer as usize].state == S_CLOSED {
                    self.socks[peer as usize].state = S_FREE;
                    self.socks[sock as usize].state = S_FREE;
                } else {
                    let host = self.socks[sock as usize].host;
                    self.deliver(host, peer, Payload::Fin);
                    self.socks[sock as usize].state = S_CLOSED;
                }
            }
            _ => self.socks[sock as usize].state = S_FREE,
        }
        self.idents[sock as usize] = 0;
    }

    fn fdref(&mut self, pid: u32, sock: u32) -> &mut FdRef {
        let at = self
            .fdrefs
            .iter()
            .position(|r| r.count > 0 && r.pid == pid && r.sock == sock)
            .or_else(|| self.fdrefs.iter().position(|r| r.count == 0))
            .expect("too many socket references in one run");
        let r = &mut self.fdrefs[at];
        if r.count == 0 {
            r.pid = pid;
            r.sock = sock;
        }
        r
    }

    /// Process `pid` gained a descriptor for `sock`.
    pub fn add_ref(&mut self, pid: u32, sock: u32) {
        self.fdref(pid, sock).count += 1;
        self.socks[sock as usize].refs += 1;
    }

    /// Process `pid` closed a descriptor for `sock`.
    pub fn drop_ref(&mut self, pid: u32, sock: u32) {
        let r = self.fdref(pid, sock);
        r.count = r.count.saturating_sub(1);
        let s = &mut self.socks[sock as usize];
        s.refs = s.refs.saturating_sub(1);
        if s.refs == 0 {
            self.release(sock);
        }
    }

    /// Replace what is recorded for `pid` by `held`, one entry per open
    /// descriptor: a new process counting what it inherited, or a new
    /// image after `execve` dropped the close-on-exec ones. Additions come
    /// first so a socket that stays open never looks released in between.
    pub fn set_refs(&mut self, pid: u32, held: &[u32]) {
        let old: [u32; MAX_SOCKETS] = std::array::from_fn(|sock| {
            self.fdrefs
                .iter()
                .find(|r| r.count > 0 && r.pid == pid && r.sock == sock as u32)
                .map_or(0, |r| r.count)
        });
        for &sock in held {
            self.add_ref(pid, sock);
        }
        for (sock, &count) in old.iter().enumerate() {
            for _ in 0..count {
                self.drop_ref(pid, sock as u32);
            }
        }
    }

    /// Process `pid` is gone and the kernel closed its descriptors.
    pub fn process_died(&mut self, pid: u32) {
        self.set_refs(pid, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zeroed `Net` on the heap: it is 17 MB, and zero is its initial
    /// state in the shared file too.
    fn net() -> Box<Net> {
        let mut n = unsafe { Box::<Net>::new_zeroed().assume_init() };
        n.add_host(b"alpha");
        n.add_host(b"beta");
        n
    }

    fn listener(n: &mut Net, host: u32, port: u16) -> u32 {
        let l = n.socket(host, FAMILY_INET, KIND_STREAM).unwrap();
        n.add_ref(host, l);
        n.bind(l, Addr::inet(0, port)).unwrap();
        n.listen(l, 8).unwrap();
        l
    }

    fn client(n: &mut Net, host: u32) -> u32 {
        let c = n.socket(host, FAMILY_INET, KIND_STREAM).unwrap();
        n.add_ref(host, c);
        c
    }

    #[test]
    fn connect_accept_and_echo_across_hosts() {
        let mut n = net();
        let l = listener(&mut n, 0, 80);
        let c = client(&mut n, 1);
        let dest = Addr::inet(n.hosts[0].addr, 80);
        assert_eq!(n.accept(l), Err(WouldBlock));
        n.connect(c, 0, &dest).unwrap();
        // Data sent before accept waits in the far end's ring
        assert_eq!(n.send(c, b"hello"), Ok(5));
        let far = n.accept(l).unwrap();
        n.add_ref(0, far);
        let mut buf = [0u8; 16];
        assert_eq!(n.recv(far, &mut buf, true), Ok(5));
        assert_eq!(n.recv(far, &mut buf, false), Ok(5));
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(n.recv(far, &mut buf, false), Err(WouldBlock));
        assert_eq!(n.socks[far as usize].peer.ip, n.hosts[1].addr);
        assert_eq!(n.socks[c as usize].peer.port, 80);
        assert!(n.socks[c as usize].local.port >= FIRST_EPHEMERAL);
        assert_eq!((n.connections, n.bytes), (1, 5));
    }

    #[test]
    fn a_full_ring_pushes_back_and_wraps() {
        let mut n = net();
        let l = listener(&mut n, 0, 80);
        let c = client(&mut n, 0);
        n.connect(c, 0, &Addr::inet(LOOPBACK, 80)).unwrap();
        let far = n.accept(l).unwrap();
        n.add_ref(0, far);
        let block = vec![7u8; RING - 10];
        assert_eq!(n.send(c, &block), Ok(RING - 10));
        assert_eq!(n.send(c, &[1; 100]), Ok(10));
        assert!(!n.writable(c));
        assert_eq!(n.send(c, &[1; 100]), Err(WouldBlock));
        let mut buf = vec![0u8; RING];
        assert_eq!(n.recv(far, &mut buf[..1000], false), Ok(1000));
        assert_eq!(n.send(c, &[9; 2000]), Ok(1000));
        assert_eq!(n.recv(far, &mut buf, false), Ok(RING));
        assert_eq!(
            &buf[RING - 1001..],
            &[1u8; 1]
                .iter()
                .chain([9u8; 1000].iter())
                .copied()
                .collect::<Vec<_>>()[..]
        );
    }

    #[test]
    fn close_is_eof_for_the_reader_and_epipe_for_the_writer() {
        let mut n = net();
        let l = listener(&mut n, 0, 80);
        let c = client(&mut n, 1);
        n.connect(c, 0, &Addr::inet(n.hosts[0].addr, 80)).unwrap();
        let far = n.accept(l).unwrap();
        n.add_ref(0, far);
        n.send(c, b"bye").unwrap();
        n.drop_ref(1, c);
        let mut buf = [0u8; 8];
        assert_eq!(n.recv(far, &mut buf, false), Ok(3));
        assert_eq!(n.recv(far, &mut buf, false), Ok(0));
        assert_eq!(n.send(far, b"x"), Err(Errno(libc::EPIPE)));
        n.drop_ref(0, far);
        assert_eq!(n.socks[c as usize].state, S_FREE);
        assert_eq!(n.socks[far as usize].state, S_FREE);
    }

    #[test]
    fn half_close_lets_the_other_direction_continue() {
        let mut n = net();
        let l = listener(&mut n, 0, 80);
        let c = client(&mut n, 0);
        n.connect(c, 0, &Addr::inet(LOOPBACK, 80)).unwrap();
        let far = n.accept(l).unwrap();
        n.add_ref(0, far);
        n.shutdown(c, false, true).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(n.recv(far, &mut buf, false), Ok(0));
        assert_eq!(n.send(far, b"late"), Ok(4));
        assert_eq!(n.recv(c, &mut buf, false), Ok(4));
        assert_eq!(n.send(c, b"x"), Err(Errno(libc::EPIPE)));
    }

    #[test]
    fn ports_and_names_are_per_host_and_loopback_stays_home() {
        let mut n = net();
        listener(&mut n, 0, 80);
        listener(&mut n, 1, 80);
        let again = n.socket(0, FAMILY_INET, KIND_STREAM).unwrap();
        assert_eq!(
            n.bind(again, Addr::inet(0, 80)),
            Err(Errno(libc::EADDRINUSE))
        );
        let other = n.hosts[1].addr;
        assert_eq!(
            n.bind(again, Addr::inet(other, 81)),
            Err(Errno(libc::EADDRNOTAVAIL))
        );

        assert_eq!(n.host_for_ip(1, LOOPBACK), Ok(1));
        assert_eq!(n.host_for_ip(1, n.hosts[0].addr), Ok(0));
        assert_eq!(n.host_for_ip(1, SUBNET + 77), Err(true));
        assert_eq!(n.host_for_ip(1, 0x0808_0808), Err(false));

        // Nothing listens on beta's port 81; alpha's loopback is not beta's
        let c = client(&mut n, 1);
        assert_eq!(
            n.connect(c, 1, &Addr::inet(LOOPBACK, 81)),
            Err(Errno(libc::ECONNREFUSED))
        );

        let u0 = n.socket(0, FAMILY_UNIX, KIND_STREAM).unwrap();
        let u1 = n.socket(1, FAMILY_UNIX, KIND_STREAM).unwrap();
        n.bind(u0, Addr::unix(b"/tmp/sock")).unwrap();
        n.bind(u1, Addr::unix(b"/tmp/sock")).unwrap();
    }

    #[test]
    fn a_dead_process_releases_what_it_held() {
        let mut n = net();
        let l = listener(&mut n, 0, 80);
        let c = client(&mut n, 1);
        n.connect(c, 0, &Addr::inet(n.hosts[0].addr, 80)).unwrap();
        let far = n.accept(l).unwrap();
        n.add_ref(0, far);
        // A forked worker (process 5) inherits the connection; the server
        // closes its own copy.
        n.set_refs(5, &[far, l]);
        n.drop_ref(0, far);
        assert!(!n.socks[c as usize].fin);
        n.process_died(5);
        assert!(n.socks[c as usize].fin);
        assert_eq!(n.socks[l as usize].state, S_LISTENING);
        assert_eq!(n.by_ident(0), None);
    }

    fn dgram(n: &mut Net, host: u32, port: Option<u16>) -> u32 {
        let s = n.socket(host, FAMILY_INET, KIND_DGRAM).unwrap();
        n.add_ref(host, s);
        if let Some(port) = port {
            n.bind(s, Addr::inet(0, port)).unwrap();
        }
        s
    }

    #[test]
    fn datagrams_keep_their_boundaries_and_sources() {
        let mut n = net();
        let server = dgram(&mut n, 0, Some(9000));
        let client = dgram(&mut n, 1, None);
        let to = (0, Addr::inet(n.hosts[0].addr, 9000));
        let mut buf = [0u8; 64];
        assert_eq!(n.recv_dgram(server, &mut buf, false), Err(WouldBlock));
        assert!(!n.readable(server));
        assert_eq!(n.send_dgram(client, Some(to), b"one"), Ok(3));
        assert_eq!(n.send_dgram(client, Some(to), b""), Ok(0));
        assert_eq!(n.send_dgram(client, Some(to), b"three!"), Ok(6));
        assert_eq!(n.pending_bytes(server), 3);

        let (len, from) = n.recv_dgram(server, &mut buf, true).unwrap();
        assert_eq!((len, &buf[..3]), (3, &b"one"[..]));
        let (len, from2) = n.recv_dgram(server, &mut buf, false).unwrap();
        assert_eq!(len, 3);
        assert_eq!((from.ip, from.port), (from2.ip, from2.port));
        // The sender was bound on first use, to its own host's address
        assert_eq!(from.ip, n.hosts[1].addr);
        assert!(from.port >= FIRST_EPHEMERAL);
        assert_eq!(n.recv_dgram(server, &mut buf, false).unwrap().0, 0);
        // A short buffer truncates and the rest of the datagram is gone
        assert_eq!(n.recv_dgram(server, &mut buf[..2], false).unwrap().0, 2);
        assert_eq!(n.recv_dgram(server, &mut buf, false), Err(WouldBlock));

        // The reply finds the client by the source address
        assert_eq!(n.send_dgram(server, Some((1, from)), b"pong"), Ok(4));
        let (len, back) = n.recv_dgram(client, &mut buf, false).unwrap();
        assert_eq!((len, back.port, back.ip), (4, 9000, n.hosts[0].addr));
    }

    #[test]
    fn lost_datagrams_are_counted_not_reported() {
        let mut n = net();
        let client = dgram(&mut n, 1, None);
        let nowhere = (0, Addr::inet(n.hosts[0].addr, 9));
        assert_eq!(n.send_dgram(client, Some(nowhere), b"x"), Ok(1));
        assert_eq!(n.dropped, 1);

        let server = dgram(&mut n, 0, Some(9000));
        let to = (0, Addr::inet(n.hosts[0].addr, 9000));
        let big = vec![1u8; MAX_DGRAM];
        assert_eq!(n.send_dgram(client, Some(to), &big), Ok(MAX_DGRAM));
        assert_eq!(n.send_dgram(client, Some(to), &big), Ok(MAX_DGRAM));
        assert_eq!(n.dropped, 2);
        assert_eq!(
            n.send_dgram(client, Some(to), &vec![0; MAX_DGRAM + 1]),
            Err(Errno(libc::EMSGSIZE))
        );
        let mut buf = vec![0u8; MAX_DGRAM];
        assert_eq!(n.recv_dgram(server, &mut buf, false).unwrap().0, MAX_DGRAM);
        assert_eq!(n.recv_dgram(server, &mut buf, false), Err(WouldBlock));
    }

    #[test]
    fn a_connected_datagram_socket_has_a_default_peer_and_a_filter() {
        let mut n = net();
        let server = dgram(&mut n, 0, Some(9000));
        let client = dgram(&mut n, 1, None);
        let stranger = dgram(&mut n, 1, None);
        let server_addr = Addr::inet(n.hosts[0].addr, 9000);
        assert_eq!(
            n.send_dgram(client, None, b"x"),
            Err(Errno(libc::EDESTADDRREQ))
        );
        n.connect_dgram(client, server_addr).unwrap();
        assert_eq!(n.send_dgram(client, None, b"hi"), Ok(2));
        let mut buf = [0u8; 8];
        let (_, from) = n.recv_dgram(server, &mut buf, false).unwrap();

        n.bind(stranger, Addr::inet(0, 9000)).unwrap();
        assert_eq!(n.send_dgram(stranger, Some((1, from)), b"no"), Ok(2));
        assert_eq!(n.recv_dgram(client, &mut buf, false), Err(WouldBlock));
        assert_eq!(n.send_dgram(server, Some((1, from)), b"yes"), Ok(3));
        assert_eq!(n.recv_dgram(client, &mut buf, false).unwrap().0, 3);
    }

    #[test]
    fn traffic_between_hosts_arrives_after_the_latency_and_in_order() {
        let mut n = net();
        n.latency_ns = 5_000_000;
        n.advance(1_000_000);
        let l = listener(&mut n, 0, 80);
        let c = client(&mut n, 1);
        n.connect(c, 0, &Addr::inet(n.hosts[0].addr, 80)).unwrap();
        let far = n.accept(l).unwrap();
        n.add_ref(0, far);
        let mut buf = [0u8; 16];

        assert_eq!(n.send(c, b"one"), Ok(3));
        n.advance(2_000_000);
        assert_eq!(n.send(c, b"two"), Ok(3));
        assert_eq!(n.next_due(), Some(6_000_000));
        assert_eq!(n.recv(far, &mut buf, false), Err(WouldBlock));
        assert!(!n.advance(5_999_999));
        assert!(n.advance(6_000_000));
        assert_eq!(n.recv(far, &mut buf, false), Ok(3));
        assert_eq!(n.next_due(), Some(7_000_000));

        // The close travels behind the data
        n.drop_ref(1, c);
        n.advance(7_000_000);
        assert_eq!(n.recv(far, &mut buf, false), Ok(3));
        assert_eq!(n.recv(far, &mut buf, false), Err(WouldBlock));
        n.advance(12_000_000);
        assert_eq!(n.recv(far, &mut buf, false), Ok(0));
        assert_eq!(n.next_due(), None);
        assert_eq!(n.bytes, 6);
    }

    #[test]
    fn bytes_in_flight_hold_the_window_and_loopback_is_never_delayed() {
        let mut n = net();
        n.latency_ns = 5_000_000;
        let l = listener(&mut n, 0, 80);
        let c = client(&mut n, 1);
        n.connect(c, 0, &Addr::inet(n.hosts[0].addr, 80)).unwrap();
        let far = n.accept(l).unwrap();
        n.add_ref(0, far);
        let big = vec![3u8; 2 * RING];
        let first = n.send(c, &big).unwrap();
        assert!(first > RING - 64 && first <= RING, "{first}");
        assert!(!n.writable(c));
        assert_eq!(n.send(c, &big), Err(WouldBlock));
        n.advance(5_000_000);
        assert_eq!(n.pending_bytes(far), first);

        let home = client(&mut n, 0);
        n.connect(home, 0, &Addr::inet(LOOPBACK, 80)).unwrap();
        let far_home = n.accept(l).unwrap();
        n.add_ref(0, far_home);
        assert_eq!(n.send(home, b"now"), Ok(3));
        let mut buf = [0u8; 8];
        assert_eq!(n.recv(far_home, &mut buf, false), Ok(3));

        let server = dgram(&mut n, 0, Some(9000));
        let sender = dgram(&mut n, 1, None);
        let to = (0, Addr::inet(n.hosts[0].addr, 9000));
        assert_eq!(n.send_dgram(sender, Some(to), b"late"), Ok(4));
        assert_eq!(n.recv_dgram(server, &mut buf, false), Err(WouldBlock));
        n.advance(10_000_000);
        assert_eq!(n.recv_dgram(server, &mut buf, false).unwrap().0, 4);
    }

    #[test]
    fn idents_find_sockets_until_they_are_released() {
        let mut n = net();
        let c = client(&mut n, 0);
        n.set_ident(c, 4711);
        assert_eq!(n.by_ident(4711), Some(c));
        n.drop_ref(0, c);
        assert_eq!(n.by_ident(4711), None);
    }
}
