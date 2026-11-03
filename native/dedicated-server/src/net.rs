// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The world's UDP socket (contract §5.1 "Bind"): one IPv6 socket in dual mode (IPV6_V6ONLY off,
//! exclusive address use) on `[::]:port`. An IPv6 "address in use" is a failure, never a fall-back;
//! any other IPv6 error (or no IPv6 on the machine) falls back to an IPv4 socket on the same port.
//! The port never moves.
//!
//! std cannot set IPV6_V6ONLY before bind (std's `UdpSocket::bind` is socket+bind in one call),
//! so the socket is made here with the system's own socket/setsockopt/bind (std already links libc
//! and ws2_32) and handed to std. On Windows SO_EXCLUSIVEADDRUSE is set on both families, as the
//! C# `ExclusiveAddressUse`.

use std::{
    io,
    net::{IpAddr, SocketAddr, UdpSocket},
};

/// Why a world socket could not be bound (`bind_failed {reason}`, `--probe-bind` exit codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindFailure {
    InUse,
    Denied,
    Other,
}

impl BindFailure {
    pub fn reason(self) -> &'static str {
        match self {
            BindFailure::InUse => "in_use",
            BindFailure::Denied => "denied",
            BindFailure::Other => "other",
        }
    }

    fn of(e: &io::Error) -> BindFailure {
        match e.kind() {
            io::ErrorKind::AddrInUse => BindFailure::InUse,
            io::ErrorKind::PermissionDenied => BindFailure::Denied,
            _ => BindFailure::Other,
        }
    }
}

/// A bound world socket; `dual` is false for the IPv4-only fall-back.
pub struct Bound {
    pub socket: UdpSocket,
    pub dual: bool,
}

/// Binds the private world socket on `port` (§5.1).
pub fn bind_world(port: u16) -> Result<Bound, BindFailure> {
    match sys::bind_v6_dual(port) {
        Ok(socket) => Ok(Bound { socket, dual: true }),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Err(BindFailure::InUse),
        Err(_) => match sys::bind_v4(port) {
            Ok(socket) => Ok(Bound {
                socket,
                dual: false,
            }),
            Err(e) => Err(BindFailure::of(&e)),
        },
    }
}

/// Binds the public profile's loopback socket (§5.2), a plain bind of a loopback literal.
pub fn bind_loopback(addr: SocketAddr) -> io::Result<UdpSocket> {
    if !is_loopback(addr.ip()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a loopback address",
        ));
    }
    UdpSocket::bind(addr)
}

/// Windows: switches off SIO_UDP_CONNRESET, so an ICMP port unreachable for an earlier send
/// does not surface as WSAECONNRESET on the next receive (best effort; the reader drops that
/// error anyway). Elsewhere a no-op.
pub fn quiet_connreset(socket: &UdpSocket) {
    #[cfg(windows)]
    sys::udp_connreset_off(socket);
    #[cfg(not(windows))]
    let _ = socket;
}

/// IPv4-mapped IPv6 addresses as their IPv4 form.
pub fn norm_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

/// The key every per-IP limit counts under: an IPv4 address (IPv4-mapped IPv6 unmapped) as it
/// is, an IPv6 address as its /64 (one subscriber's prefix: rotating addresses inside it share
/// one budget). A relay leg's virtual peer keeps its whole address (its low bytes are its
/// grant's, never one subscriber's).
pub fn ip_key(endpoint: SocketAddr) -> IpAddr {
    match norm_ip(endpoint.ip()) {
        IpAddr::V6(v6) if !crate::core::legs::is_virtual(endpoint) => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
        ip => ip,
    }
}

pub fn is_loopback(ip: IpAddr) -> bool {
    norm_ip(ip).is_loopback()
}

/// RFC 1918 / ULA / link-local: an address that never crosses the Internet.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match norm_ip(ip) {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let s = v6.segments()[0];
            (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80
        }
    }
}

