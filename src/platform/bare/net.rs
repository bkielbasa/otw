//! Network addresses (from `core`) and sockets. The kernel has no network
//! yet: sockets fail as an unreachable network would.

use alloc::vec::Vec;
use core::time::Duration;

pub use core::net::{AddrParseError, IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use super::io::{self, Error, ErrorKind, Read, Write};

fn unavailable() -> Error {
    Error::from(ErrorKind::Unsupported)
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

impl ToSocketAddrs for (&str, u16) {
    type Iter = alloc::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        match self.0.parse::<Ipv4Addr>() {
            Ok(ip) => Ok(alloc::vec![SocketAddr::V4(SocketAddrV4::new(ip, self.1))].into_iter()),
            // No name resolution without a network.
            Err(_) => Err(unavailable()),
        }
    }
}

impl ToSocketAddrs for str {
    type Iter = alloc::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        match self.parse::<SocketAddr>() {
            Ok(address) => Ok(alloc::vec![address].into_iter()),
            Err(_) => Err(unavailable()),
        }
    }
}

impl ToSocketAddrs for &str {
    type Iter = alloc::vec::IntoIter<SocketAddr>;
    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        (**self).to_socket_addrs()
    }
}

pub struct TcpStream(());
pub struct TcpListener(());
pub struct UdpSocket(());

impl TcpStream {
    pub fn connect(_address: impl ToSocketAddrs) -> io::Result<TcpStream> {
        Err(unavailable())
    }
    pub fn connect_timeout(_address: &SocketAddr, _timeout: Duration) -> io::Result<TcpStream> {
        Err(unavailable())
    }
    pub fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
        Err(unavailable())
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Err(unavailable())
    }
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        Err(unavailable())
    }
    pub fn set_nonblocking(&self, _on: bool) -> io::Result<()> {
        Err(unavailable())
    }
    pub fn set_nodelay(&self, _on: bool) -> io::Result<()> {
        Err(unavailable())
    }
    pub fn set_read_timeout(&self, _t: Option<Duration>) -> io::Result<()> {
        Err(unavailable())
    }
    pub fn set_write_timeout(&self, _t: Option<Duration>) -> io::Result<()> {
        Err(unavailable())
    }
    pub fn peek(&self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(unavailable())
    }
    pub fn try_clone(&self) -> io::Result<TcpStream> {
        Err(unavailable())
    }
}

impl Read for TcpStream {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(unavailable())
    }
}

impl Read for &TcpStream {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(unavailable())
    }
}

impl Write for TcpStream {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(unavailable())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for &TcpStream {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(unavailable())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl TcpListener {
    pub fn bind(_address: impl ToSocketAddrs) -> io::Result<TcpListener> {
        Err(unavailable())
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Err(unavailable())
    }
    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        Err(unavailable())
    }
    pub fn set_nonblocking(&self, _on: bool) -> io::Result<()> {
        Err(unavailable())
    }
}

impl UdpSocket {
    pub fn bind(_address: impl ToSocketAddrs) -> io::Result<UdpSocket> {
        Err(unavailable())
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Err(unavailable())
    }
    pub fn connect(&self, _address: impl ToSocketAddrs) -> io::Result<()> {
        Err(unavailable())
    }
    pub fn send(&self, _buffer: &[u8]) -> io::Result<usize> {
        Err(unavailable())
    }
    pub fn send_to(&self, _buffer: &[u8], _address: impl ToSocketAddrs) -> io::Result<usize> {
        Err(unavailable())
    }
    pub fn recv(&self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(unavailable())
    }
    pub fn recv_from(&self, _buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        Err(unavailable())
    }
    pub fn peek(&self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(unavailable())
    }
    pub fn set_nonblocking(&self, _on: bool) -> io::Result<()> {
        Err(unavailable())
    }
    pub fn set_broadcast(&self, _on: bool) -> io::Result<()> {
        Err(unavailable())
    }
    pub fn set_read_timeout(&self, _t: Option<Duration>) -> io::Result<()> {
        Err(unavailable())
    }
}

#[allow(dead_code)]
fn _unused(_: Vec<u8>) {}
