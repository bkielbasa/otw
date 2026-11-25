//! WinSock 1.1 guest API backed by the host networking stack.
//!
//! Genuine Win95 network clients (WININET.DLL used by Internet Explorer 3,
//! WSOCK32.DLL users, and the Winsock tools from the era) call these stdcall
//! imports. Sockets are real host sockets, so a guest can talk to a local
//! server or the internet without reimplementing TCP/IP in the VM. The guest
//! still sees WinSock handles, error codes and structure layouts.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs, UdpSocket},
    time::{Duration, Instant},
};

use crate::{
    kernel::pe::ImportName,
    win32::{ApiContext, ApiOutcome, GuestMemory, GuestMessage, SharedState},
};

/// `INVALID_SOCKET` and `SOCKET_ERROR` are both `(SOCKET)-1` in WinSock 1.1.
const INVALID_SOCKET: u32 = u32::MAX;
const SOCKET_ERROR: u32 = u32::MAX;

const AF_INET: u16 = 2;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;

const WSAEINTR: u32 = 10004;
const WSAEACCES: u32 = 10013;
const WSAEFAULT: u32 = 10014;
const WSAEINVAL: u32 = 10022;
const WSAEWOULDBLOCK: u32 = 10035;
const WSAENOTSOCK: u32 = 10038;
const WSAEDESTADDRREQ: u32 = 10039;
const WSAEPROTONOSUPPORT: u32 = 10043;
const WSAEOPNOTSUPP: u32 = 10045;
const WSAEAFNOSUPPORT: u32 = 10047;
const WSAEADDRINUSE: u32 = 10048;
const WSAEADDRNOTAVAIL: u32 = 10049;
const WSAENETDOWN: u32 = 10050;
const WSAECONNABORTED: u32 = 10053;
const WSAECONNRESET: u32 = 10054;
const WSAENOBUFS: u32 = 10055;
const WSAEISCONN: u32 = 10056;
const WSAENOTCONN: u32 = 10056;
const WSAESHUTDOWN: u32 = 10057;
const WSAETIMEDOUT: u32 = 10060;
const WSAECONNREFUSED: u32 = 10061;
const WSAHOST_NOT_FOUND: u32 = 11001;
const WSAVERNOTSUPPORTED: u32 = 10092;
const WSANOTINITIALISED: u32 = 10093;

const FIONBIO: u32 = 0x8004_667E;
const FIONREAD: u32 = 0x4004_667F;

const FD_SETSIZE: usize = 64;
const FD_READ: u16 = 0x0001;
const FD_WRITE: u16 = 0x0002;
const FD_ACCEPT: u16 = 0x0008;
const FD_CONNECT: u16 = 0x0010;
const FD_CLOSE: u16 = 0x0020;

/// A host socket plus the bookkeeping a guest can observe.
struct SocketEntry {
    kind: GuestSocket,
    nonblocking: bool,
    local_addr: Option<SocketAddrV4>,
    remote_addr: Option<SocketAddrV4>,
    /// `WSAAsyncSelect` registration; zero disables notifications.
    async_hwnd: u32,
    async_msg: u32,
    async_events: u32,
}

enum GuestSocket {
    /// `socket()` has been called but not yet bound or connected.
    Unbound {
        af: i32,
        kind: i32,
        protocol: i32,
    },
    TcpListener {
        listener: TcpListener,
        /// Connections accepted while polling in `select` but not yet returned
        /// to the guest through `accept`.
        pending: VecDeque<(TcpStream, SocketAddr)>,
    },
    TcpStream(TcpStream),
    Udp(UdpSocket),
}

/// WinSock state attached to [`crate::win32::SharedState`]. Host sockets are not
/// `Debug`, so only the guest-observable counters are reported.
pub struct WinSockState {
    started: bool,
    version: u16,
    last_error: u32,
    next_socket: u32,
    sockets: HashMap<u32, SocketEntry>,
    /// Guest address of the buffer returned by `inet_ntoa`.
    inet_buffer: u32,
    /// `select` calls whose thread came off the CPU, by thread.
    selects: HashMap<u32, PendingSelect>,
}

impl fmt::Debug for WinSockState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WinSockState")
            .field("started", &self.started)
            .field("version", &format_args!("{:#06x}", self.version))
            .field("last_error", &self.last_error)
            .field("sockets", &self.sockets.len())
            .finish_non_exhaustive()
    }
}

impl Default for WinSockState {
    fn default() -> Self {
        Self {
            started: false,
            version: 0,
            last_error: 0,
            next_socket: 1,
            sockets: HashMap::new(),
            inet_buffer: 0,
            selects: HashMap::new(),
        }
    }
}

impl WinSockState {
    fn insert(&mut self, kind: GuestSocket, nonblocking: bool) -> u32 {
        let handle = self.next_socket;
        self.next_socket = self.next_socket.wrapping_add(1);
        if self.next_socket == 0 || self.next_socket == INVALID_SOCKET {
            self.next_socket = 1;
        }
        self.sockets.insert(
            handle,
            SocketEntry {
                kind,
                nonblocking,
                local_addr: None,
                remote_addr: None,
                async_hwnd: 0,
                async_msg: 0,
                async_events: 0,
            },
        );
        handle
    }

    fn entry(&mut self, handle: u32) -> Option<&mut SocketEntry> {
        self.sockets.get_mut(&handle)
    }
}

fn ok(state: &mut WinSockState, value: u32, count: usize) -> ApiOutcome {
    state.last_error = 0;
    ApiOutcome::new(value, count)
}

fn fail(state: &mut WinSockState, code: u32, count: usize) -> ApiOutcome {
    state.last_error = code;
    ApiOutcome::new(SOCKET_ERROR, count)
}

fn io_error_code(error: &io::Error) -> u32 {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => WSAECONNREFUSED,
        io::ErrorKind::ConnectionAborted => WSAECONNABORTED,
        io::ErrorKind::ConnectionReset => WSAECONNRESET,
        io::ErrorKind::TimedOut => WSAETIMEDOUT,
        io::ErrorKind::AddrInUse => WSAEADDRINUSE,
        io::ErrorKind::AddrNotAvailable => WSAEADDRNOTAVAIL,
        io::ErrorKind::WouldBlock => WSAEWOULDBLOCK,
        io::ErrorKind::PermissionDenied => WSAEACCES,
        io::ErrorKind::InvalidInput => WSAEINVAL,
        io::ErrorKind::UnexpectedEof => WSAECONNRESET,
        io::ErrorKind::Interrupted => WSAEINTR,
        io::ErrorKind::OutOfMemory => WSAENOBUFS,
        io::ErrorKind::NotConnected => WSAENOTCONN,
        io::ErrorKind::BrokenPipe => WSAESHUTDOWN,
        _ => WSAENETDOWN,
    }
}

fn io_failure(state: &mut WinSockState, error: &io::Error, count: usize) -> ApiOutcome {
    if std::env::var_os("OTW_TRACE_NET").is_some() {
        eprintln!("[net] failed: {error}");
    }
    fail(state, io_error_code(error), count)
}

fn ipv4(address: SocketAddr) -> Option<SocketAddrV4> {
    match address {
        SocketAddr::V4(address) => Some(address),
        SocketAddr::V6(_) => None,
    }
}

/// Deliver a `WSAAsyncSelect` notification through the guest message queue.
fn post_async(shared: &mut SharedState, handle: u32, event: u16) {
    let (hwnd, message) = match shared.winsock.sockets.get(&handle) {
        Some(entry) if entry.async_hwnd != 0 && entry.async_events & event as u32 != 0 => {
            (entry.async_hwnd, entry.async_msg)
        }
        _ => return,
    };
    shared.gui.posted.push_back(GuestMessage {
        hwnd,
        message,
        wparam: handle,
        // Low word: network event. High word: error code (success here).
        lparam: event as u32,
    });
}

