//! TCP/IP: smoltcp over the network card, its address from DHCP, names
//! from the name servers DHCP names. Connections to the machine itself
//! (127.0.0.0/8) never reach the card: they are pipes in memory.
//!
//! A task polls the stack every millisecond; every socket operation polls
//! it too. Nothing here waits for the network except `resolve`.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};

use otw_vm::platform::{NetError, Network, SocketId};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, dns, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint};

use crate::sync::SpinLock;
use crate::nic::Nic;
use crate::{apic, sched};

const TCP_RECEIVE: usize = 256 * 1024;
const TCP_SEND: usize = 64 * 1024;
const LOOPBACK_LIMIT: usize = 256 * 1024;

struct Card(Box<dyn Nic>);

struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut dyn Nic);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, length: usize, f: F) -> R {
        let mut frame = alloc::vec![0u8; length];
        let result = f(&mut frame);
        self.0.send(&frame);
        result
    }
}

impl Device for Card {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx<'_>)> {
        if !self.0.can_send() {
            return None;
        }
        let frame = self.0.receive()?;
        Some((Rx(frame), Tx(&mut *self.0)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        self.0.can_send().then(|| Tx(&mut *self.0))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

/// One direction of a connection to the machine itself.
#[derive(Default)]
struct Pipe {
    data: VecDeque<u8>,
    closed: bool,
}

/// A connection to the machine itself: what it reads, what it writes.
struct LocalStream {
    incoming: Arc<SpinLock<Pipe>>,
    outgoing: Arc<SpinLock<Pipe>>,
    local: SocketAddr,
    peer: SocketAddr,
}

enum Entry {
    Tcp { handle: SocketHandle, peer: SocketAddr },
    Local(LocalStream),
    /// Listening on the machine itself (connections from elsewhere are not
    /// taken).
    Listener { port: u16, pending: VecDeque<(LocalStream, SocketAddr)> },
    Udp { handle: Option<SocketHandle>, port: u16, peer: Option<SocketAddr>, local: VecDeque<(Vec<u8>, SocketAddr)> },
}

struct Stack {
    card: Card,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    dns: SocketHandle,
    address: Option<Ipv4Addr>,
    entries: BTreeMap<SocketId, Entry>,
    next_id: SocketId,
    next_port: u16,
    /// Streams closed by the program, kept until they finish closing.
    closing: Vec<SocketHandle>,
    reported: u64,
}

static STACK: SpinLock<Option<Stack>> = SpinLock::new(None);

fn now() -> Instant {
    Instant::from_micros(apic::now_us() as i64)
}

fn is_local(ip: IpAddr) -> bool {
    ip.is_loopback()
}

impl Stack {
    fn poll(&mut self) {
        self.iface.poll(now(), &mut self.card, &mut self.sockets);
        let event = match self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).poll() {
            Some(dhcpv4::Event::Configured(config)) => {
                Some(Some((config.address, config.router, config.dns_servers.iter().copied().collect::<Vec<_>>())))
            }
            Some(dhcpv4::Event::Deconfigured) => Some(None),
            None => None,
        };
        match event {
            Some(Some((address, router, dns_servers))) => {
                self.iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(address));
                });
                if let Some(router) = router {
                    let _ = self.iface.routes_mut().add_default_ipv4_route(router);
                } else {
                    self.iface.routes_mut().remove_default_ipv4_route();
                }
                let servers: Vec<IpAddress> = dns_servers.iter().map(|s| IpAddress::Ipv4(*s)).collect();
                self.sockets.get_mut::<dns::Socket>(self.dns).update_servers(&servers);
                self.address = Some(address.address());
                crate::log!(
                    "OTW kernel: network address {} (router {:?}, name servers {:?})",
                    address,
                    router,
                    dns_servers
                );
            }
            Some(None) => {
                self.iface.update_ip_addrs(|addrs| addrs.clear());
                self.iface.routes_mut().remove_default_ipv4_route();
                self.address = None;
            }
            None => {}
        }
        if self.address.is_none() && apic::now_us() / 5_000_000 != self.reported {
            self.reported = apic::now_us() / 5_000_000;
            let (received, sent) = self.card.0.counters();
            let link = if self.card.0.link_up() { "link up" } else { "no link" };
            crate::log!("OTW kernel: network: no address yet ({link}, {received} frames received, {sent} sent)");
        }
        // Closed streams the program let go of.
        let sockets = &mut self.sockets;
        self.closing.retain(|&handle| {
            if sockets.get::<tcp::Socket>(handle).state() == tcp::State::Closed {
                sockets.remove(handle);
                false
            } else {
                true
            }
        });
    }

    fn add(&mut self, entry: Entry) -> SocketId {
        self.next_id += 1;
        self.entries.insert(self.next_id, entry);
        self.next_id
    }

    fn ephemeral(&mut self) -> u16 {
        self.next_port = if self.next_port >= 65000 { 49152 } else { self.next_port + 1 };
        self.next_port
    }

    fn local_ip(&self) -> Ipv4Addr {
        self.address.unwrap_or(Ipv4Addr::UNSPECIFIED)
    }
}

