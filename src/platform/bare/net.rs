//! Network addresses (from `core`) and sockets, served by the system's
//! `Network` (none: sockets fail as an unreachable network would). The
//! system never waits; blocking sockets wait here, a millisecond at a time.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;

pub use core::net::{AddrParseError, IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use super::io::{self, Error, ErrorKind, Read, Write};
use super::{system, NetError, Network, SocketId};

fn unavailable() -> Error {
    Error::from(ErrorKind::Unsupported)
}

fn network() -> io::Result<&'static dyn Network> {
    system().network().ok_or_else(|| Error::from(ErrorKind::NotConnected))
}

fn error(e: NetError) -> Error {
    Error::from(match e {
        NetError::WouldBlock => ErrorKind::WouldBlock,
        NetError::Refused => ErrorKind::ConnectionRefused,
        NetError::Reset => ErrorKind::ConnectionReset,
        NetError::NotConnected => ErrorKind::NotConnected,
        NetError::TimedOut => ErrorKind::TimedOut,
        NetError::Unreachable => ErrorKind::NotConnected,
        NetError::AddrInUse => ErrorKind::AddrInUse,
        NetError::NotFound => ErrorKind::NotFound,
        NetError::Invalid => ErrorKind::InvalidInput,
    })
}

/// Repeat `attempt` while it would block (unless `nonblocking`), until
/// `timeout` (microseconds; 0: no limit).
fn waiting<T>(nonblocking: bool, timeout_us: u64, mut attempt: impl FnMut() -> Result<T, NetError>) -> io::Result<T> {
    let deadline = (timeout_us != 0).then(|| system().now_us() + timeout_us);
    loop {
        match attempt() {
            Err(NetError::WouldBlock) if !nonblocking => {
                if deadline.is_some_and(|d| system().now_us() >= d) {
                    return Err(Error::from(ErrorKind::TimedOut));
                }
                system().sleep_us(1000);
            }
            other => return other.map_err(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shutdown {
    Read,
    Write,
    Both,
}

pub trait ToSocketAddrs {
    type Iter: Iterator<Item = SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter>;
}

impl ToSocketAddrs for SocketAddr {
    type Iter = core::option::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        Ok(Some(*self).into_iter())
    }
}

impl ToSocketAddrs for SocketAddrV4 {
    type Iter = core::option::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        Ok(Some(SocketAddr::V4(*self)).into_iter())
    }
}

impl ToSocketAddrs for (Ipv4Addr, u16) {
    type Iter = core::option::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        Ok(Some(SocketAddr::V4(SocketAddrV4::new(self.0, self.1))).into_iter())
    }
}

fn lookup(host: &str, port: u16) -> io::Result<alloc::vec::IntoIter<SocketAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(alloc::vec![SocketAddr::new(ip, port)].into_iter());
    }
    let addresses = network()?.resolve(host).map_err(error)?;
    Ok(addresses.into_iter().map(|ip| SocketAddr::new(ip, port)).collect::<Vec<_>>().into_iter())
}

impl ToSocketAddrs for (&str, u16) {
    type Iter = alloc::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        lookup(self.0, self.1)
    }
}

impl ToSocketAddrs for str {
    type Iter = alloc::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        if let Ok(address) = self.parse::<SocketAddr>() {
            return Ok(alloc::vec![address].into_iter());
        }
        let (host, port) = self.rsplit_once(':').ok_or_else(|| Error::from(ErrorKind::InvalidInput))?;
        let port = port.parse().map_err(|_| Error::from(ErrorKind::InvalidInput))?;
        lookup(host, port)
    }
}

impl ToSocketAddrs for &str {
    type Iter = alloc::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        (**self).to_socket_addrs()
    }
}

fn first(address: impl ToSocketAddrs) -> io::Result<SocketAddr> {
    address.to_socket_addrs()?.next().ok_or_else(|| Error::from(ErrorKind::InvalidInput))
}

/// What the three socket kinds share: the system's socket, the mode.
struct Socket {
    id: SocketId,
    nonblocking: AtomicBool,
    /// Microseconds (0: none).
    read_timeout: AtomicU64,
}

impl Socket {
    fn new(id: SocketId) -> Self {
        Self { id, nonblocking: AtomicBool::new(false), read_timeout: AtomicU64::new(0) }
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Relaxed)
    }
    fn recv(&self, buffer: &mut [u8], peek: bool) -> io::Result<(usize, Option<SocketAddr>)> {
        let net = network()?;
        waiting(self.nonblocking(), self.read_timeout.load(Ordering::Relaxed), || net.recv(self.id, buffer, peek))
    }
    fn send(&self, data: &[u8], to: Option<SocketAddr>) -> io::Result<usize> {
        let net = network()?;
        waiting(self.nonblocking(), 0, || net.send(self.id, data, to))
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) {
        self.read_timeout.store(timeout.map(|t| (t.as_micros() as u64).max(1)).unwrap_or(0), Ordering::Relaxed);
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        if let Some(net) = system().network() {
            net.close(self.id);
        }
    }
}