/// The function a WSOCK32 (or 16-bit-era WINSOCK) export ordinal names;
/// WinINet imports WSOCK32 by ordinal.
pub fn ordinal_name(dll: &str, ordinal: u16) -> Option<&'static str> {
    if !dll.eq_ignore_ascii_case("wsock32.dll") && !dll.eq_ignore_ascii_case("winsock.dll") {
        return None;
    }
    Some(match ordinal {
        1 => "accept",
        2 => "bind",
        3 => "closesocket",
        4 => "connect",
        5 => "getpeername",
        6 => "getsockname",
        7 => "getsockopt",
        8 => "htonl",
        9 => "htons",
        10 => "inet_addr",
        11 => "inet_ntoa",
        12 => "ioctlsocket",
        13 => "listen",
        14 => "ntohl",
        15 => "ntohs",
        16 => "recv",
        17 => "recvfrom",
        18 => "select",
        19 => "send",
        20 => "sendto",
        21 => "setsockopt",
        22 => "shutdown",
        23 => "socket",
        51 => "gethostbyaddr",
        52 => "gethostbyname",
        53 => "getprotobyname",
        54 => "getprotobynumber",
        55 => "getservbyname",
        56 => "getservbyport",
        57 => "gethostname",
        101 => "WSAAsyncSelect",
        102 => "WSAAsyncGetHostByAddr",
        103 => "WSAAsyncGetHostByName",
        108 => "WSACancelAsyncRequest",
        109 => "WSASetBlockingHook",
        110 => "WSAUnhookBlockingHook",
        111 => "WSAGetLastError",
        112 => "WSASetLastError",
        113 => "WSACancelBlockingCall",
        114 => "WSAIsBlocking",
        115 => "WSAStartup",
        116 => "WSACleanup",
        151 => "__WSAFDIsSet",
        1107 => "WSARecvEx",
        _ => return None,
    })
}

/// Number of dwords the guest pushes for each stdcall import.
pub fn arg_count(name: &str) -> Option<usize> {
    Some(match name {
        "WSACleanup"
        | "WSAGetLastError"
        | "WSAIsBlocking"
        | "WSACancelBlockingCall"
        | "WSAUnhookBlockingHook" => 0,
        "WSASetLastError"
        | "closesocket"
        | "gethostbyname"
        | "inet_addr"
        | "inet_ntoa"
        | "htonl"
        | "ntohl"
        | "htons"
        | "ntohs"
        | "getprotobyname"
        | "getprotobynumber"
        | "WSACancelAsyncRequest"
        | "WSASetBlockingHook" => 1,
        "listen" | "shutdown" | "getservbyname" | "getservbyport" | "WSAStartup"
        | "gethostname" | "__WSAFDIsSet" => 2,
        "socket" | "connect" | "bind" | "accept" | "ioctlsocket" | "gethostbyaddr"
        | "getpeername" | "getsockname" => 3,
        "send" | "recv" | "WSARecvEx" | "WSAAsyncSelect" => 4,
        "select" | "setsockopt" | "getsockopt" | "WSAAsyncGetHostByName" => 5,
        "sendto" | "recvfrom" => 6,
        "WSAAsyncGetHostByAddr" => 7,
        _ => return None,
    })
}