/// sockaddr_in6 (the same layout on Linux and Windows): family, port (big-endian), flow info,
/// address, scope id. 28 bytes.
#[cfg(any(target_os = "linux", target_os = "android", windows))]
#[allow(dead_code)]
#[repr(C)]
struct SockaddrIn6 {
    family: u16,
    port: u16,
    flowinfo: u32,
    addr: [u8; 16],
    scope_id: u32,
}

/// sockaddr_in (Windows only here): family, port (big-endian), address, zero padding. 16 bytes.
#[cfg(windows)]
#[allow(dead_code)]
#[repr(C)]
struct SockaddrIn {
    family: u16,
    port: u16,
    addr: [u8; 4],
    zero: [u8; 8],
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod sys {
    use super::SockaddrIn6;
    use std::{
        ffi::{c_int, c_void},
        io,
        net::{Ipv4Addr, UdpSocket},
        os::fd::FromRawFd,
    };

    const AF_INET6: c_int = 10;
    const SOCK_DGRAM: c_int = 2;
    const SOCK_CLOEXEC: c_int = 0o2000000;
    const IPPROTO_IPV6: c_int = 41;
    const IPV6_V6ONLY: c_int = 26;

    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
        fn setsockopt(
            fd: c_int,
            level: c_int,
            name: c_int,
            value: *const c_void,
            len: u32,
        ) -> c_int;
        fn bind(fd: c_int, addr: *const c_void, len: u32) -> c_int;
        fn close(fd: c_int) -> c_int;
    }

    #[allow(unsafe_code)]
    pub fn bind_v6_dual(port: u16) -> io::Result<UdpSocket> {
        // SAFETY: plain socket calls on a descriptor this function owns; on any failure it is
        // closed before returning, on success ownership moves into the UdpSocket.
        unsafe {
            let fd = socket(AF_INET6, SOCK_DGRAM | SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let off: c_int = 0;
            let sa = SockaddrIn6 {
                family: AF_INET6 as u16,
                port: port.to_be(),
                flowinfo: 0,
                addr: [0; 16],
                scope_id: 0,
            };
            if setsockopt(
                fd,
                IPPROTO_IPV6,
                IPV6_V6ONLY,
                &off as *const c_int as *const c_void,
                4,
            ) != 0
                || bind(
                    fd,
                    &sa as *const SockaddrIn6 as *const c_void,
                    size_of::<SockaddrIn6>() as u32,
                ) != 0
            {
                let e = io::Error::last_os_error();
                close(fd);
                return Err(e);
            }
            Ok(UdpSocket::from_raw_fd(fd))
        }
    }

    pub fn bind_v4(port: u16) -> io::Result<UdpSocket> {
        // Linux has no SO_EXCLUSIVEADDRUSE; without SO_REUSEADDR/SO_REUSEPORT a second bind of
        // the port fails, which is the same guarantee.
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
    }
}

#[cfg(windows)]
mod sys {
    use super::{SockaddrIn, SockaddrIn6};
    use std::{
        io,
        net::{Ipv4Addr, UdpSocket},
        os::windows::io::FromRawSocket,
    };

    const AF_INET: i32 = 2;
    const AF_INET6: i32 = 23;
    const SOCK_DGRAM: i32 = 2;
    const SOL_SOCKET: i32 = 0xffff;
    const SO_EXCLUSIVEADDRUSE: i32 = !4; // (int)(~SO_REUSEADDR)
    const IPPROTO_IPV6: i32 = 41;
    const IPV6_V6ONLY: i32 = 27;
    const INVALID_SOCKET: usize = !0;

    #[allow(unsafe_code)]
    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn socket(af: i32, ty: i32, protocol: i32) -> usize;
        fn setsockopt(s: usize, level: i32, name: i32, value: *const u8, len: i32) -> i32;
        fn bind(s: usize, name: *const u8, len: i32) -> i32;
        fn closesocket(s: usize) -> i32;
        fn WSAGetLastError() -> i32;
        fn WSAIoctl(
            s: usize,
            code: u32,
            input: *const u8,
            input_len: u32,
            output: *mut u8,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut u8,
            completion: *mut u8,
        ) -> i32;
    }