pub struct TcpStream(Socket);
pub struct TcpListener(Socket);
pub struct UdpSocket(Socket);

impl TcpStream {
    pub fn connect(address: impl ToSocketAddrs) -> io::Result<TcpStream> {
        Self::connect_timeout(&first(address)?, Duration::from_secs(75))
    }
    pub fn connect_timeout(address: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        let net = network()?;
        let socket = Socket::new(net.tcp_connect(*address).map_err(error)?);
        waiting(false, timeout.as_micros() as u64, || net.tcp_connected(socket.id))?;
        Ok(TcpStream(socket))
    }
    /// Start connecting and return at once; `connected` tells when the
    /// connection is made (a non-blocking `connect`).
    pub fn connect_started(address: &SocketAddr) -> io::Result<TcpStream> {
        let net = network()?;
        Ok(TcpStream(Socket::new(net.tcp_connect(*address).map_err(error)?)))
    }
    /// Whether the connection is made: `Ok(false)` while it is being made.
    pub fn connected(&self) -> io::Result<bool> {
        match network()?.tcp_connected(self.0.id) {
            Ok(()) => Ok(true),
            Err(NetError::WouldBlock) => Ok(false),
            Err(e) => Err(error(e)),
        }
    }
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        if how != Shutdown::Read {
            network()?.shutdown(self.0.id);
        }
        Ok(())
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        network()?.local_addr(self.0.id).map_err(error)
    }
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        network()?.peer_addr(self.0.id).map_err(error)
    }
    pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.nonblocking.store(on, Ordering::Relaxed);
        Ok(())
    }
    pub fn set_nodelay(&self, _on: bool) -> io::Result<()> {
        Ok(())
    }
    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(t);
        Ok(())
    }
    pub fn set_write_timeout(&self, _t: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
    pub fn peek(&self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.recv(buffer, true).map(|(n, _)| n)
    }
    pub fn try_clone(&self) -> io::Result<TcpStream> {
        Err(unavailable())
    }
}

impl Read for TcpStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buffer)
    }
}

impl Read for &TcpStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.0.recv(buffer, false).map(|(n, _)| n)
    }
}

impl Write for TcpStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        (&*self).write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for &TcpStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.0.send(buffer, None)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl TcpListener {
    pub fn bind(address: impl ToSocketAddrs) -> io::Result<TcpListener> {
        let at = first(address)?;
        Ok(TcpListener(Socket::new(network()?.tcp_listen(at).map_err(error)?)))
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        network()?.local_addr(self.0.id).map_err(error)
    }
    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let net = network()?;
        let (id, from) = waiting(self.0.nonblocking(), 0, || net.tcp_accept(self.0.id))?;
        Ok((TcpStream(Socket::new(id)), from))
    }
    pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.nonblocking.store(on, Ordering::Relaxed);
        Ok(())
    }
}

impl UdpSocket {
    pub fn bind(address: impl ToSocketAddrs) -> io::Result<UdpSocket> {
        let at = first(address)?;
        Ok(UdpSocket(Socket::new(network()?.udp_bind(at).map_err(error)?)))
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        network()?.local_addr(self.0.id).map_err(error)
    }
    pub fn connect(&self, address: impl ToSocketAddrs) -> io::Result<()> {
        let to = first(address)?;
        network()?.udp_connect(self.0.id, to).map_err(error)
    }
    pub fn send(&self, buffer: &[u8]) -> io::Result<usize> {
        self.0.send(buffer, None)
    }
    pub fn send_to(&self, buffer: &[u8], address: impl ToSocketAddrs) -> io::Result<usize> {
        let to = first(address)?;
        self.0.send(buffer, Some(to))
    }
    pub fn recv(&self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.recv(buffer, false).map(|(n, _)| n)
    }
    pub fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (n, from) = self.0.recv(buffer, false)?;
        Ok((n, from.unwrap_or(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))))
    }
    pub fn peek(&self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.recv(buffer, true).map(|(n, _)| n)
    }
    pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.nonblocking.store(on, Ordering::Relaxed);
        Ok(())
    }
    pub fn set_broadcast(&self, _on: bool) -> io::Result<()> {
        Ok(())
    }
    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(t);
        Ok(())
    }
}