pub fn dispatch<M: GuestMemory + ?Sized>(
    ctx: &ApiContext,
    memory: &mut M,
    name: &ImportName,
    args: &[u32],
) -> Option<ApiOutcome> {
    let ImportName::ByName(name) = name else {
        return None;
    };
    let count = arg_count(name)?;
    if args.len() < count {
        return None;
    }
    if name == "select" {
        return Some(select_waiting(ctx, memory, args, count));
    }
    // One borrow for the whole call: helpers receive `&mut SharedState` so no
    // nested `RefCell::borrow_mut` can panic.
    let mut shared = ctx.shared.borrow_mut();

    let outcome = match name.as_str() {
        "WSAStartup" => {
            let requested = (args[0] & 0xffff) as u16;
            if requested == 0 || requested > 0x0101 {
                let _ = write_wsadata(memory, args[1], 0);
                fail(&mut shared.winsock, WSAVERNOTSUPPORTED, count)
            } else {
                shared.winsock.started = true;
                shared.winsock.version = requested;
                let _ = write_wsadata(memory, args[1], requested);
                ok(&mut shared.winsock, 0, count)
            }
        }
        "WSACleanup" => {
            if !shared.winsock.started {
                fail(&mut shared.winsock, WSANOTINITIALISED, count)
            } else {
                shared.winsock.started = false;
                shared.winsock.sockets.clear();
                ok(&mut shared.winsock, 0, count)
            }
        }
        "WSAGetLastError" => ApiOutcome::new(shared.winsock.last_error, count),
        "WSASetLastError" => {
            shared.winsock.last_error = args[0];
            ApiOutcome::new(0, count)
        }
        "WSAIsBlocking" | "WSACancelBlockingCall" | "WSAUnhookBlockingHook" => {
            ApiOutcome::new(0, count)
        }
        "WSASetBlockingHook" => {
            // Return a non-null sentinel for the previous hook. The guest does
            // not actually run blocking hooks inside the VM.
            ApiOutcome::new(1, count)
        }
        "WSACancelAsyncRequest" => ApiOutcome::new(0, count),

        "socket" => {
            if !shared.winsock.started {
                return Some(fail(&mut shared.winsock, WSANOTINITIALISED, count));
            }
            let (af, kind, protocol) = (args[0] as i32, args[1] as i32, args[2] as i32);
            if af != AF_INET as i32 {
                return Some(fail(&mut shared.winsock, WSAEAFNOSUPPORT, count));
            }
            if kind != SOCK_STREAM && kind != SOCK_DGRAM {
                return Some(fail(&mut shared.winsock, WSAEPROTONOSUPPORT, count));
            }
            if protocol != 0
                && !(kind == SOCK_STREAM && protocol == 6)
                && !(kind == SOCK_DGRAM && protocol == 17)
            {
                return Some(fail(&mut shared.winsock, WSAEPROTONOSUPPORT, count));
            }
            let handle = shared
                .winsock
                .insert(GuestSocket::Unbound { af, kind, protocol }, false);
            ok(&mut shared.winsock, handle, count)
        }
        "closesocket" => {
            if shared.winsock.sockets.contains_key(&args[0]) {
                post_async(&mut shared, args[0], FD_CLOSE);
                shared.winsock.sockets.remove(&args[0]);
                ok(&mut shared.winsock, 0, count)
            } else {
                fail(&mut shared.winsock, WSAENOTSOCK, count)
            }
        }
        "bind" => bind(&mut shared, memory, args, count),
        "listen" => {
            if !shared.winsock.sockets.contains_key(&args[0]) {
                return Some(fail(&mut shared.winsock, WSAENOTSOCK, count));
            }
            let supported = matches!(
                shared.winsock.sockets[&args[0]].kind,
                GuestSocket::Unbound {
                    kind: SOCK_STREAM,
                    ..
                } | GuestSocket::TcpListener { .. }
            );
            if supported {
                ok(&mut shared.winsock, 0, count)
            } else {
                fail(&mut shared.winsock, WSAEOPNOTSUPP, count)
            }
        }
        "connect" => connect(&mut shared, memory, args, count),
        "accept" => accept(&mut shared, memory, args, count),
        "send" => send(&mut shared, memory, args, count),
        "recv" | "WSARecvEx" => recv(&mut shared, memory, args, count),
        "getpeername" => peer_name(&mut shared, memory, args, count, true),
        "getsockname" => peer_name(&mut shared, memory, args, count, false),
        "sendto" => sendto(&mut shared, memory, args, count),
        "recvfrom" => recvfrom(&mut shared, memory, args, count),
        "shutdown" => {
            if !shared.winsock.sockets.contains_key(&args[0]) {
                return Some(fail(&mut shared.winsock, WSAENOTSOCK, count));
            }
            let result = match &mut shared.winsock.sockets.get_mut(&args[0]).unwrap().kind {
                GuestSocket::TcpStream(stream) => match args[1] {
                    0 => stream.shutdown(std::net::Shutdown::Read),
                    1 => stream.shutdown(std::net::Shutdown::Write),
                    2 => stream.shutdown(std::net::Shutdown::Both),
                    _ => return Some(fail(&mut shared.winsock, WSAEINVAL, count)),
                },
                _ => Err(io::Error::from(io::ErrorKind::NotConnected)),
            };
            match result {
                Ok(()) => ok(&mut shared.winsock, 0, count),
                Err(error) => io_failure(&mut shared.winsock, &error, count),
            }
        }
        "ioctlsocket" => ioctlsocket(&mut shared, memory, args, count),
        "setsockopt" => {
            if shared.winsock.sockets.contains_key(&args[0]) {
                ok(&mut shared.winsock, 0, count)
            } else {
                fail(&mut shared.winsock, WSAENOTSOCK, count)
            }
        }
        "getsockopt" => {
            if !shared.winsock.sockets.contains_key(&args[0]) {
                return Some(fail(&mut shared.winsock, WSAENOTSOCK, count));
            }
            if args[4] != 0 {
                let _ = memory.write_u32(args[3], 0);
            }
            ok(&mut shared.winsock, 0, count)
        }
        "gethostbyname" => {
            let name = read_cstr_limited(memory, args[0], 256);
            let name = String::from_utf8_lossy(&name);
            if name.is_empty() {
                return Some(fail(&mut shared.winsock, WSAEFAULT, count));
            }
            let addresses = resolve_host(&name);
            match build_hostent(&mut shared, memory, &name, &addresses) {
                Some(va) => ok(&mut shared.winsock, va, count),
                None => {
                    shared.winsock.last_error = WSAHOST_NOT_FOUND;
                    ApiOutcome::new(0, count)
                }
            }
        }
        "gethostbyaddr" => {
            let bytes = if args[1] == 4 && args[2] == AF_INET as u32 {
                memory.read_bytes(args[0], 4)
            } else {
                None
            };
            let Some(bytes) = bytes else {
                return Some(fail(&mut shared.winsock, WSAEINVAL, count));
            };
            let address = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
            let name = address.to_string();
            match build_hostent(&mut shared, memory, &name, &[address]) {
                Some(va) => ok(&mut shared.winsock, va, count),
                None => {
                    shared.winsock.last_error = WSAHOST_NOT_FOUND;
                    ApiOutcome::new(0, count)
                }
            }
        }
        "gethostname" => {
            let name = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_owned());
            let capacity = args[1] as usize;
            if args[0] == 0 || capacity == 0 {
                return Some(fail(&mut shared.winsock, WSAEFAULT, count));
            }
            let bytes = name.as_bytes();
            let length = bytes.len().min(capacity.saturating_sub(1));
            if memory
                .write_bytes(args[0], &bytes[..length])
                .and_then(|()| memory.write_bytes(args[0].wrapping_add(length as u32), b"\0"))
                .is_none()
            {
                return Some(fail(&mut shared.winsock, WSAEFAULT, count));
            }
            ok(&mut shared.winsock, 0, count)
        }
        "getprotobyname" => {
            let name = read_cstr_limited(memory, args[0], 32);
            let text = String::from_utf8_lossy(&name);
            match protocol_number(&name) {
                Some(number) => match write_protoent(&mut shared, memory, &text, number) {
                    Some(va) => ok(&mut shared.winsock, va, count),
                    None => fail(&mut shared.winsock, WSAEFAULT, count),
                },
                None => host_not_found(&mut shared, count),
            }
        }
        "getprotobynumber" => {
            let number = args[0] as u16;
            let name = protocol_name(number).unwrap_or("");
            if name.is_empty() {
                host_not_found(&mut shared, count)
            } else {
                match write_protoent(&mut shared, memory, name, number) {
                    Some(va) => ok(&mut shared.winsock, va, count),
                    None => fail(&mut shared.winsock, WSAEFAULT, count),
                }
            }
        }
        "getservbyname" => {
            let name = read_cstr_limited(memory, args[0], 32);
            let protocol = read_cstr_limited(memory, args[1], 16);
            let text = String::from_utf8_lossy(&name);
            match service_port(&name, &protocol) {
                Some((port, proto)) => {
                    match write_servent(&mut shared, memory, &text, port, proto) {
                        Some(va) => ok(&mut shared.winsock, va, count),
                        None => fail(&mut shared.winsock, WSAEFAULT, count),
                    }
                }
                None => host_not_found(&mut shared, count),
            }
        }
        "getservbyport" => {
            let port = (args[0] as u16).swap_bytes();
            let protocol = read_cstr_limited(memory, args[1], 16);
            match service_name(port, &protocol) {
                Some((name, proto)) => {
                    match write_servent(&mut shared, memory, name, port, proto) {
                        Some(va) => ok(&mut shared.winsock, va, count),
                        None => fail(&mut shared.winsock, WSAEFAULT, count),
                    }
                }
                None => host_not_found(&mut shared, count),
            }
        }
        "inet_addr" => {
            let text = read_cstr_limited(memory, args[0], 16);
            match parse_ipv4(&text) {
                // An in_addr: the octets in network order in memory.
                Some(address) => ok(
                    &mut shared.winsock,
                    u32::from_le_bytes(address.octets()),
                    count,
                ),
                None => ok(&mut shared.winsock, INVALID_SOCKET, count),
            }
        }
        "inet_ntoa" => {
            let address = Ipv4Addr::from(args[0].to_le_bytes());
            let text = address.to_string();
            let buffer = if shared.winsock.inet_buffer == 0 {
                let va = shared.bump_alloc(16);
                shared.winsock.inet_buffer = va;
                va
            } else {
                shared.winsock.inet_buffer
            };
            match memory.write_bytes(buffer, format!("{text}\0").as_bytes()) {
                Some(()) => ok(&mut shared.winsock, buffer, count),
                None => fail(&mut shared.winsock, WSAEFAULT, count),
            }
        }
        "htonl" | "ntohl" => ApiOutcome::new(args[0].swap_bytes(), count),
        "htons" | "ntohs" => {
            ApiOutcome::new(((args[0] & 0xffff) as u16).swap_bytes() as u32, count)
        }
        "WSAAsyncSelect" => {
            let Some(entry) = shared.winsock.entry(args[0]) else {
                return Some(fail(&mut shared.winsock, WSAENOTSOCK, count));
            };
            entry.async_hwnd = if args[3] == 0 { 0 } else { args[1] };
            entry.async_msg = args[2];
            entry.async_events = args[3];
            ok(&mut shared.winsock, 0, count)
        }
        "WSAAsyncGetHostByName" => {
            let name = read_cstr_limited(memory, args[2], 256);
            let name = String::from_utf8_lossy(&name).into_owned();
            let addresses = resolve_host(&name);
            match write_hostent_into_buffer(memory, args[3], args[4] as usize, &name, &addresses) {
                Some(()) => {
                    post_async_message(&mut shared, args[0], args[1], 1, 0);
                    ok(&mut shared.winsock, 1, count)
                }
                None => host_not_found(&mut shared, count),
            }
        }
        "WSAAsyncGetHostByAddr" => {
            let bytes = if args[3] == 4 && args[4] == AF_INET as u32 {
                memory.read_bytes(args[2], 4)
            } else {
                None
            };
            match bytes {
                Some(bytes) if bytes.len() == 4 => {
                    let address = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
                    let name = address.to_string();
                    match write_hostent_into_buffer(
                        memory,
                        args[5],
                        args[6] as usize,
                        &name,
                        &[address],
                    ) {
                        Some(()) => {
                            post_async_message(&mut shared, args[0], args[1], 1, 0);
                            ok(&mut shared.winsock, 1, count)
                        }
                        None => host_not_found(&mut shared, count),
                    }
                }
                _ => fail(&mut shared.winsock, WSAEINVAL, count),
            }
        }
        "__WSAFDIsSet" => {
            let set = read_fd_set(memory, args[1]).unwrap_or_default();
            ApiOutcome::new(u32::from(set.contains(&args[0])), count)
        }
        _ => return None,
    };
    Some(outcome)
}

fn read_cstr_limited<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, max: usize) -> Vec<u8> {
    if va == 0 {
        return Vec::new();
    }
    let bytes = memory.read_bytes(va, max).unwrap_or_default();
    match bytes.iter().position(|byte| *byte == 0) {
        Some(index) => bytes[..index].to_vec(),
        None => bytes,
    }
}

