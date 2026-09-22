//! What a guest can learn about where it is: its host's name, the
//! addresses of the run's virtual hosts, and its interfaces. Names that
//! are not virtual hosts go to the real resolver.

use std::ffi::{c_char, c_int, CStr};

use crate::sched;
use crate::shared::netstate::{LOOPBACK, NAME_MAX};
use crate::spin::SpinLock;

fn in_run() -> bool {
    crate::coord::connected()
}

/// Lists we handed out, by address, so the matching free function can tell
/// them from libSystem's
static OURS: SpinLock<Vec<usize>> = SpinLock::new(Vec::new());

fn forget(p: usize) -> bool {
    let mut ours = OURS.lock();
    match ours.iter().position(|&o| o == p) {
        Some(i) => {
            ours.swap_remove(i);
            true
        }
        None => false,
    }
}

fn my_host() -> Option<(Vec<u8>, u32)> {
    sched::with(|s, pid| {
        let h = &s.net.hosts[s.procs[pid as usize].host as usize];
        (h.name().to_vec(), h.addr)
    })
}

pub unsafe extern "C" fn my_gethostname(name: *mut c_char, len: usize) -> c_int {
    let host = if in_run() { my_host() } else { None };
    let Some((host, _)) = host else {
        return libc::gethostname(name, len);
    };
    if name.is_null() || len == 0 {
        *libc::__error() = libc::EFAULT;
        return -1;
    }
    let n = host.len().min(len - 1);
    std::ptr::copy_nonoverlapping(host.as_ptr(), name.cast::<u8>(), n);
    *name.add(n) = 0;
    0
}

/// One `getaddrinfo` result; `ai` comes first so a pointer to it is a
/// pointer to the entry.
#[repr(C)]
struct Entry {
    ai: libc::addrinfo,
    addr: libc::sockaddr_in,
    name: [c_char; NAME_MAX + 1],
}

fn sockaddr_in(ip: u32, port: u16) -> libc::sockaddr_in {
    let mut a: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    a.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    a.sin_family = libc::AF_INET as u8;
    a.sin_port = port.to_be();
    a.sin_addr.s_addr = ip.to_be();
    a
}

unsafe fn entry(name: &[u8], ip: u32, port: u16, socktype: c_int, canon: bool) -> *mut Entry {
    let mut e = Box::new(Entry {
        ai: std::mem::zeroed(),
        addr: sockaddr_in(ip, port),
        name: [0; NAME_MAX + 1],
    });
    for (dst, &b) in e.name.iter_mut().zip(name.iter().take(NAME_MAX)) {
        *dst = b as c_char;
    }
    e.ai.ai_family = libc::AF_INET;
    e.ai.ai_socktype = socktype;
    e.ai.ai_protocol = if socktype == libc::SOCK_DGRAM {
        libc::IPPROTO_UDP
    } else {
        libc::IPPROTO_TCP
    };
    e.ai.ai_addrlen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let p = Box::into_raw(e);
    (*p).ai.ai_addr = (&raw mut (*p).addr).cast();
    if canon {
        (*p).ai.ai_canonname = (&raw mut (*p).name).cast();
    }
    p
}

static REFUSED_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A name the resolver answers without asking the network
unsafe fn needs_no_lookup(name: &[u8], hints: *const libc::addrinfo) -> bool {
    if !hints.is_null() && (*hints).ai_flags & libc::AI_NUMERICHOST != 0 {
        return true;
    }
    let Ok(text) = std::str::from_utf8(name) else {
        return false;
    };
    text.eq_ignore_ascii_case("localhost")
        || text.parse::<std::net::IpAddr>().is_ok()
        || text.strip_prefix('[').is_some_and(|t| {
            t.strip_suffix(']')
                .is_some_and(|t| t.parse::<std::net::Ipv6Addr>().is_ok())
        })
}

pub unsafe extern "C" fn my_getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const libc::addrinfo,
    res: *mut *mut libc::addrinfo,
) -> c_int {
    let virtual_host = if in_run() && !node.is_null() {
        let name = CStr::from_ptr(node).to_bytes();
        sched::with(|s, _| {
            s.net
                .host_by_name(name)
                .map(|h| s.net.hosts[h as usize].addr)
        })
        .flatten()
        .map(|addr| (name.to_vec(), addr))
    } else {
        None
    };
    // A lookup that needs no resolver is answered here all the same: the
    // system's own goes through libinfo, whose threads and sockets are
    // outside the run (Go's cgo resolver asks it for every port).
    let local = if in_run() && virtual_host.is_none() && service_is_numeric(service) {
        local_answer(node, hints)
    } else {
        None
    };
    let Some((name, addr)) = virtual_host.or(local) else {
        // Any other name is the system resolver's, which answers in real
        // time with whatever the world says: refused with the rest of the
        // outside network. Numeric addresses and localhost need no lookup.
        if in_run()
            && !node.is_null()
            && !sched::with(|s, _| s.net.outside_allowed).unwrap_or(true)
            && !needs_no_lookup(CStr::from_ptr(node).to_bytes(), hints)
        {
            if !REFUSED_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                crate::report::log(
                    "name lookup outside the virtual network refused; \
                     `outside-network: allow` in the run file lets it through",
                );
            }
            return libc::EAI_NONAME;
        }
        return libc::getaddrinfo(node, service, hints, res);
    };
    let (family, socktype, flags) = if hints.is_null() {
        (libc::AF_UNSPEC, 0, 0)
    } else {
        ((*hints).ai_family, (*hints).ai_socktype, (*hints).ai_flags)
    };
    if family != libc::AF_UNSPEC && family != libc::AF_INET {
        return libc::EAI_FAMILY;
    }
    let port = if service.is_null() {
        0
    } else {
        match CStr::from_ptr(service)
            .to_str()
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
        {
            Some(p) => p,
            None => return libc::EAI_SERVICE,
        }
    };
    let canon = flags & libc::AI_CANONNAME != 0;
    let types: &[c_int] = match socktype {
        0 => &[libc::SOCK_STREAM, libc::SOCK_DGRAM],
        libc::SOCK_STREAM => &[libc::SOCK_STREAM],
        libc::SOCK_DGRAM => &[libc::SOCK_DGRAM],
        _ => return libc::EAI_SOCKTYPE,
    };
    let mut head: *mut Entry = std::ptr::null_mut();
    for (i, &ty) in types.iter().enumerate().rev() {
        let e = entry(&name, addr, port, ty, canon && i == 0);
        (*e).ai.ai_next = head.cast();
        head = e;
    }
    OURS.lock().push(head as usize);
    *res = head.cast();
    0
}