    /// `_WSAIOW(IOC_VENDOR, 12)`.
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;

    #[allow(unsafe_code)]
    pub fn udp_connreset_off(socket: &UdpSocket) {
        use std::os::windows::io::AsRawSocket;
        let off: u32 = 0; // BOOL FALSE
        let mut returned: u32 = 0;
        // SAFETY: a synchronous ioctl on the live socket `socket` owns (borrowed for the call);
        // the input is a local 4-byte BOOL, no output buffer, `returned` a live local (required
        // when not overlapped), no overlapped structure or completion routine. The result is
        // ignored: on failure the socket keeps the default and the reader drops the error.
        unsafe {
            WSAIoctl(
                socket.as_raw_socket() as usize,
                SIO_UDP_CONNRESET,
                &off as *const u32 as *const u8,
                4,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
    }

    #[allow(unsafe_code)]
    fn bind_raw(af: i32, dual: bool, addr: *const u8, len: i32) -> io::Result<UdpSocket> {
        // std calls WSAStartup the first time it makes a socket; this throwaway one does that
        drop(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)));
        // SAFETY: plain Winsock calls on a socket this function owns; `addr` points to a live
        // sockaddr of `len` bytes for the duration of the call; on failure the socket is closed,
        // on success ownership moves into the UdpSocket.
        unsafe {
            let s = socket(af, SOCK_DGRAM, 0);
            if s == INVALID_SOCKET {
                return Err(io::Error::from_raw_os_error(WSAGetLastError()));
            }
            let on: u32 = 1;
            let off: u32 = 0;
            let failed = setsockopt(
                s,
                SOL_SOCKET,
                SO_EXCLUSIVEADDRUSE,
                &on as *const u32 as *const u8,
                4,
            ) != 0
                || (dual
                    && setsockopt(
                        s,
                        IPPROTO_IPV6,
                        IPV6_V6ONLY,
                        &off as *const u32 as *const u8,
                        4,
                    ) != 0)
                || bind(s, addr, len) != 0;
            if failed {
                let e = io::Error::from_raw_os_error(WSAGetLastError());
                closesocket(s);
                return Err(e);
            }
            Ok(UdpSocket::from_raw_socket(s as u64))
        }
    }

    pub fn bind_v6_dual(port: u16) -> io::Result<UdpSocket> {
        let sa = SockaddrIn6 {
            family: AF_INET6 as u16,
            port: port.to_be(),
            flowinfo: 0,
            addr: [0; 16],
            scope_id: 0,
        };
        bind_raw(
            AF_INET6,
            true,
            &sa as *const SockaddrIn6 as *const u8,
            size_of::<SockaddrIn6>() as i32,
        )
    }

    pub fn bind_v4(port: u16) -> io::Result<UdpSocket> {
        let sa = SockaddrIn {
            family: AF_INET as u16,
            port: port.to_be(),
            addr: [0; 4],
            zero: [0; 8],
        };
        bind_raw(
            AF_INET,
            false,
            &sa as *const SockaddrIn as *const u8,
            size_of::<SockaddrIn>() as i32,
        )
    }
}

/// Elsewhere (macOS, BSDs; development only): a plain `[::]` bind, dual-stack or not as the
/// system defaults.
#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
mod sys {
    use std::{
        io,
        net::{Ipv4Addr, Ipv6Addr, UdpSocket},
    };

    pub fn bind_v6_dual(port: u16) -> io::Result<UdpSocket> {
        UdpSocket::bind((Ipv6Addr::UNSPECIFIED, port))
    }

    pub fn bind_v4(port: u16) -> io::Result<UdpSocket> {
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))
    }
}