fn write_wsadata<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, version: u16) -> Option<()> {
    if va == 0 {
        return None;
    }
    // WSADATA: wVersion, wHighVersion, szDescription[257],
    // szSystemStatus[129], iMaxSockets, iMaxUdpDg, lpVendorInfo (at 396,
    // aligned): 400 bytes. Writing more overruns callers' stack buffers.
    let mut data = vec![0u8; 400];
    data[0..2].copy_from_slice(&version.to_le_bytes());
    data[2..4].copy_from_slice(&0x0101u16.to_le_bytes());
    put_cstr(&mut data[4..261], b"WinSock 1.1");
    put_cstr(&mut data[261..390], b"Running");
    data[390..392].copy_from_slice(&256u16.to_le_bytes());
    data[392..394].copy_from_slice(&8192u16.to_le_bytes());
    memory.write_bytes(va, &data)
}

fn put_cstr(target: &mut [u8], text: &[u8]) {
    let length = text.len().min(target.len().saturating_sub(1));
    target[..length].copy_from_slice(&text[..length]);
}

fn host_not_found(shared: &mut SharedState, count: usize) -> ApiOutcome {
    shared.winsock.last_error = WSAHOST_NOT_FOUND;
    ApiOutcome::new(0, count)
}

fn post_async_message(shared: &mut SharedState, hwnd: u32, message: u32, wparam: u32, error: u32) {
    if hwnd == 0 {
        return;
    }
    shared.gui.posted.push_back(GuestMessage {
        hwnd,
        message,
        wparam,
        // Low word: result size, high word: error code.
        lparam: error << 16,
    });
}

fn protocol_number(name: &[u8]) -> Option<u16> {
    let name = std::str::from_utf8(name).ok()?;
    Some(match name.to_ascii_lowercase().as_str() {
        "ip" => 0,
        "icmp" => 1,
        "tcp" => 6,
        "udp" => 17,
        "ipv6" => 41,
        _ => return None,
    })
}

fn protocol_name(number: u16) -> Option<&'static str> {
    Some(match number {
        0 => "ip",
        1 => "icmp",
        6 => "tcp",
        17 => "udp",
        41 => "ipv6",
        _ => return None,
    })
}

fn service_port(name: &[u8], protocol: &[u8]) -> Option<(u16, &'static str)> {
    let name = std::str::from_utf8(name).ok()?.to_ascii_lowercase();
    let protocol = std::str::from_utf8(protocol).ok()?.to_ascii_lowercase();
    let (port, canonical) = match name.as_str() {
        "http" | "www" => (80, "tcp"),
        "https" => (443, "tcp"),
        "ftp" => (21, "tcp"),
        "telnet" => (23, "tcp"),
        "smtp" => (25, "tcp"),
        "domain" => (53, "udp"),
        "gopher" => (70, "tcp"),
        "pop3" => (110, "tcp"),
        "nntp" => (119, "tcp"),
        _ => return None,
    };
    if !protocol.is_empty() && protocol != canonical {
        return None;
    }
    Some((port, canonical))
}

fn service_name(port: u16, protocol: &[u8]) -> Option<(&'static str, &'static str)> {
    let protocol = std::str::from_utf8(protocol).ok()?.to_ascii_lowercase();
    let (name, canonical) = match port {
        21 => ("ftp", "tcp"),
        23 => ("telnet", "tcp"),
        25 => ("smtp", "tcp"),
        53 => ("domain", "udp"),
        70 => ("gopher", "tcp"),
        80 => ("http", "tcp"),
        110 => ("pop3", "tcp"),
        119 => ("nntp", "tcp"),
        443 => ("https", "tcp"),
        _ => return None,
    };
    if !protocol.is_empty() && protocol != canonical {
        return None;
    }
    Some((name, canonical))
}

fn write_protoent<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    name: &str,
    number: u16,
) -> Option<u32> {
    let name_va = shared.bump_alloc((name.len() + 1) as u32);
    let aliases_va = shared.bump_alloc(4);
    let entry_va = shared.bump_alloc(12);
    memory.write_bytes(name_va, format!("{name}\0").as_bytes())?;
    memory.write_u32(aliases_va, 0)?;
    memory.write_u32(entry_va, name_va)?;
    memory.write_u32(entry_va.wrapping_add(4), aliases_va)?;
    memory.write_bytes(entry_va.wrapping_add(8), &number.to_le_bytes())?;
    Some(entry_va)
}

fn write_servent<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    name: &str,
    port: u16,
    protocol: &str,
) -> Option<u32> {
    let name_va = shared.bump_alloc((name.len() + 1) as u32);
    let aliases_va = shared.bump_alloc(4);
    let protocol_va = shared.bump_alloc((protocol.len() + 1) as u32);
    let entry_va = shared.bump_alloc(16);
    memory.write_bytes(name_va, format!("{name}\0").as_bytes())?;
    memory.write_u32(aliases_va, 0)?;
    memory.write_bytes(protocol_va, format!("{protocol}\0").as_bytes())?;
    memory.write_u32(entry_va, name_va)?;
    memory.write_u32(entry_va.wrapping_add(4), aliases_va)?;
    // s_port is stored in network byte order.
    memory.write_bytes(entry_va.wrapping_add(8), &port.to_be_bytes())?;
    memory.write_u32(entry_va.wrapping_add(12), protocol_va)?;
    Some(entry_va)
}

fn write_hostent_into_buffer<M: GuestMemory + ?Sized>(
    memory: &mut M,
    buffer: u32,
    capacity: usize,
    name: &str,
    addresses: &[Ipv4Addr],
) -> Option<()> {
    if buffer == 0 || addresses.is_empty() {
        return None;
    }
    let align4 = |value: usize| (value + 3) & !3;
    let name_off = 16;
    let aliases_off = align4(name_off + name.len() + 1);
    let address_list_off = aliases_off + 4;
    let address_data_off = address_list_off + (addresses.len() + 1) * 4;
    let total = address_data_off + addresses.len() * 4;
    if total > capacity {
        return None;
    }

    let hostent = buffer;
    memory.write_u32(hostent, buffer.wrapping_add(name_off as u32))?;
    memory.write_u32(
        hostent.wrapping_add(4),
        buffer.wrapping_add(aliases_off as u32),
    )?;
    memory.write_bytes(hostent.wrapping_add(8), &AF_INET.to_le_bytes())?;
    memory.write_bytes(hostent.wrapping_add(10), &4u16.to_le_bytes())?;
    memory.write_u32(
        hostent.wrapping_add(12),
        buffer.wrapping_add(address_list_off as u32),
    )?;

    memory.write_bytes(
        buffer.wrapping_add(name_off as u32),
        format!("{name}\0").as_bytes(),
    )?;
    memory.write_u32(buffer.wrapping_add(aliases_off as u32), 0)?;
    for (index, address) in addresses.iter().enumerate() {
        let entry_va = buffer.wrapping_add((address_data_off + index * 4) as u32);
        memory.write_bytes(entry_va, &address.octets())?;
        memory.write_u32(
            buffer.wrapping_add((address_list_off + index * 4) as u32),
            entry_va,
        )?;
    }
    memory.write_u32(
        buffer.wrapping_add((address_list_off + addresses.len() * 4) as u32),
        0,
    )?;
    Some(())
}

fn peer_name<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    args: &[u32],
    count: usize,
    remote: bool,
) -> ApiOutcome {
    let Some(entry) = shared.winsock.entry(args[0]) else {
        return fail(&mut shared.winsock, WSAENOTSOCK, count);
    };
    let address = if remote {
        entry.remote_addr
    } else {
        entry.local_addr
    };
    let Some(address) = address else {
        return fail(&mut shared.winsock, WSAENOTCONN, count);
    };
    if args[1] != 0 {
        let _ = write_sockaddr(memory, args[1], address);
    }
    if args[2] != 0 {
        let _ = memory.write_u32(args[2], 16);
    }
    ok(&mut shared.winsock, 0, count)
}