unsafe fn service_is_numeric(service: *const c_char) -> bool {
    service.is_null()
        || CStr::from_ptr(service)
            .to_str()
            .is_ok_and(|s| s.parse::<u16>().is_ok())
}

/// The answer to a lookup of nothing (this host: any address for a
/// passive socket, else loopback), of `localhost`, or of a numeric IPv4
/// address, as `(name, address)`.
unsafe fn local_answer(node: *const c_char, hints: *const libc::addrinfo) -> Option<(Vec<u8>, u32)> {
    if node.is_null() {
        let passive = !hints.is_null() && (*hints).ai_flags & libc::AI_PASSIVE != 0;
        let ip = if passive { 0 } else { u32::from(std::net::Ipv4Addr::LOCALHOST) };
        return Some((b"localhost".to_vec(), ip));
    }
    let name = CStr::from_ptr(node).to_bytes();
    if name == b"localhost" {
        return Some((name.to_vec(), u32::from(std::net::Ipv4Addr::LOCALHOST)));
    }
    let text = std::str::from_utf8(name).ok()?;
    let ip: std::net::Ipv4Addr = text.parse().ok()?;
    Some((name.to_vec(), u32::from(ip)))
}

pub unsafe extern "C" fn my_freeaddrinfo(list: *mut libc::addrinfo) {
    if !forget(list as usize) {
        return libc::freeaddrinfo(list);
    }
    let mut p = list.cast::<Entry>();
    while !p.is_null() {
        let next = (*p).ai.ai_next.cast::<Entry>();
        drop(Box::from_raw(p));
        p = next;
    }
}

/// One `getifaddrs` result, `ifa` first
#[repr(C)]
struct Interface {
    ifa: libc::ifaddrs,
    name: [c_char; 8],
    addr: libc::sockaddr_in,
    mask: libc::sockaddr_in,
    broadcast: libc::sockaddr_in,
}

unsafe fn interface(name: &[u8], flags: c_int, ip: u32, mask: u32) -> *mut Interface {
    let mut i = Box::new(Interface {
        ifa: std::mem::zeroed(),
        name: [0; 8],
        addr: sockaddr_in(ip, 0),
        mask: sockaddr_in(mask, 0),
        broadcast: sockaddr_in(ip | !mask, 0),
    });
    for (dst, &b) in i.name.iter_mut().zip(name.iter().take(7)) {
        *dst = b as c_char;
    }
    i.ifa.ifa_flags = flags as libc::c_uint;
    let p = Box::into_raw(i);
    (*p).ifa.ifa_name = (&raw mut (*p).name).cast();
    (*p).ifa.ifa_addr = (&raw mut (*p).addr).cast();
    (*p).ifa.ifa_netmask = (&raw mut (*p).mask).cast();
    if flags & libc::IFF_BROADCAST != 0 {
        (*p).ifa.ifa_dstaddr = (&raw mut (*p).broadcast).cast();
    }
    p
}

/// The loopback and one interface with the virtual host's address.
pub unsafe extern "C" fn my_getifaddrs(out: *mut *mut libc::ifaddrs) -> c_int {
    let host = if in_run() { my_host() } else { None };
    let Some((_, addr)) = host else {
        return libc::getifaddrs(out);
    };
    let up = libc::IFF_UP | libc::IFF_RUNNING;
    let lo = interface(b"lo0", up | libc::IFF_LOOPBACK, LOOPBACK, 0xFF00_0000);
    let en = interface(
        b"en0",
        up | libc::IFF_BROADCAST | libc::IFF_MULTICAST,
        addr,
        crate::shared::netstate::SUBNET_MASK,
    );
    (*lo).ifa.ifa_next = en.cast();
    OURS.lock().push(lo as usize);
    *out = lo.cast();
    0
}

pub unsafe extern "C" fn my_freeifaddrs(list: *mut libc::ifaddrs) {
    if !forget(list as usize) {
        return libc::freeifaddrs(list);
    }
    let mut p = list.cast::<Interface>();
    while !p.is_null() {
        let next = (*p).ifa.ifa_next.cast::<Interface>();
        drop(Box::from_raw(p));
        p = next;
    }
}