/// Find the network card and start DHCP; the stack is polled from then on.
pub fn init(devices: &[crate::pci::Device]) -> bool {
    let Some(card) = crate::nic::find(devices) else {
        return false;
    };
    let mac = card.mac();
    let name = card.name();
    let mut card = Card(card);
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    config.random_seed = apic::now_us() ^ unsafe { core::arch::x86_64::_rdtsc() };
    let iface = Interface::new(config, &mut card, now());
    let mut sockets = SocketSet::new(Vec::new());
    let dhcp = sockets.add(dhcpv4::Socket::new());
    let dns = sockets.add(dns::Socket::new(&[], Vec::new()));
    crate::log!(
        "OTW kernel: network card ({}) {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        name,
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    *STACK.lock() = Some(Stack {
        card,
        iface,
        sockets,
        dhcp,
        dns,
        address: None,
        entries: BTreeMap::new(),
        next_id: 0,
        next_port: 49152 + (apic::now_us() % 10000) as u16,
        closing: Vec::new(),
        reported: 0,
    });
    sched::spawn(|| loop {
        let _tag = crate::profile::on().then(|| sched::tagged(crate::profile::tag("[network]")));
        if let Some(stack) = STACK.lock().as_mut() {
            stack.poll();
        }
        sched::sleep_us(1000);
    });
    true
}

/// Run `f` on the stack, polled before and after.
fn with<T>(f: impl FnOnce(&mut Stack) -> Result<T, NetError>) -> Result<T, NetError> {
    let mut guard = STACK.lock();
    let stack = guard.as_mut().ok_or(NetError::Unreachable)?;
    stack.poll();
    let result = f(stack);
    stack.poll();
    result
}

fn endpoint(address: SocketAddr) -> Result<IpEndpoint, NetError> {
    match address {
        SocketAddr::V4(v4) => Ok(IpEndpoint::new(IpAddress::Ipv4(*v4.ip()), v4.port())),
        SocketAddr::V6(_) => Err(NetError::Unreachable),
    }
}

fn from_endpoint(endpoint: IpEndpoint) -> SocketAddr {
    match endpoint.addr {
        IpAddress::Ipv4(ip) => SocketAddr::V4(SocketAddrV4::new(ip, endpoint.port)),
    }
}

/// Whether there is a network.
pub fn available() -> bool {
    STACK.lock().is_some()
}

pub struct Net;

impl Network for Net {
    fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError> {
        if host.eq_ignore_ascii_case("localhost") {
            return Ok(alloc::vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
        }
        // Wait for an address first (DHCP), a few seconds at most.
        let deadline = apic::now_us() + 10_000_000;
        let query = loop {
            let started = with(|stack| {
                if stack.address.is_none() {
                    return Err(NetError::WouldBlock);
                }
                let dns = stack.sockets.get_mut::<dns::Socket>(stack.dns);
                dns.start_query(stack.iface.context(), host, smoltcp::wire::DnsQueryType::A).map_err(|_| NetError::NotFound)
            });
            match started {
                Err(NetError::WouldBlock) if apic::now_us() < deadline => sched::sleep_us(5000),
                other => break other?,
            }
        };
        loop {
            let result = with(|stack| {
                let dns = stack.sockets.get_mut::<dns::Socket>(stack.dns);
                match dns.get_query_result(query) {
                    Ok(addresses) => Ok(addresses
                        .iter()
                        .map(|a| match a {
                            IpAddress::Ipv4(ip) => IpAddr::V4(*ip),
                        })
                        .collect::<Vec<_>>()),
                    Err(dns::GetQueryResultError::Pending) => Err(NetError::WouldBlock),
                    Err(_) => Err(NetError::NotFound),
                }
            });
            match result {
                Err(NetError::WouldBlock) => sched::sleep_us(1000),
                other => return other,
            }
        }
    }

    fn tcp_connect(&self, to: SocketAddr) -> Result<SocketId, NetError> {
        with(|stack| {
            if is_local(to.ip()) {
                // A pipe to a listener here.
                let port = stack.ephemeral();
                let local = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
                let listener = stack.entries.values_mut().find_map(|e| match e {
                    Entry::Listener { port: p, pending } if *p == to.port() => Some(pending),
                    _ => None,
                });
                let Some(pending) = listener else {
                    return Err(NetError::Refused);
                };
                let (a, b) = (Arc::new(SpinLock::new(Pipe::default())), Arc::new(SpinLock::new(Pipe::default())));
                pending.push_back((LocalStream { incoming: b.clone(), outgoing: a.clone(), local: to, peer: local }, local));
                return Ok(stack.add(Entry::Local(LocalStream { incoming: a, outgoing: b, local, peer: to })));
            }
            if stack.address.is_none() {
                return Err(NetError::WouldBlock);
            }
            let mut socket = tcp::Socket::new(
                tcp::SocketBuffer::new(alloc::vec![0; TCP_RECEIVE]),
                tcp::SocketBuffer::new(alloc::vec![0; TCP_SEND]),
            );
            socket.set_nagle_enabled(false);
            let port = stack.ephemeral();
            socket.connect(stack.iface.context(), endpoint(to)?, port).map_err(|_| NetError::Unreachable)?;
            let handle = stack.sockets.add(socket);
            Ok(stack.add(Entry::Tcp { handle, peer: to }))
        })
        .or_else(|e| {
            // No address yet: wait for DHCP a little.
            if e == NetError::WouldBlock {
                let deadline = apic::now_us() + 10_000_000;
                while apic::now_us() < deadline {
                    sched::sleep_us(5000);
                    if STACK.lock().as_ref().is_some_and(|s| s.address.is_some()) {
                        return self.tcp_connect(to);
                    }
                }
                return Err(NetError::Unreachable);
            }
            Err(e)
        })
    }

    fn tcp_connected(&self, socket: SocketId) -> Result<(), NetError> {
        with(|stack| match stack.entries.get(&socket) {
            Some(Entry::Local(_)) => Ok(()),
            Some(Entry::Tcp { handle, .. }) => match stack.sockets.get::<tcp::Socket>(*handle).state() {
                tcp::State::SynSent | tcp::State::SynReceived => Err(NetError::WouldBlock),
                tcp::State::Closed => Err(NetError::Refused),
                _ => Ok(()),
            },
            _ => Err(NetError::Invalid),
        })
    }

    fn tcp_listen(&self, at: SocketAddr) -> Result<SocketId, NetError> {
        with(|stack| {
            let port = if at.port() == 0 { stack.ephemeral() } else { at.port() };
            if stack.entries.values().any(|e| matches!(e, Entry::Listener { port: p, .. } if *p == port)) {
                return Err(NetError::AddrInUse);
            }
            Ok(stack.add(Entry::Listener { port, pending: VecDeque::new() }))
        })
    }

    fn tcp_accept(&self, listener: SocketId) -> Result<(SocketId, SocketAddr), NetError> {
        with(|stack| {
            let accepted = match stack.entries.get_mut(&listener) {
                Some(Entry::Listener { pending, .. }) => pending.pop_front().ok_or(NetError::WouldBlock)?,
                _ => return Err(NetError::Invalid),
            };
            let (stream, from) = accepted;
            Ok((stack.add(Entry::Local(stream)), from))
        })
    }

    fn udp_bind(&self, at: SocketAddr) -> Result<SocketId, NetError> {
        with(|stack| {
            let port = if at.port() == 0 { stack.ephemeral() } else { at.port() };
            let handle = if is_local(at.ip()) {
                None
            } else {
                let mut socket = udp::Socket::new(
                    udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 64], alloc::vec![0; 64 * 1024]),
                    udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 64], alloc::vec![0; 64 * 1024]),
                );
                socket.bind(IpListenEndpoint { addr: None, port }).map_err(|_| NetError::AddrInUse)?;
                Some(stack.sockets.add(socket))
            };
            Ok(stack.add(Entry::Udp { handle, port, peer: None, local: VecDeque::new() }))
        })
    }

    fn udp_connect(&self, socket: SocketId, to: SocketAddr) -> Result<(), NetError> {
        with(|stack| match stack.entries.get_mut(&socket) {
            Some(Entry::Udp { peer, .. }) => {
                *peer = Some(to);
                Ok(())
            }
            _ => Err(NetError::Invalid),
        })
    }

    fn send(&self, socket: SocketId, data: &[u8], to: Option<SocketAddr>) -> Result<usize, NetError> {
        with(|stack| {
            let local_ip = stack.local_ip();
            match stack.entries.get_mut(&socket) {
                Some(Entry::Local(stream)) => {
                    let mut pipe = stream.outgoing.lock();
                    if pipe.closed {
                        return Err(NetError::Reset);
                    }
                    let room = LOOPBACK_LIMIT.saturating_sub(pipe.data.len());
                    if room == 0 {
                        return Err(NetError::WouldBlock);
                    }
                    let n = room.min(data.len());
                    pipe.data.extend(&data[..n]);
                    Ok(n)
                }
                Some(Entry::Tcp { handle, .. }) => {
                    let s = stack.sockets.get_mut::<tcp::Socket>(*handle);
                    if !s.may_send() {
                        return Err(if s.state() == tcp::State::SynSent { NetError::WouldBlock } else { NetError::Reset });
                    }
                    match s.send_slice(data) {
                        Ok(0) => Err(NetError::WouldBlock),
                        Ok(n) => Ok(n),
                        Err(_) => Err(NetError::Reset),
                    }
                }
                Some(Entry::Udp { handle, port, peer, .. }) => {
                    let to = to.or(*peer).ok_or(NetError::NotConnected)?;
                    let from_port = *port;
                    if is_local(to.ip()) {
                        let from = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, from_port));
                        if let Some(Entry::Udp { local, .. }) =
                            stack.entries.values_mut().find(|e| matches!(e, Entry::Udp { port, .. } if *port == to.port()))
                        {
                            local.push_back((data.to_vec(), from));
                        }
                        return Ok(data.len());
                    }
                    let _ = local_ip;
                    let handle = handle.ok_or(NetError::Unreachable)?;
                    let s = stack.sockets.get_mut::<udp::Socket>(handle);
                    s.send_slice(data, endpoint(to)?).map_err(|_| NetError::WouldBlock)?;
                    Ok(data.len())
                }
                _ => Err(NetError::Invalid),
            }
        })
    }

    fn recv(&self, socket: SocketId, buffer: &mut [u8], peek: bool) -> Result<(usize, Option<SocketAddr>), NetError> {
        with(|stack| match stack.entries.get_mut(&socket) {
            Some(Entry::Local(stream)) => {
                let mut pipe = stream.incoming.lock();
                if pipe.data.is_empty() {
                    return if pipe.closed { Ok((0, None)) } else { Err(NetError::WouldBlock) };
                }
                let n = buffer.len().min(pipe.data.len());
                for (i, byte) in pipe.data.iter().take(n).enumerate() {
                    buffer[i] = *byte;
                }
                if !peek {
                    pipe.data.drain(..n);
                }
                Ok((n, None))
            }
            Some(Entry::Tcp { handle, .. }) => {
                let s = stack.sockets.get_mut::<tcp::Socket>(*handle);
                if s.can_recv() {
                    let n = if peek { s.peek_slice(buffer) } else { s.recv_slice(buffer) }.map_err(|_| NetError::Reset)?;
                    return Ok((n, None));
                }
                match s.state() {
                    tcp::State::SynSent | tcp::State::SynReceived => Err(NetError::WouldBlock),
                    tcp::State::Closed if !s.may_recv() => Ok((0, None)),
                    _ if !s.may_recv() => Ok((0, None)),
                    _ => Err(NetError::WouldBlock),
                }
            }
            Some(Entry::Udp { handle, local, .. }) => {
                if let Some((data, from)) = local.front() {
                    let n = buffer.len().min(data.len());
                    buffer[..n].copy_from_slice(&data[..n]);
                    let from = *from;
                    if !peek {
                        local.pop_front();
                    }
                    return Ok((n, Some(from)));
                }
                let Some(handle) = *handle else {
                    return Err(NetError::WouldBlock);
                };
                let s = stack.sockets.get_mut::<udp::Socket>(handle);
                let received = if peek {
                    s.peek().map(|(data, meta)| {
                        let n = buffer.len().min(data.len());
                        buffer[..n].copy_from_slice(&data[..n]);
                        (n, meta.endpoint)
                    })
                } else {
                    s.recv_slice(buffer).map(|(n, meta)| (n, meta.endpoint))
                };
                match received {
                    Ok((n, from)) => Ok((n, Some(from_endpoint(from)))),
                    Err(_) => Err(NetError::WouldBlock),
                }
            }
            _ => Err(NetError::Invalid),
        })
    }

    fn local_addr(&self, socket: SocketId) -> Result<SocketAddr, NetError> {
        with(|stack| {
            let ip = stack.local_ip();
            match stack.entries.get(&socket) {
                Some(Entry::Local(stream)) => Ok(stream.local),
                Some(Entry::Tcp { handle, .. }) => stack
                    .sockets
                    .get::<tcp::Socket>(*handle)
                    .local_endpoint()
                    .map(from_endpoint)
                    .ok_or(NetError::NotConnected),
                Some(Entry::Listener { port, .. }) => Ok(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, *port))),
                Some(Entry::Udp { handle, port, .. }) => {
                    let ip = if handle.is_some() { ip } else { Ipv4Addr::LOCALHOST };
                    Ok(SocketAddr::V4(SocketAddrV4::new(ip, *port)))
                }
                None => Err(NetError::Invalid),
            }
        })
    }

    fn peer_addr(&self, socket: SocketId) -> Result<SocketAddr, NetError> {
        with(|stack| match stack.entries.get(&socket) {
            Some(Entry::Local(stream)) => Ok(stream.peer),
            Some(Entry::Tcp { peer, .. }) => Ok(*peer),
            Some(Entry::Udp { peer: Some(peer), .. }) => Ok(*peer),
            _ => Err(NetError::NotConnected),
        })
    }

    fn shutdown(&self, socket: SocketId) {
        let _ = with(|stack| {
            match stack.entries.get(&socket) {
                Some(Entry::Local(stream)) => stream.outgoing.lock().closed = true,
                Some(Entry::Tcp { handle, .. }) => stack.sockets.get_mut::<tcp::Socket>(*handle).close(),
                _ => {}
            }
            Ok(())
        });
    }

    fn close(&self, socket: SocketId) {
        let _ = with(|stack| {
            match stack.entries.remove(&socket) {
                Some(Entry::Local(stream)) => {
                    stream.outgoing.lock().closed = true;
                    stream.incoming.lock().closed = true;
                }
                Some(Entry::Tcp { handle, .. }) => {
                    stack.sockets.get_mut::<tcp::Socket>(handle).close();
                    stack.closing.push(handle);
                }
                Some(Entry::Udp { handle: Some(handle), .. }) => {
                    stack.sockets.remove(handle);
                }
                _ => {}
            }
            Ok(())
        });
    }
}