fn read_sockaddr<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> Option<SocketAddrV4> {
    let bytes = memory.read_bytes(va, 16)?;
    if u16::from_le_bytes([bytes[0], bytes[1]]) != AF_INET {
        return None;
    }
    let port = u16::from_be_bytes([bytes[2], bytes[3]]);
    let address = Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]);
    Some(SocketAddrV4::new(address, port))
}

fn write_sockaddr<M: GuestMemory + ?Sized>(
    memory: &mut M,
    va: u32,
    address: SocketAddrV4,
) -> Option<()> {
    let mut bytes = [0u8; 16];
    bytes[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    bytes[2..4].copy_from_slice(&address.port().to_be_bytes());
    bytes[4..8].copy_from_slice(&address.ip().octets());
    memory.write_bytes(va, &bytes)
}

fn bind<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    args: &[u32],
    count: usize,
) -> ApiOutcome {
    let Some(address) = read_sockaddr(memory, args[1]) else {
        return fail(&mut shared.winsock, WSAEFAULT, count);
    };
    if !shared.winsock.sockets.contains_key(&args[0]) {
        return fail(&mut shared.winsock, WSAENOTSOCK, count);
    }
    let (af, kind, nonblocking) = match &shared.winsock.sockets[&args[0]] {
        SocketEntry {
            kind: GuestSocket::Unbound { af, kind, .. },
            nonblocking,
            ..
        } => (*af, *kind, *nonblocking),
        _ => return fail(&mut shared.winsock, WSAEINVAL, count),
    };
    if af != AF_INET as i32 {
        return fail(&mut shared.winsock, WSAEAFNOSUPPORT, count);
    }
    let bound = match kind {
        SOCK_STREAM => TcpListener::bind(address).map(|listener| GuestSocket::TcpListener {
            listener,
            pending: VecDeque::new(),
        }),
        SOCK_DGRAM => UdpSocket::bind(address).map(GuestSocket::Udp),
        _ => return fail(&mut shared.winsock, WSAEPROTONOSUPPORT, count),
    };
    match bound {
        Ok(kind) => {
            // The address the host gave (port 0 asks for any free port).
            let actual = match &kind {
                GuestSocket::TcpListener { listener, .. } => listener.local_addr().ok().and_then(ipv4),
                GuestSocket::Udp(socket) => socket.local_addr().ok().and_then(ipv4),
                _ => None,
            };
            let entry = shared.winsock.sockets.get_mut(&args[0]).unwrap();
            entry.kind = kind;
            entry.nonblocking = nonblocking;
            entry.local_addr = Some(actual.unwrap_or(address));
            ok(&mut shared.winsock, 0, count)
        }
        Err(error) => io_failure(&mut shared.winsock, &error, count),
    }
}

fn connect<M: GuestMemory + ?Sized>(
    mut shared: &mut SharedState,
    memory: &mut M,
    args: &[u32],
    count: usize,
) -> ApiOutcome {
    let Some(address) = read_sockaddr(memory, args[1]) else {
        return fail(&mut shared.winsock, WSAEFAULT, count);
    };
    if !shared.winsock.sockets.contains_key(&args[0]) {
        return fail(&mut shared.winsock, WSAENOTSOCK, count);
    }
    let (af, kind, protocol, nonblocking) = match &shared.winsock.sockets[&args[0]].kind {
        GuestSocket::Unbound { af, kind, protocol } => (
            *af,
            *kind,
            *protocol,
            shared.winsock.sockets[&args[0]].nonblocking,
        ),
        GuestSocket::TcpStream(_) => return fail(&mut shared.winsock, WSAEISCONN, count),
        GuestSocket::Udp(socket) => {
            // A bound datagram socket gets a default peer.
            let result = socket.connect(address);
            return match result {
                Ok(()) => {
                    shared.winsock.sockets.get_mut(&args[0]).unwrap().remote_addr = Some(address);
                    ok(&mut shared.winsock, 0, count)
                }
                Err(error) => io_failure(&mut shared.winsock, &error, count),
            };
        }
        // A stream socket bound (WinINet binds to any address and port 0
        // first) but not accepting yet connects like a fresh one; the host
        // picks its local address.
        GuestSocket::TcpListener { pending, .. } if pending.is_empty() => {
            (AF_INET as i32, SOCK_STREAM, 0, shared.winsock.sockets[&args[0]].nonblocking)
        }
        GuestSocket::TcpListener { .. } => return fail(&mut shared.winsock, WSAEOPNOTSUPP, count),
    };
    if af != AF_INET as i32 {
        return fail(&mut shared.winsock, WSAEAFNOSUPPORT, count);
    }
    if protocol != 0
        && !(kind == SOCK_STREAM && protocol == 6)
        && !(kind == SOCK_DGRAM && protocol == 17)
    {
        return fail(&mut shared.winsock, WSAEPROTONOSUPPORT, count);
    }
    if std::env::var_os("OTW_TRACE_NET").is_some() {
        eprintln!("[net] connect socket {} to {address}", args[0]);
    }
    let result = match kind {
        SOCK_STREAM => TcpStream::connect_timeout(&address.into(), Duration::from_secs(20)).map(|stream| {
            let _ = stream.set_nonblocking(nonblocking);
            GuestSocket::TcpStream(stream)
        }),
        SOCK_DGRAM => UdpSocket::bind("0.0.0.0:0").and_then(|socket| {
            socket.connect(address)?;
            let _ = socket.set_nonblocking(nonblocking);
            Ok(GuestSocket::Udp(socket))
        }),
        _ => return fail(&mut shared.winsock, WSAEPROTONOSUPPORT, count),
    };
    match result {
        Ok(kind) => {
            let entry = shared.winsock.sockets.get_mut(&args[0]).unwrap();
            entry.local_addr = match &kind {
                GuestSocket::TcpStream(stream) => stream.local_addr().ok().and_then(ipv4),
                GuestSocket::Udp(socket) => socket.local_addr().ok().and_then(ipv4),
                _ => None,
            };
            entry.remote_addr = Some(address);
            entry.kind = kind;
            post_async(&mut shared, args[0], FD_CONNECT);
            post_async(&mut shared, args[0], FD_WRITE);
            ok(&mut shared.winsock, 0, count)
        }
        Err(error) => io_failure(&mut shared.winsock, &error, count),
    }
}

fn accept<M: GuestMemory + ?Sized>(
    mut shared: &mut SharedState,
    memory: &mut M,
    args: &[u32],
    count: usize,
) -> ApiOutcome {
    if !shared.winsock.sockets.contains_key(&args[0]) {
        return fail(&mut shared.winsock, WSAENOTSOCK, count);
    }
    let accepted = match &mut shared.winsock.sockets.get_mut(&args[0]).unwrap().kind {
        GuestSocket::TcpListener { listener, pending } => {
            if let Some(accepted) = pending.pop_front() {
                Ok(accepted)
            } else {
                let _ = listener.set_nonblocking(true);
                listener.accept()
            }
        }
        _ => return fail(&mut shared.winsock, WSAEOPNOTSUPP, count),
    };
    let (stream, address) = match accepted {
        Ok(value) => value,
        Err(error) => return io_failure(&mut shared.winsock, &error, count),
    };
    let nonblocking = shared.winsock.sockets[&args[0]].nonblocking;
    let listener_addr = match &shared.winsock.sockets[&args[0]].kind {
        GuestSocket::TcpListener { listener, .. } => listener.local_addr().ok().and_then(ipv4),
        _ => None,
    };
    let remote_addr = ipv4(address);
    let _ = stream.set_nonblocking(nonblocking);
    if args[1] != 0 {
        if let Some(address) = remote_addr {
            let _ = write_sockaddr(memory, args[1], address);
        }
        if args[2] != 0 {
            let _ = memory.write_u32(args[2], 16);
        }
    }
    post_async(&mut shared, args[0], FD_ACCEPT);
    let handle = shared
        .winsock
        .insert(GuestSocket::TcpStream(stream), nonblocking);
    let entry = shared.winsock.entry(handle).unwrap();
    entry.local_addr = listener_addr;
    entry.remote_addr = remote_addr;
    ok(&mut shared.winsock, handle, count)
}

