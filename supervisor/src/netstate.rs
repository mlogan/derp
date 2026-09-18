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
#[derive(Clone, Copy)]
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
    rx: [u8; RING],
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
    pub connections: u64,
    pub bytes: u64,
    /// Connections that left the virtual network for the kernel's
    pub passthrough: u64,
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
        match s.state {
            S_LISTENING => s.backlog_len > 0,
            S_CONNECTED => s.rx_len > 0 || s.fin || s.shut_rd,
            _ => true,
        }
    }

    pub fn writable(&self, sock: u32) -> bool {
        let s = &self.socks[sock as usize];
        if s.state != S_CONNECTED || s.shut_wr {
            return true;
        }
        let p = &self.socks[s.far_end as usize];
        p.state != S_CONNECTED || p.shut_rd || (p.rx_len as usize) < RING
    }

    /// The single entry point for traffic between guests: append `bytes`
    /// to `dst`'s receive ring, as many as fit, and return that count.
    /// Delivery here is immediate and in order. A simulator would decide
    /// from the two hosts and the virtual clock when, and whether, the
    /// bytes arrive.
    fn deliver(&mut self, _src_host: u32, dst: u32, bytes: &[u8]) -> usize {
        let d = &mut self.socks[dst as usize];
        let n = bytes.len().min(RING - d.rx_len as usize);
        let mut at = (d.rx_head as usize + d.rx_len as usize) % RING;
        for &b in &bytes[..n] {
            d.rx[at] = b;
            at = (at + 1) % RING;
        }
        d.rx_len += n as u32;
        d.bytes_in += n as u64;
        self.bytes += n as u64;
        n
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
        match self.deliver(host, peer, bytes) {
            0 => Err(WouldBlock),
            n => Ok(n),
        }
    }

    /// Receive from a stream; 0 is end of stream.
    pub fn recv(&mut self, sock: u32, out: &mut [u8], peek: bool) -> Result<usize, NetError> {
        let s = &mut self.socks[sock as usize];
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
        let mut at = s.rx_head as usize;
        for b in &mut out[..n] {
            *b = s.rx[at];
            at = (at + 1) % RING;
        }
        if !peek {
            s.rx_head = at as u32;
            s.rx_len -= n as u32;
        }
        Ok(n)
    }

    pub fn pending_bytes(&self, sock: u32) -> usize {
        self.socks[sock as usize].rx_len as usize
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
            let peer = s.far_end;
            self.socks[peer as usize].fin = true;
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
                        self.socks[near as usize].fin = true;
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
                    self.socks[peer as usize].fin = true;
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