fn send<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    args: &[u32],
    count: usize,
) -> ApiOutcome {
    let Some(entry) = shared.winsock.entry(args[0]) else {
        return fail(&mut shared.winsock, WSAENOTSOCK, count);
    };
    let Some(bytes) = memory.read_bytes(args[1], bounded_length(args[2])) else {
        return fail(&mut shared.winsock, WSAEFAULT, count);
    };
    let result = match &mut entry.kind {
        GuestSocket::TcpStream(stream) => stream.write_all(&bytes).map(|()| bytes.len()),
        GuestSocket::Udp(socket) => socket.send(&bytes),
        _ => return fail(&mut shared.winsock, WSAENOTCONN, count),
    };
    match result {
        Ok(length) => {
            post_async(shared, args[0], FD_WRITE);
            ok(&mut shared.winsock, length as u32, count)
        }
        Err(error) => io_failure(&mut shared.winsock, &error, count),
    }
}

fn recv<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    args: &[u32],
    count: usize,
) -> ApiOutcome {
    let Some(entry) = shared.winsock.entry(args[0]) else {
        return fail(&mut shared.winsock, WSAENOTSOCK, count);
    };
    let mut buffer = vec![0u8; bounded_length(args[2])];
    let peek = args[3] & 0x02 != 0; // MSG_PEEK
    let result = match &mut entry.kind {
        GuestSocket::TcpStream(stream) => {
            if peek {
                stream.peek(&mut buffer)
            } else {
                stream.read(&mut buffer)
            }
        }
        GuestSocket::Udp(socket) => {
            if peek {
                socket.peek(&mut buffer)
            } else {
                socket.recv(&mut buffer)
            }
        }
        _ => return fail(&mut shared.winsock, WSAENOTCONN, count),
    };
    match result {
        Ok(read) => {
            if memory.write_bytes(args[1], &buffer[..read]).is_none() {
                return fail(&mut shared.winsock, WSAEFAULT, count);
            }
            post_async(shared, args[0], FD_READ);
            ok(&mut shared.winsock, read as u32, count)
        }
        Err(error) => io_failure(&mut shared.winsock, &error, count),
    }
}

fn sendto<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    args: &[u32],
    count: usize,
) -> ApiOutcome {
    let address = if args[4] == 0 {
        None
    } else {
        match read_sockaddr(memory, args[4]) {
            Some(address) => Some(address),
            None => return fail(&mut shared.winsock, WSAEFAULT, count),
        }
    };
    let bytes = match memory.read_bytes(args[1], bounded_length(args[2])) {
        Some(bytes) => bytes,
        None => return fail(&mut shared.winsock, WSAEFAULT, count),
    };
    if !shared.winsock.sockets.contains_key(&args[0]) {
        return fail(&mut shared.winsock, WSAENOTSOCK, count);
    }
    let result = match &mut shared.winsock.sockets.get_mut(&args[0]).unwrap().kind {
        GuestSocket::Udp(socket) => match address {
            Some(address) => socket.send_to(&bytes, address),
            None => socket.send(&bytes),
        },
        GuestSocket::TcpStream(stream) => stream.write_all(&bytes).map(|()| bytes.len()),
        _ => return fail(&mut shared.winsock, WSAEDESTADDRREQ, count),
    };
    match result {
        Ok(length) => ok(&mut shared.winsock, length as u32, count),
        Err(error) => io_failure(&mut shared.winsock, &error, count),
    }
}

fn recvfrom<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    args: &[u32],
    count: usize,
) -> ApiOutcome {
    if !shared.winsock.sockets.contains_key(&args[0]) {
        return fail(&mut shared.winsock, WSAENOTSOCK, count);
    }
    let mut buffer = vec![0u8; bounded_length(args[2])];
    let result = match &mut shared.winsock.sockets.get_mut(&args[0]).unwrap().kind {
        GuestSocket::Udp(socket) => socket
            .recv_from(&mut buffer)
            .map(|(size, address)| (size, Some(address))),
        GuestSocket::TcpStream(stream) => stream.read(&mut buffer).map(|size| (size, None)),
        _ => return fail(&mut shared.winsock, WSAENOTCONN, count),
    };
    let (read, address) = match result {
        Ok(value) => value,
        Err(error) => return io_failure(&mut shared.winsock, &error, count),
    };
    if memory.write_bytes(args[1], &buffer[..read]).is_none() {
        return fail(&mut shared.winsock, WSAEFAULT, count);
    }
    if let Some(SocketAddr::V4(address)) = address {
        if args[4] != 0 && args[5] != 0 {
            let _ = write_sockaddr(memory, args[4], address);
            let _ = memory.write_u32(args[5], 16);
        }
    }
    ok(&mut shared.winsock, read as u32, count)
}

fn read_fd_set<M: GuestMemory + ?Sized>(memory: &mut M, va: u32) -> Option<Vec<u32>> {
    if va == 0 {
        return Some(Vec::new());
    }
    let count = memory.read_u32(va)? as usize;
    if count > FD_SETSIZE {
        return None;
    }
    let mut sockets = Vec::with_capacity(count);
    for index in 0..count {
        sockets.push(memory.read_u32(va.wrapping_add(4 + index as u32 * 4))?);
    }
    Some(sockets)
}

fn write_fd_set<M: GuestMemory + ?Sized>(memory: &mut M, va: u32, sockets: &[u32]) -> Option<()> {
    if va == 0 {
        return Some(());
    }
    let count = sockets.len().min(FD_SETSIZE);
    memory.write_u32(va, count as u32)?;
    for (index, socket) in sockets.iter().take(count).enumerate() {
        memory.write_u32(va.wrapping_add(4 + index as u32 * 4), *socket)?;
    }
    Some(())
}

fn is_readable(entry: &mut SocketEntry) -> bool {
    // Probes never block: the socket is non-blocking for the peek and then
    // restored to the guest's mode.
    let restore = !entry.nonblocking;
    let mut probe = [0u8; 1];
    match &mut entry.kind {
        GuestSocket::TcpStream(stream) => {
            let _ = stream.set_nonblocking(true);
            let ready = !matches!(stream.peek(&mut probe), Err(ref error) if error.kind() == io::ErrorKind::WouldBlock);
            if restore {
                let _ = stream.set_nonblocking(false);
            }
            ready
        }
        GuestSocket::Udp(socket) => {
            let _ = socket.set_nonblocking(true);
            let ready = !matches!(socket.peek(&mut probe), Err(ref error) if error.kind() == io::ErrorKind::WouldBlock);
            if restore {
                let _ = socket.set_nonblocking(false);
            }
            ready
        }
        GuestSocket::TcpListener { listener, pending } => {
            if !pending.is_empty() {
                return true;
            }
            let _ = listener.set_nonblocking(true);
            match listener.accept() {
                Ok(accepted) => {
                    pending.push_back(accepted);
                    true
                }
                Err(error) => error.kind() != io::ErrorKind::WouldBlock,
            }
        }
        GuestSocket::Unbound { .. } => false,
    }
}

/// A `select` waiting off the CPU: its deadline and the sockets it reads
/// and writes.
struct PendingSelect {
    deadline: Option<Instant>,
    read: Vec<u32>,
    write: Vec<u32>,
}

/// Whether a thread waiting in `select` can go on: `None` when it is not
/// in `select`.
pub fn select_ready(shared: &mut SharedState, thread: u32) -> Option<bool> {
    let state = &mut shared.winsock;
    let pending = state.selects.get(&thread)?;
    if pending.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Some(true);
    }
    let (read, write) = (pending.read.clone(), pending.write.clone());
    let writable = write.iter().any(|h| state.sockets.get(h).is_some_and(|e| !matches!(e.kind, GuestSocket::Unbound { .. } | GuestSocket::TcpListener { .. })));
    Some(writable || read.iter().any(|h| state.entry(*h).is_some_and(is_readable)))
}

/// `select`: sockets are polled; while none is ready and the timeout has
/// not passed, a thread that can come off the CPU does (the call is made
/// again when it next runs), so the program's other threads keep running.
fn select_waiting<M: GuestMemory + ?Sized>(ctx: &ApiContext, memory: &mut M, args: &[u32], count: usize) -> ApiOutcome {
    let thread = memory.current_thread();
    let deadline = match ctx.shared.borrow_mut().winsock.selects.remove(&thread) {
        Some(pending) => pending.deadline,
        None if args[4] == 0 => None,
        None => {
            let seconds = memory.read_u32(args[4]).unwrap_or(0);
            let micros = memory.read_u32(args[4].wrapping_add(4)).unwrap_or(0);
            Some(Instant::now() + Duration::from_secs(seconds as u64) + Duration::from_micros(micros as u64))
        }
    };
    loop {
        let expired = deadline.is_some_and(|deadline| Instant::now() >= deadline);
        if let Some(outcome) = select_poll(&mut ctx.shared.borrow_mut(), memory, args, count, expired) {
            return outcome;
        }
        let suspended = memory.suspend_thread();
        if std::env::var_os("OTW_TRACE_THREADS").is_some() {
            eprintln!("[thread] {thread} select: not ready, suspended={suspended} deadline in {:?}", deadline.map(|d| d.saturating_duration_since(Instant::now())));
        }
        if suspended {
            let read = read_fd_set(memory, args[1]).unwrap_or_default();
            let write = read_fd_set(memory, args[2]).unwrap_or_default();
            ctx.shared.borrow_mut().winsock.selects.insert(thread, PendingSelect { deadline, read, write });
            return ApiOutcome::new(0, count);
        }
        if crate::win32::sync::run_ready_threads(ctx, memory) {
            continue;
        }
        if memory.stopping() {
            return select_poll(&mut ctx.shared.borrow_mut(), memory, args, count, true).expect("expired");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// One look at the sockets of a `select`: the result when some are ready
/// or the time is up.
fn select_poll<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    args: &[u32],
    count: usize,
    expired: bool,
) -> Option<ApiOutcome> {
    let read = read_fd_set(memory, args[1]).unwrap_or_default();
    let write = read_fd_set(memory, args[2]).unwrap_or_default();
    let except = read_fd_set(memory, args[3]).unwrap_or_default();
    for handle in read.iter().chain(&write).chain(&except) {
        if !shared.winsock.sockets.contains_key(handle) {
            return Some(fail(&mut shared.winsock, WSAENOTSOCK, count));
        }
    }
    let mut ready_read = Vec::new();
    for handle in &read {
        if let Some(entry) = shared.winsock.entry(*handle) {
            if is_readable(entry) {
                ready_read.push(*handle);
            }
        }
    }
    let ready_write: Vec<u32> = write
        .iter()
        .copied()
        .filter(|handle| shared.winsock.sockets.get(handle).is_some_and(|e| !matches!(e.kind, GuestSocket::Unbound { .. } | GuestSocket::TcpListener { .. })))
        .collect();
    let ready_except: Vec<u32> = Vec::new();
    let total = ready_read.len() + ready_write.len() + ready_except.len();
    if total == 0 && !expired {
        return None;
    }
    let _ = write_fd_set(memory, args[1], &ready_read);
    let _ = write_fd_set(memory, args[2], &ready_write);
    let _ = write_fd_set(memory, args[3], &ready_except);
    Some(ok(&mut shared.winsock, total as u32, count))
}

fn ioctlsocket<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    args: &[u32],
    count: usize,
) -> ApiOutcome {
    if !shared.winsock.sockets.contains_key(&args[0]) {
        return fail(&mut shared.winsock, WSAENOTSOCK, count);
    }
    match args[1] {
        FIONBIO => {
            let enabled = memory.read_u32(args[2]).unwrap_or(0) != 0;
            let result = match &mut shared.winsock.sockets.get_mut(&args[0]).unwrap().kind {
                GuestSocket::TcpListener { listener, .. } => listener.set_nonblocking(enabled),
                GuestSocket::TcpStream(stream) => stream.set_nonblocking(enabled),
                GuestSocket::Udp(socket) => socket.set_nonblocking(enabled),
                GuestSocket::Unbound { .. } => Ok(()),
            };
            shared
                .winsock
                .sockets
                .get_mut(&args[0])
                .unwrap()
                .nonblocking = enabled;
            match result {
                Ok(()) => ok(&mut shared.winsock, 0, count),
                Err(error) => io_failure(&mut shared.winsock, &error, count),
            }
        }
        FIONREAD => {
            let mut probe = [0u8; 4096];
            let available = match &mut shared.winsock.sockets.get_mut(&args[0]).unwrap().kind {
                GuestSocket::TcpStream(stream) => stream.peek(&mut probe).unwrap_or(0),
                GuestSocket::Udp(socket) => socket.peek(&mut probe).unwrap_or(0),
                _ => 0,
            };
            match memory.write_u32(args[2], available as u32) {
                Some(()) => ok(&mut shared.winsock, 0, count),
                None => fail(&mut shared.winsock, WSAEFAULT, count),
            }
        }
        _ => fail(&mut shared.winsock, WSAEINVAL, count),
    }
}

fn resolve_host(name: &str) -> Vec<Ipv4Addr> {
    if let Ok(address) = name.parse::<Ipv4Addr>() {
        return vec![address];
    }
    let mut addresses = Vec::new();
    if let Ok(entries) = (name, 0u16).to_socket_addrs() {
        for entry in entries {
            if let SocketAddr::V4(address) = entry {
                let ip = *address.ip();
                if !addresses.contains(&ip) {
                    addresses.push(ip);
                }
            }
        }
    }
    addresses
}

fn build_hostent<M: GuestMemory + ?Sized>(
    shared: &mut SharedState,
    memory: &mut M,
    name: &str,
    addresses: &[Ipv4Addr],
) -> Option<u32> {
    if addresses.is_empty() {
        return None;
    }
    let (name_va, list_va, address_va, hostent_va, aliases_va) = (
        shared.bump_alloc((name.len() + 1) as u32),
        shared.bump_alloc(((addresses.len() + 1) * 4) as u32),
        shared.bump_alloc((addresses.len() * 4) as u32),
        shared.bump_alloc(16),
        shared.bump_alloc(4),
    );
    memory.write_bytes(name_va, format!("{name}\0").as_bytes())?;
    // No aliases: an empty list, never a null pointer (WinINet walks it).
    memory.write_u32(aliases_va, 0)?;
    for (index, address) in addresses.iter().enumerate() {
        let entry = address_va.wrapping_add(index as u32 * 4);
        memory.write_bytes(entry, &address.octets())?;
        memory.write_u32(list_va.wrapping_add(index as u32 * 4), entry)?;
    }
    memory.write_u32(list_va.wrapping_add(addresses.len() as u32 * 4), 0)?;

    let mut hostent = [0u8; 16];
    hostent[0..4].copy_from_slice(&name_va.to_le_bytes());
    hostent[4..8].copy_from_slice(&aliases_va.to_le_bytes());
    hostent[8..10].copy_from_slice(&AF_INET.to_le_bytes());
    hostent[10..12].copy_from_slice(&4u16.to_le_bytes());
    hostent[12..16].copy_from_slice(&list_va.to_le_bytes());
    memory.write_bytes(hostent_va, &hostent)?;
    Some(hostent_va)
}

fn parse_ipv4(text: &[u8]) -> Option<Ipv4Addr> {
    let text = std::str::from_utf8(text).ok()?;
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (index, part) in parts.iter().enumerate() {
        octets[index] = part.parse().ok()?;
    }
    Some(Ipv4Addr::from(octets))
}

fn bounded_length(length: u32) -> usize {
    (length as usize).min(16 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, thread};

    use super::*;

    #[derive(Default)]
    struct TestMemory(HashMap<u32, u8>);

    impl GuestMemory for TestMemory {
        fn read_bytes(&mut self, va: u32, len: usize) -> Option<Vec<u8>> {
            Some(
                (0..len)
                    .map(|offset| {
                        self.0
                            .get(&va.wrapping_add(offset as u32))
                            .copied()
                            .unwrap_or(0)
                    })
                    .collect(),
            )
        }

        fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Option<()> {
            for (offset, byte) in bytes.iter().enumerate() {
                self.0.insert(va.wrapping_add(offset as u32), *byte);
            }
            Some(())
        }

        fn write_u32(&mut self, va: u32, value: u32) -> Option<()> {
            self.write_bytes(va, &value.to_le_bytes())
        }
    }

    fn invoke(ctx: &ApiContext, memory: &mut TestMemory, name: &str, args: &[u32]) -> ApiOutcome {
        dispatch(ctx, memory, &ImportName::ByName(name.to_owned()), args)
            .expect("winsock import should be handled")
    }

    fn call(name: &str, args: &[u32], memory: &mut TestMemory) -> (ApiOutcome, u32) {
        let ctx = ApiContext::default();
        let outcome = dispatch(&ctx, memory, &ImportName::ByName(name.to_owned()), args)
            .expect("winsock import should be handled");
        let error = ctx.shared.borrow().winsock.last_error;
        (outcome, error)
    }

    fn cstr(memory: &mut TestMemory, va: u32) -> String {
        let bytes = read_cstr_limited(memory, va, 64);
        String::from_utf8(bytes).expect("guest string must be ASCII")
    }

    #[test]
    fn startup_writes_wsadata_and_byteswap_helpers_work() {
        let mut memory = TestMemory::default();
        let (outcome, error) = call("WSAStartup", &[0x0101, 0x1000], &mut memory);
        assert_eq!(outcome.value, 0);
        assert_eq!(error, 0);
        assert_eq!(memory.read_bytes(0x1000, 2), Some(vec![1, 1]));
        assert_eq!(memory.read_bytes(0x1002, 2), Some(vec![1, 1]));

        let (outcome, _) = call("htonl", &[0x0102_0304], &mut memory);
        assert_eq!(outcome.value, 0x0403_0201);
        let (outcome, _) = call("htons", &[0x0102], &mut memory);
        assert_eq!(outcome.value, 0x0201);
    }

    #[test]
    fn loopback_connect_send_recv_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 16];
            let length = stream.read(&mut buffer).unwrap();
            stream.write_all(&buffer[..length]).unwrap();
        });

        let ctx = ApiContext::default();
        let mut memory = TestMemory::default();
        assert_eq!(
            invoke(&ctx, &mut memory, "WSAStartup", &[0x0101, 0x1000]).value,
            0
        );

        // sockaddr_in for 127.0.0.1:<port>
        memory.write_bytes(
            0x2000,
            &[
                2,
                0,
                (port >> 8) as u8,
                port as u8,
                127,
                0,
                0,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ],
        );
        let socket = invoke(&ctx, &mut memory, "socket", &[2, 1, 0]).value;
        assert_ne!(socket, INVALID_SOCKET);
        assert_eq!(
            invoke(&ctx, &mut memory, "connect", &[socket, 0x2000, 16]).value,
            0
        );
        memory.write_bytes(0x3000, b"ping");
        assert_eq!(
            invoke(&ctx, &mut memory, "send", &[socket, 0x3000, 4, 0]).value,
            4
        );
        assert_eq!(
            invoke(&ctx, &mut memory, "recv", &[socket, 0x4000, 16, 0]).value,
            4
        );
        assert_eq!(memory.read_bytes(0x4000, 4), Some(b"ping".to_vec()));

        assert_eq!(
            invoke(&ctx, &mut memory, "getpeername", &[socket, 0x5000, 0x5010]).value,
            0
        );
        let length = memory.read_u32(0x5010).unwrap();
        assert_eq!(length, 16);
        assert_eq!(memory.read_bytes(0x5004, 4), Some(vec![127, 0, 0, 1]));
        assert_eq!(
            invoke(&ctx, &mut memory, "getsockname", &[socket, 0x5100, 0x5110]).value,
            0
        );
        assert_eq!(memory.read_u32(0x5110), Some(16));

        assert_eq!(invoke(&ctx, &mut memory, "closesocket", &[socket]).value, 0);
        server.join().unwrap();
    }

    #[test]
    fn protocol_and_service_lookups_return_guest_structures() {
        let ctx = ApiContext::default();
        let mut memory = TestMemory::default();
        memory.write_bytes(0x1000, b"tcp\0");
        memory.write_bytes(0x1010, b"http\0");
        memory.write_bytes(0x1020, b"tcp\0");

        let protoent = invoke(&ctx, &mut memory, "getprotobyname", &[0x1000]).value;
        assert_ne!(protoent, 0);
        let name = memory.read_u32(protoent).unwrap();
        assert_eq!(cstr(&mut memory, name), "tcp");
        assert_eq!(
            memory.read_bytes(protoent.wrapping_add(8), 2),
            Some(6u16.to_le_bytes().to_vec())
        );

        let servent = invoke(&ctx, &mut memory, "getservbyname", &[0x1010, 0x1020]).value;
        assert_ne!(servent, 0);
        let name = memory.read_u32(servent).unwrap();
        assert_eq!(cstr(&mut memory, name), "http");
        assert_eq!(
            memory.read_bytes(servent.wrapping_add(8), 2),
            Some(80u16.to_be_bytes().to_vec())
        );
        let protocol = memory.read_u32(servent.wrapping_add(12)).unwrap();
        assert_eq!(cstr(&mut memory, protocol), "tcp");
    }

    #[test]
    fn async_select_posts_connect_and_write_notifications() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(20));
        });

        let ctx = ApiContext::default();
        let mut memory = TestMemory::default();
        invoke(&ctx, &mut memory, "WSAStartup", &[0x0101, 0x1000]);
        memory.write_bytes(
            0x2000,
            &[
                2,
                0,
                (port >> 8) as u8,
                port as u8,
                127,
                0,
                0,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ],
        );
        let socket = invoke(&ctx, &mut memory, "socket", &[2, 1, 0]).value;
        assert_eq!(
            invoke(
                &ctx,
                &mut memory,
                "WSAAsyncSelect",
                &[socket, 0x3000, 0x0400, FD_CONNECT as u32 | FD_WRITE as u32],
            )
            .value,
            0
        );
        assert_eq!(
            invoke(&ctx, &mut memory, "connect", &[socket, 0x2000, 16]).value,
            0
        );

        let messages: Vec<_> = ctx.shared.borrow().gui.posted.iter().copied().collect();
        assert!(messages.iter().any(|message| {
            message.message == 0x0400
                && message.wparam == socket
                && message.lparam & 0xffff == FD_CONNECT as u32
        }));
        assert!(messages.iter().any(|message| {
            message.message == 0x0400
                && message.wparam == socket
                && message.lparam & 0xffff == FD_WRITE as u32
        }));
        server.join().unwrap();
    }

    #[test]
    fn async_get_host_by_name_fills_the_guest_buffer() {
        let ctx = ApiContext::default();
        let mut memory = TestMemory::default();
        memory.write_bytes(0x1000, b"127.0.0.1\0");

        assert_eq!(
            invoke(
                &ctx,
                &mut memory,
                "WSAAsyncGetHostByName",
                &[0x3000, 0x0400, 0x1000, 0x4000, 256],
            )
            .value,
            1
        );
        assert_eq!(ctx.shared.borrow().gui.posted.len(), 1);

        let name = memory.read_u32(0x4000).unwrap();
        assert_eq!(cstr(&mut memory, name), "127.0.0.1");
        assert_eq!(
            memory.read_bytes(0x4000 + 10, 2),
            Some(4u16.to_le_bytes().to_vec())
        );
        let list = memory.read_u32(0x4000 + 12).unwrap();
        let address = memory.read_u32(list).unwrap();
        assert_eq!(memory.read_bytes(address, 4), Some(vec![127, 0, 0, 1]));
    }

    #[test]
    fn uninitialised_calls_report_expected_error() {
        let mut memory = TestMemory::default();
        let (outcome, error) = call("socket", &[2, 1, 0], &mut memory);
        assert_eq!(outcome.value, SOCKET_ERROR);
        assert_eq!(error, WSANOTINITIALISED);
    }
}
