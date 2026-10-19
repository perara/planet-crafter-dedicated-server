//! Unity Transport 2.x over UDP: what the game's UnityTransport speaks for direct-IP joins.
//!
//! Handshake (SimpleConnectionLayer): "UTP" + version 1, request (1) or accept (2), and the
//! client's 8-byte connection token. After that every datagram is a message type (1 data,
//! 2 disconnect, 3 heartbeat) and the token; data adds a pipeline id and its stage headers.
//! Netcode creates three pipelines, in this order: 1 fragmentation, 2 fragmentation plus
//! unreliable sequenced, 3 reliable sequenced. Netcode batches travel with an i32 length
//! prefix: as one byte stream on the reliable pipeline, as whole messages per datagram on the
//! others. Everything this server sends goes over the reliable pipeline.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const SIGNATURE: [u8; 4] = [b'U', b'T', b'P', 1];
const REQUEST: u8 = 1;
const ACCEPT: u8 = 2;
const DATA: u8 = 1;
const DISCONNECT: u8 = 2;
const HEARTBEAT: u8 = 3;
const RELIABLE: u8 = 3;
const PAYLOAD: u8 = 0;
const ACK: u8 = 1;
/// Netcode's reliable window (WithReliableStageParameters(64, ...)): packets in flight.
const WINDOW: usize = 64;
/// Reliable bytes per datagram, well under the default 1400-byte maximum message size.
const CHUNK: usize = 1200;
const RESEND: Duration = Duration::from_millis(200);
/// Nothing a client could not have sent proves it receives at its address (the token is its
/// own, the server's sequence starts at 0), so a spoofed source must get little: the window
/// starts at INITIAL_WINDOW packets and grows by what is acknowledged; resends back off up to
/// RESEND << MAX_BACKOFF; data unacknowledged for STALL drops the session. STALL is long
/// enough for a game client that stops acking while it builds a large world on join.
const INITIAL_WINDOW: usize = 4;
const MAX_BACKOFF: u32 = 4;
const STALL: Duration = Duration::from_secs(15);
/// How long a session closed from this side may take to deliver what was queued (a kick's
/// reason) before its disconnect goes out.
const LINGER: Duration = Duration::from_secs(2);
const HEARTBEAT_AFTER: Duration = Duration::from_millis(500);
const TIMEOUT: Duration = Duration::from_secs(30);
const FIRST: u16 = 0x8000;
const LAST: u16 = 0x4000;
/// Hostile-client limits: one batch (or reassembled unreliable message), bytes waiting to be
/// sent, datagrams read per poll, connections in all and per IP, and how long a handshake may
/// go without data.
const MAX_BATCH: usize = 1 << 20;
// Room for the world a late-game save sends on join (saves reach several MB); only approved
// clients (the limits.maxPlayers rule) get that much sent.
const MAX_QUEUED: usize = 64 << 20;
const MAX_DATAGRAMS: usize = 4096;
const MAX_CONNECTIONS: usize = 128;
const MAX_PER_IP: usize = 8;
const DATA_WITHIN: Duration = Duration::from_secs(10);
/// New sessions per IP within HANDSHAKE_WINDOW. Loopback addresses are exempt from the per-IP
/// limits: nobody outside can forge them, and behind a UDP relay every player has one.
const MAX_HANDSHAKES: u32 = 4;
const HANDSHAKE_WINDOW: Duration = Duration::from_secs(10);

/// Loopback, also as an IPv4-mapped IPv6 address.
pub fn is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

pub type Token = [u8; 8];

pub enum Event {
    Connected(Token, SocketAddr),
    Data(Token, Vec<u8>),
    Disconnected(Token),
}

#[derive(Default)]
struct Fragments {
    buffer: Vec<u8>,
    next: u16,
    broken: bool,
}

struct Connection {
    /// Fixed at the handshake: datagrams from anywhere else are dropped.
    addr: SocketAddr,
    opened: Instant,
    /// Until the client sent data we send it nothing (a spoofed handshake reflects nothing).
    data_seen: bool,
    last_receive: Instant,
    last_send: Instant,
    /// Reliable receive: highest sequence seen and which of the 64 before it arrived (bit i
    /// is `received - i`), the next sequence to deliver, early arrivals, the byte stream.
    received: u16,
    mask: u64,
    next: u16,
    early: HashMap<u16, Vec<u8>>,
    stream: Vec<u8>,
    ack_due: bool,
    /// Reliable send: next sequence, packets not yet acknowledged, bytes waiting for room.
    sequence: u16,
    in_flight: VecDeque<(u16, Vec<u8>, Instant)>,
    queued: VecDeque<u8>,
    fragments: [Fragments; 2],
    /// Packets allowed in flight, when the in-flight ones were last acknowledged (or the first
    /// of them sent), and resend rounds since.
    window: usize,
    progress: Instant,
    backoff: u32,
    /// Closed from this side: the disconnect goes out once everything is acknowledged, or then.
    closing: Option<Instant>,
}

impl Connection {
    fn new(addr: SocketAddr, now: Instant) -> Connection {
        Connection {
            addr,
            opened: now,
            data_seen: false,
            last_receive: now,
            last_send: now,
            received: u16::MAX,
            mask: u64::MAX,
            next: 0,
            early: HashMap::new(),
            stream: Vec::new(),
            ack_due: false,
            sequence: 0,
            in_flight: VecDeque::new(),
            queued: VecDeque::new(),
            fragments: Default::default(),
            window: INITIAL_WINDOW,
            progress: now,
            backoff: 0,
            closing: None,
        }
    }

    /// Reliable header: type, ack mask length (0 = 8 bytes for a 64 window), processing time,
    /// sequence, then what we received: highest sequence and mask.
    fn reliable_header(&self, kind: u8, sequence: u16) -> Vec<u8> {
        let mut h = vec![kind, 0, 0, 0];
        h.extend_from_slice(&sequence.to_le_bytes());
        h.extend_from_slice(&self.received.to_le_bytes());
        h.extend_from_slice(&self.mask.to_le_bytes());
        h
    }

    /// Records a reliable payload; in-order bytes go to `stream`, early ones wait.
    fn receive_reliable(&mut self, sequence: u16, payload: &[u8]) {
        // The sender's window starts at or before `next` (unreceived, so unacked): anything
        // WINDOW or more ahead of it is hostile, neither buffered nor acknowledged.
        let early = sequence.wrapping_sub(self.next) as i16;
        if early >= WINDOW as i16 {
            return;
        }
        let ahead = sequence.wrapping_sub(self.received) as i16;
        if ahead > 0 {
            self.mask = if ahead >= 64 { 1 } else { (self.mask << ahead) | 1 };
            self.received = sequence;
        } else {
            let back = -(ahead as i32) as u32;
            if back >= 64 || self.mask & (1 << back) != 0 {
                return;
            }
            self.mask |= 1 << back;
        }
        if sequence == self.next {
            self.stream.extend_from_slice(payload);
            self.next = self.next.wrapping_add(1);
            while let Some(p) = self.early.remove(&self.next) {
                self.stream.extend_from_slice(&p);
                self.next = self.next.wrapping_add(1);
            }
        } else if early > 0 {
            self.early.insert(sequence, payload.to_vec());
        }
    }

    /// False for an ack of a packet not sent yet: no client sends one, a blind spoofer may.
    fn acknowledge(&mut self, acked: u16, mask: u64, now: Instant) -> bool {
        if acked.wrapping_sub(self.sequence.wrapping_sub(1)) as i16 > 0 {
            return false;
        }
        let before = self.in_flight.len();
        self.in_flight.retain(|(s, _, _)| {
            let back = acked.wrapping_sub(*s) as i16;
            !((0..64).contains(&back) && mask & (1 << back) != 0)
        });
        if self.in_flight.len() < before {
            self.window = (self.window + before - self.in_flight.len()).min(WINDOW);
            self.progress = now;
            self.backoff = 0;
        }
        true
    }
}

/// Splits off every complete length-prefixed batch at the front of `buffer`; false when one
/// claims more than MAX_BATCH (the caller drops what is left).
fn pop_batches(buffer: &mut Vec<u8>, out: &mut Vec<Vec<u8>>) -> bool {
    let mut at = 0;
    while buffer.len() - at >= 4 {
        let n = i32::from_le_bytes(buffer[at..at + 4].try_into().unwrap()).max(0) as usize;
        if n > MAX_BATCH {
            return false;
        }
        if buffer.len() - at - 4 < n {
            break;
        }
        // An empty (or negative) length carries nothing: no empty batch for each.
        if n > 0 {
            out.push(buffer[at + 4..at + 4 + n].to_vec());
        }
        at += 4 + n;
    }
    buffer.drain(..at);
    true
}

pub struct Utp {
    socket: UdpSocket,
    connections: crate::idmap::FxHashMap<Token, Connection>,
    /// Dropped by send() (over MAX_QUEUED); the next poll reports them.
    dropped: Vec<Token>,
    /// Datagrams to send, all in one system call at the end of flush.
    outbox: Vec<(SocketAddr, Vec<u8>)>,
    inbox: mmsg::Inbox,
    /// New sessions per IP: since when, how many.
    handshakes: HashMap<IpAddr, (Instant, u32)>,
}

impl Utp {
    pub fn bind(addr: SocketAddr) -> std::io::Result<Utp> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        Ok(Utp { socket, connections: Default::default(), dropped: Vec::new(), outbox: Vec::new(), inbox: mmsg::Inbox::new(), handshakes: HashMap::new() })
    }

    fn transmit(&mut self, token: &Token, kind: u8, body: &[u8]) {
        let Some(c) = self.connections.get_mut(token) else { return };
        self.outbox.push((c.addr, packet(token, kind, &[body])));
        c.last_send = Instant::now();
    }

    /// Client side: connects to a server from `local` and returns once it accepted, with our
    /// token.
    pub fn connect(local: SocketAddr, server: SocketAddr, timeout: Duration) -> std::io::Result<(Utp, Token)> {
        let mut utp = Utp::bind(local)?;
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        let port = utp.socket.local_addr().map(|a| a.port() as u64).unwrap_or(0);
        let token: Token = (seed ^ (std::process::id() as u64).rotate_left(32) ^ port.rotate_left(16)).to_le_bytes();
        let mut request = SIGNATURE.to_vec();
        request.push(REQUEST);
        request.extend_from_slice(&token);
        let deadline = Instant::now() + timeout;
        let mut buffer = [0u8; 2048];
        while Instant::now() < deadline {
            utp.socket.send_to(&request, server)?;
            let wait = Instant::now() + Duration::from_millis(500);
            while Instant::now() < wait {
                if let Ok((n, from)) = utp.socket.recv_from(&mut buffer) {
                    if from == server && n >= 13 && buffer[..4] == SIGNATURE && buffer[4] == ACCEPT && buffer[5..13] == token {
                        let mut c = Connection::new(server, Instant::now());
                        c.data_seen = true;
                        utp.connections.insert(token, c);
                        return Ok((utp, token));
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer from the server"))
    }

    /// Reads up to MAX_DATAGRAMS waiting datagrams; also reports connections that went silent,
    /// sent no data in time after their handshake, or that send() dropped.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events: Vec<Event> = self.dropped.drain(..).map(Event::Disconnected).collect();
        let now = Instant::now();
        let mut inbox = std::mem::take(&mut self.inbox);
        let mut read = 0;
        while read < MAX_DATAGRAMS {
            let n = inbox.receive(&self.socket);
            for (d, from) in (0..n).filter_map(|i| inbox.get(i)) {
                self.datagram(d, from, now, &mut events);
            }
            read += n;
            if n < inbox.capacity() {
                break;
            }
        }
        self.inbox = inbox;
        let stalled = |c: &Connection| c.closing.is_none() && !c.in_flight.is_empty() && now - c.progress > STALL;
        let silent = |c: &Connection| now - c.last_receive > TIMEOUT || (!c.data_seen && now - c.opened > DATA_WITHIN) || stalled(c);
        let silent: Vec<Token> = if self.connections.values().any(silent) { self.connections.iter().filter(|(_, c)| silent(c)).map(|(t, _)| *t).collect() } else { Vec::new() };
        for token in silent {
            if self.connections.get(&token).is_some_and(stalled) {
                eprintln!("utp: {}: nothing acknowledged for {} s, dropped", self.connections[&token].addr, STALL.as_secs());
                self.disconnect(&token);
            }
            self.connections.remove(&token);
            events.push(Event::Disconnected(token));
        }
        events
    }

    fn datagram(&mut self, d: &[u8], from: SocketAddr, now: Instant, events: &mut Vec<Event>) {
        if d.len() >= 13 && d[..3] == SIGNATURE[..3] {
            let token: Token = d[5..13].try_into().unwrap();
            if d[3] != SIGNATURE[3] || d[4] != REQUEST {
                return;
            }
            match self.connections.get(&token) {
                Some(c) if c.addr != from => return,
                Some(_) => {}
                None => {
                    let local = is_loopback(from.ip());
                    let same_ip = self.connections.values().filter(|c| c.addr.ip() == from.ip()).count();
                    if self.connections.len() >= MAX_CONNECTIONS || (!local && same_ip >= MAX_PER_IP) {
                        return;
                    }
                    if !local {
                        let (since, n) = self.handshakes.entry(from.ip()).or_insert((now, 0));
                        if now - *since > HANDSHAKE_WINDOW {
                            (*since, *n) = (now, 0);
                        }
                        if *n >= MAX_HANDSHAKES {
                            return;
                        }
                        *n += 1;
                        // Sessions are capped, so this holds at most the addresses of the last window.
                        if self.handshakes.len() > 4 * MAX_CONNECTIONS {
                            self.handshakes.retain(|_, (t, _)| now - *t <= HANDSHAKE_WINDOW);
                        }
                    }
                    self.connections.insert(token, Connection::new(from, now));
                    events.push(Event::Connected(token, from));
                }
            }
            let mut accept = SIGNATURE.to_vec();
            accept.push(ACCEPT);
            accept.extend_from_slice(&token);
            let _ = self.socket.send_to(&accept, from);
            return;
        }
        if d.len() < 9 {
            return;
        }
        let token: Token = d[1..9].try_into().unwrap();
        let Some(c) = self.connections.get_mut(&token).filter(|c| c.addr == from) else { return };
        c.last_receive = now;
        c.data_seen |= d[0] == DATA;
        match d[0] {
            DISCONNECT => {
                self.connections.remove(&token);
                events.push(Event::Disconnected(token));
            }
            DATA if d.len() > 10 => {
                let mut batches = Vec::new();
                let body = &d[10..];
                match d[9] {
                    RELIABLE if body.len() >= 16 => {
                        let sequence = u16::from_le_bytes([body[4], body[5]]);
                        let acked = u16::from_le_bytes([body[6], body[7]]);
                        if !c.acknowledge(acked, u64::from_le_bytes(body[8..16].try_into().unwrap()), now) {
                            eprintln!("utp: {from}: acknowledged sequence {acked}, which was never sent; dropped");
                            self.disconnect(&token);
                            events.push(Event::Disconnected(token));
                            return;
                        }
                        if body[0] == PAYLOAD {
                            c.receive_reliable(sequence, &body[16..]);
                            c.ack_due = true;
                            if !pop_batches(&mut c.stream, &mut batches) {
                                eprintln!("utp: {from}: a batch over {} MiB, dropped", MAX_BATCH >> 20);
                                self.disconnect(&token);
                                events.push(Event::Disconnected(token));
                                return;
                            }
                        }
                    }
                    // Unreliable: an optional 2-byte sequence (pipeline 2), then a fragment header.
                    pipeline @ (1 | 2) => {
                        let body = if pipeline == 2 { &body[2.min(body.len())..] } else { body };
                        if body.len() >= 2 {
                            let header = u16::from_le_bytes([body[0], body[1]]);
                            let f = &mut c.fragments[pipeline as usize - 1];
                            let sequence = header & 0x3FFF;
                            if header & FIRST != 0 {
                                *f = Fragments { buffer: Vec::new(), next: sequence, broken: false };
                            }
                            if sequence != f.next {
                                f.broken = true;
                            }
                            f.next = (sequence + 1) & 0x3FFF;
                            f.broken |= f.buffer.len() + body.len() - 2 > MAX_BATCH;
                            if f.broken {
                                f.buffer = Vec::new();
                            } else {
                                f.buffer.extend_from_slice(&body[2..]);
                                if header & LAST != 0 {
                                    let mut whole = std::mem::take(&mut f.buffer);
                                    pop_batches(&mut whole, &mut batches);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                events.extend(batches.into_iter().map(|b| Event::Data(token, b)));
            }
            _ => {}
        }
    }

    /// Queues one Netcode batch on the reliable stream; a client that lets more than
    /// MAX_QUEUED pile up (never acks) is dropped, and the next poll reports it.
    pub fn send(&mut self, token: &Token, batch: &[u8]) {
        let Some(c) = self.connections.get_mut(token) else { return };
        if c.queued.len() + batch.len() > MAX_QUEUED {
            eprintln!("utp: {}: over {} MiB waiting to be sent, dropped", c.addr, MAX_QUEUED >> 20);
            self.disconnect(token);
            self.dropped.push(*token);
            return;
        }
        c.queued.extend((batch.len() as i32).to_le_bytes());
        c.queued.extend(batch);
    }

    /// Sends what fits in the window, resends what was not acknowledged, acks, heartbeats.
    pub fn flush(&mut self) {
        let now = Instant::now();
        let mut done = Vec::new();
        for (token, c) in self.connections.iter_mut() {
            if !c.data_seen {
                continue;
            }
            if c.closing.is_some_and(|deadline| now >= deadline || (c.queued.is_empty() && c.in_flight.is_empty())) {
                done.push(*token);
                continue;
            }
            let before = self.outbox.len();
            let resend = RESEND * (1 << c.backoff.min(MAX_BACKOFF));
            for i in 0..c.in_flight.len() {
                if now - c.in_flight[i].2 >= resend {
                    c.in_flight[i].2 = now;
                    let header = c.reliable_header(PAYLOAD, c.in_flight[i].0);
                    self.outbox.push((c.addr, packet(token, DATA, &[&[RELIABLE], &header, &c.in_flight[i].1])));
                }
            }
            if self.outbox.len() > before {
                c.backoff += 1;
            }
            while c.in_flight.len() < c.window && !c.queued.is_empty() {
                if c.in_flight.is_empty() {
                    c.progress = now;
                }
                let payload: Vec<u8> = c.queued.drain(..CHUNK.min(c.queued.len())).collect();
                let sequence = c.sequence;
                c.sequence = c.sequence.wrapping_add(1);
                let header = c.reliable_header(PAYLOAD, sequence);
                self.outbox.push((c.addr, packet(token, DATA, &[&[RELIABLE], &header, &payload])));
                c.in_flight.push_back((sequence, payload, now));
            }
            if c.ack_due && self.outbox.len() == before {
                let header = c.reliable_header(ACK, 0);
                self.outbox.push((c.addr, packet(token, DATA, &[&[RELIABLE], &header, &[0]])));
            }
            c.ack_due = false;
            if self.outbox.len() == before && now - c.last_send > HEARTBEAT_AFTER {
                self.outbox.push((c.addr, packet(token, HEARTBEAT, &[])));
            }
            if self.outbox.len() > before {
                c.last_send = now;
            }
        }
        mmsg::send(&self.socket, &self.outbox);
        self.outbox.clear();
        for token in done {
            self.disconnect(&token);
        }
    }

    /// Closes a session from this side once what is queued for it (a kick's reason) is
    /// delivered, or after LINGER; nothing more is reported for it.
    pub fn close(&mut self, token: &Token) {
        if let Some(c) = self.connections.get_mut(token) {
            c.closing = Some(Instant::now() + LINGER);
        }
    }

    /// Tells the client it is disconnected and forgets it.
    pub fn disconnect(&mut self, token: &Token) {
        self.transmit(token, DISCONNECT, &[]);
        self.connections.remove(token);
        mmsg::send(&self.socket, &self.outbox);
        self.outbox.clear();
    }

    /// The socket, for a caller that waits on it together with other descriptors.
    pub fn fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.socket.as_raw_fd()
    }

    pub fn tokens(&self) -> Vec<Token> {
        self.connections.keys().copied().collect()
    }
}

/// A datagram: its type, the connection's token, then the parts.
fn packet(token: &Token, kind: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut d = Vec::with_capacity(9 + parts.iter().map(|p| p.len()).sum::<usize>());
    d.push(kind);
    d.extend_from_slice(token);
    parts.iter().for_each(|p| d.extend_from_slice(p));
    d
}

/// Linux's batched UDP calls: sendmmsg and recvmmsg move many datagrams per system call.
mod mmsg {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
    use std::os::fd::AsRawFd;

    /// Datagrams per call.
    pub const BATCH: usize = 32;

    fn raw(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
        // SAFETY: sockaddr_storage is plain data; zero is a valid value of every field.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let len = match addr {
            SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in { sin_family: libc::AF_INET as libc::sa_family_t, sin_port: a.port().to_be(), sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) }, sin_zero: [0; 8] };
                // SAFETY: sockaddr_storage is large and aligned enough for any socket address.
                unsafe { std::ptr::write(&mut storage as *mut _ as *mut libc::sockaddr_in, sin) };
                std::mem::size_of::<libc::sockaddr_in>()
            }
            SocketAddr::V6(a) => {
                let sin6 = libc::sockaddr_in6 { sin6_family: libc::AF_INET6 as libc::sa_family_t, sin6_port: a.port().to_be(), sin6_flowinfo: a.flowinfo(), sin6_addr: libc::in6_addr { s6_addr: a.ip().octets() }, sin6_scope_id: a.scope_id() };
                // SAFETY: as above.
                unsafe { std::ptr::write(&mut storage as *mut _ as *mut libc::sockaddr_in6, sin6) };
                std::mem::size_of::<libc::sockaddr_in6>()
            }
        };
        (storage, len as libc::socklen_t)
    }

    fn address(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
        // SAFETY: the family says which address type the kernel wrote.
        match storage.ss_family as libc::c_int {
            libc::AF_INET => {
                let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
                Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes()), u16::from_be(sin.sin_port))))
            }
            libc::AF_INET6 => {
                let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
                Some(SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(sin6.sin6_addr.s6_addr), u16::from_be(sin6.sin6_port), sin6.sin6_flowinfo, sin6.sin6_scope_id)))
            }
            _ => None,
        }
    }

    /// Sends every datagram; one the socket refuses is dropped, as a failed send_to drops it.
    pub fn send(socket: &UdpSocket, datagrams: &[(SocketAddr, Vec<u8>)]) {
        for chunk in datagrams.chunks(BATCH) {
            let mut names: Vec<_> = chunk.iter().map(|(a, _)| raw(a)).collect();
            let mut iovs: Vec<libc::iovec> = chunk.iter().map(|(_, d)| libc::iovec { iov_base: d.as_ptr() as *mut libc::c_void, iov_len: d.len() }).collect();
            let mut messages: Vec<libc::mmsghdr> = names
                .iter_mut()
                .zip(iovs.iter_mut())
                .map(|((name, len), iov)| {
                    // SAFETY: mmsghdr is plain data; the pointers set below outlive the call.
                    let mut m: libc::mmsghdr = unsafe { std::mem::zeroed() };
                    m.msg_hdr.msg_name = name as *mut _ as *mut libc::c_void;
                    m.msg_hdr.msg_namelen = *len;
                    m.msg_hdr.msg_iov = iov;
                    m.msg_hdr.msg_iovlen = 1;
                    m
                })
                .collect();
            let mut sent = 0;
            while sent < messages.len() {
                // SAFETY: messages[sent..] are valid mmsghdrs pointing at live names and buffers.
                let n = unsafe { libc::sendmmsg(socket.as_raw_fd(), messages.as_mut_ptr().add(sent), (messages.len() - sent) as libc::c_uint, 0) };
                if n > 0 {
                    sent += n as usize;
                } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    sent += 1;
                }
            }
        }
    }

    /// recvmmsg's buffers and headers, made once: each header points at its buffer and address.
    #[derive(Default)]
    pub struct Inbox {
        buffers: Vec<[u8; 2048]>,
        names: Vec<libc::sockaddr_storage>,
        iovs: Vec<libc::iovec>,
        headers: Vec<libc::mmsghdr>,
    }

    impl Inbox {
        pub fn new() -> Inbox {
            // SAFETY: plain data, filled by the kernel.
            let mut b = Inbox { buffers: vec![[0; 2048]; BATCH], names: vec![unsafe { std::mem::zeroed() }; BATCH], iovs: Vec::with_capacity(BATCH), headers: Vec::with_capacity(BATCH) };
            for buffer in &mut b.buffers {
                b.iovs.push(libc::iovec { iov_base: buffer.as_mut_ptr() as *mut libc::c_void, iov_len: buffer.len() });
            }
            for (name, iov) in b.names.iter_mut().zip(b.iovs.iter_mut()) {
                // SAFETY: as in send. The vectors never grow again, so the pointers stay valid.
                let mut m: libc::mmsghdr = unsafe { std::mem::zeroed() };
                m.msg_hdr.msg_name = name as *mut _ as *mut libc::c_void;
                m.msg_hdr.msg_iov = iov;
                m.msg_hdr.msg_iovlen = 1;
                b.headers.push(m);
            }
            b
        }

        /// Reads up to BATCH waiting datagrams; returns how many (0: none waits).
        pub fn receive(&mut self, socket: &UdpSocket) -> usize {
            for m in &mut self.headers {
                m.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            }
            // SAFETY: valid headers over live buffers; the socket is non-blocking.
            let n = unsafe { libc::recvmmsg(socket.as_raw_fd(), self.headers.as_mut_ptr(), self.headers.len() as libc::c_uint, 0, std::ptr::null_mut()) };
            n.max(0) as usize
        }

        /// Datagram i of the last receive, with its sender.
        pub fn get(&self, i: usize) -> Option<(&[u8], SocketAddr)> {
            Some((&self.buffers[i][..(self.headers[i].msg_len as usize).min(2048)], address(&self.names[i])?))
        }

        pub fn capacity(&self) -> usize {
            self.headers.len()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(port: u16) -> UdpSocket {
        let s = UdpSocket::bind(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        s
    }

    fn recv(s: &UdpSocket) -> Vec<u8> {
        let mut b = [0u8; 2048];
        let n = s.recv(&mut b).unwrap();
        b[..n].to_vec()
    }

    #[test]
    fn handshake_reliable_stream_and_acks() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.socket.local_addr().unwrap();
        let c = client(0);
        c.connect(addr).unwrap();
        let token = [1, 2, 3, 4, 5, 6, 7, 8];
        c.send(&[b'U', b'T', b'P', 1, REQUEST, 1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(server.poll().as_slice(), [Event::Connected(t, _)] if *t == token));
        assert_eq!(recv(&c), [b'U', b'T', b'P', 1, ACCEPT, 1, 2, 3, 4, 5, 6, 7, 8]);

        // Two reliable packets out of order; a batch split across them.
        let stream = [7i32.to_le_bytes().to_vec(), b"batch-1".to_vec()].concat();
        let packet = |seq: u16, bytes: &[u8]| {
            let mut d = vec![DATA];
            d.extend_from_slice(&token);
            d.push(RELIABLE);
            d.extend_from_slice(&[PAYLOAD, 0, 0, 0]);
            d.extend_from_slice(&seq.to_le_bytes());
            d.extend_from_slice(&u16::MAX.to_le_bytes());
            d.extend_from_slice(&u64::MAX.to_le_bytes());
            d.extend_from_slice(bytes);
            d
        };
        c.send(&packet(1, &stream[5..])).unwrap();
        c.send(&packet(0, &stream[..5])).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let events = server.poll();
        assert!(matches!(events.as_slice(), [Event::Data(_, b)] if b == b"batch-1"));

        // The ack names sequence 1 with both packets received.
        server.flush();
        let ack = recv(&c);
        assert_eq!(&ack[..10], &[[DATA].as_slice(), &token, &[RELIABLE]].concat()[..]);
        assert_eq!(ack[10], ACK);
        assert_eq!(u16::from_le_bytes([ack[16], ack[17]]), 1);
        assert_eq!(u64::from_le_bytes(ack[18..26].try_into().unwrap()) & 3, 3);

        // Our own data: sequence 0, resent until acknowledged.
        server.send(&token, b"hello");
        server.flush();
        let data = recv(&c);
        assert_eq!(&data[10..12], &[PAYLOAD, 0]);
        assert_eq!(u16::from_le_bytes([data[14], data[15]]), 0);
        assert_eq!(&data[26..], [5i32.to_le_bytes().as_slice(), b"hello"].concat());
        let mut ack = vec![DATA];
        ack.extend_from_slice(&token);
        ack.extend_from_slice(&[RELIABLE, ACK, 0, 0, 0, 0, 0, 0, 0]);
        ack.extend_from_slice(&u64::MAX.to_le_bytes());
        c.send(&ack).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        server.poll();
        assert!(server.connections[&token].in_flight.is_empty());
    }

    #[test]
    fn unreliable_fragments_reassemble() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let from: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let token = [9; 8];
        let mut events = Vec::new();
        server.datagram(&[[b'U', b'T', b'P', 1, REQUEST].as_slice(), &token].concat(), from, Instant::now(), &mut events);
        // Pipeline 2: unreliable sequence number, then fragments 5 (first) and 6 (last).
        let whole = [3i32.to_le_bytes().to_vec(), b"abc".to_vec()].concat();
        for (header, part) in [(FIRST | 5, &whole[..4]), (LAST | 6, &whole[4..])] {
            let d = [[DATA].as_slice(), &token, &[2, 0, 0], &header.to_le_bytes(), part].concat();
            server.datagram(&d, from, Instant::now(), &mut events);
        }
        assert!(matches!(events.as_slice(), [Event::Connected(..), Event::Data(_, b)] if b == b"abc"));
    }

    fn handshake(server: &mut Utp, from: SocketAddr, token: Token) -> Vec<Event> {
        let mut events = Vec::new();
        server.datagram(&[[b'U', b'T', b'P', 1, REQUEST].as_slice(), &token].concat(), from, Instant::now(), &mut events);
        events
    }

    fn reliable(token: &Token, sequence: u16, bytes: &[u8]) -> Vec<u8> {
        [[DATA].as_slice(), token, &[RELIABLE, PAYLOAD, 0, 0, 0], &sequence.to_le_bytes(), &u16::MAX.to_le_bytes(), &u64::MAX.to_le_bytes(), bytes].concat()
    }

    /// A closed session first delivers what was queued (the kick's reason), then disconnects;
    /// one that never acks is disconnected after LINGER.
    #[test]
    fn a_closed_session_delivers_its_last_words_first() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.socket.local_addr().unwrap();
        let c = client(0);
        c.connect(addr).unwrap();
        let token = [14; 8];
        c.send(&[[b'U', b'T', b'P', 1, REQUEST].as_slice(), &token].concat()).unwrap();
        c.send(&[[DATA].as_slice(), &token].concat()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        server.poll();
        recv(&c);
        server.send(&token, b"you are kicked");
        server.close(&token);
        server.flush();
        let reason = recv(&c);
        assert_eq!((reason[0], &reason[26..30]), (DATA, 14i32.to_le_bytes().as_slice()), "the reason first");
        server.flush();
        assert!(server.connections.contains_key(&token), "waits for the ack");
        let ack = [[DATA].as_slice(), &token, &[RELIABLE, ACK, 0, 0, 0, 0, 0, 0, 0], &u64::MAX.to_le_bytes()].concat();
        c.send(&ack).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(server.poll().is_empty());
        server.flush();
        assert_eq!(recv(&c), [[DISCONNECT].as_slice(), &token].concat());
        assert!(server.connections.is_empty());
        // Never acknowledged: gone after LINGER all the same.
        let mute = [15; 8];
        handshake(&mut server, "127.0.0.1:9".parse().unwrap(), mute);
        server.datagram(&[[DATA].as_slice(), &mute].concat(), "127.0.0.1:9".parse().unwrap(), Instant::now(), &mut Vec::new());
        server.send(&mute, b"bye");
        server.close(&mute);
        server.flush();
        server.connections.get_mut(&mute).unwrap().closing = Some(Instant::now());
        server.flush();
        assert!(server.connections.is_empty());
    }

    /// Lengths of 0 or below are skipped, not passed on as empty batches.
    #[test]
    fn empty_and_negative_lengths_make_no_batches() {
        let mut stream = [0i32.to_le_bytes(), (-5i32).to_le_bytes(), 0i32.to_le_bytes()].concat();
        stream.extend_from_slice(&2i32.to_le_bytes());
        stream.extend_from_slice(b"ok");
        let mut out = Vec::new();
        assert!(pop_batches(&mut stream, &mut out));
        assert_eq!((out, stream.len()), (vec![b"ok".to_vec()], 0));
    }

    /// One whole batch on unreliable pipeline 1.
    fn unreliable(token: &Token, batch: &[u8]) -> Vec<u8> {
        [[DATA].as_slice(), token, &[1], &(FIRST | LAST).to_le_bytes(), &(batch.len() as i32).to_le_bytes(), batch].concat()
    }

    #[test]
    fn hostile_reliable_streams() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let (from, token) = ("127.0.0.1:9".parse().unwrap(), [3; 8]);
        handshake(&mut server, from, token);
        let mut events = Vec::new();
        // Early packets wait only within the window of the next one due; the rest are dropped.
        for sequence in [1, 63, 64, 1000, 40000] {
            server.datagram(&reliable(&token, sequence, b"x"), from, Instant::now(), &mut events);
        }
        let c = &server.connections[&token];
        let mut early: Vec<u16> = c.early.keys().copied().collect();
        early.sort();
        assert_eq!((early, c.received), (vec![1, 63], 63));
        // A batch claiming 2 GiB drops the client.
        server.datagram(&reliable(&token, 0, &i32::MAX.to_le_bytes()), from, Instant::now(), &mut events);
        assert!(matches!(events.as_slice(), [Event::Disconnected(t)] if *t == token));
        assert!(server.connections.is_empty());
    }

    #[test]
    fn unreliable_reassembly_is_capped() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let (from, token) = ("127.0.0.1:9".parse().unwrap(), [4; 8]);
        handshake(&mut server, from, token);
        let mut events = Vec::new();
        let fragment = |header: u16| [[DATA].as_slice(), &token, &[1], &header.to_le_bytes(), &[0; 1400]].concat();
        server.datagram(&fragment(FIRST), from, Instant::now(), &mut events);
        for sequence in 1..800 {
            server.datagram(&fragment(sequence), from, Instant::now(), &mut events);
            assert!(server.connections[&token].fragments[0].buffer.len() <= MAX_BATCH);
        }
        server.datagram(&fragment(LAST | 800), from, Instant::now(), &mut events);
        assert!(events.is_empty() && server.connections[&token].fragments[0].buffer.is_empty());
    }

    #[test]
    fn a_client_that_never_acks_is_dropped() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let token = [5; 8];
        handshake(&mut server, "127.0.0.1:9".parse().unwrap(), token);
        let batch = vec![0; 1 << 20];
        for _ in 0..(MAX_QUEUED >> 20) - 1 {
            server.send(&token, &batch);
        }
        assert!(server.connections.contains_key(&token));
        server.send(&token, &batch);
        assert!(!server.connections.contains_key(&token));
        assert!(matches!(server.poll().as_slice(), [Event::Disconnected(t)] if *t == token));
    }

    #[test]
    fn connection_limits() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        // One address opens MAX_HANDSHAKES sessions per window, MAX_PER_IP at once.
        let remote = |i: u8| SocketAddr::from(([10, 0, 0, 2], 1000 + i as u16));
        for i in 0..=MAX_PER_IP as u8 {
            handshake(&mut server, remote(i), [i, 1, 0, 0, 0, 0, 0, 0]);
            if server.connections.len() as u32 == MAX_HANDSHAKES {
                assert!(handshake(&mut server, remote(100 + i), [i, 9, 0, 0, 0, 0, 0, 0]).is_empty(), "over the handshake rate");
                server.handshakes.values_mut().for_each(|(since, _)| *since -= HANDSHAKE_WINDOW + Duration::from_secs(1));
            }
        }
        assert_eq!(server.connections.len(), MAX_PER_IP);
        // Loopback is exempt from both (behind a relay every player is 127.0.0.1); the total holds.
        for i in 0..20u8 {
            handshake(&mut server, SocketAddr::from(([127, 0, 0, 1], 2000 + i as u16)), [i, 3, 0, 0, 0, 0, 0, 0]);
        }
        assert_eq!(server.connections.len(), MAX_PER_IP + 20);
        for i in 0..200u8 {
            handshake(&mut server, SocketAddr::from(([127, 1, 0, i], 9)), [i, 2, 0, 0, 0, 0, 0, 0]);
        }
        assert_eq!(server.connections.len(), MAX_CONNECTIONS);
        assert!(is_loopback("::ffff:127.0.0.1".parse().unwrap()) && !is_loopback("10.0.0.2".parse().unwrap()));
    }

    /// The reflection attack: a forged source sends a handshake, data and heartbeats, never
    /// acks. It gets the first window of 4 packets, resent ever less often, then the session
    /// goes. A client that acks gets a window that grows to the full 64.
    #[test]
    fn a_session_that_never_acks_gets_little_and_is_dropped() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let (from, token) = ("127.0.0.1:9".parse().unwrap(), [11; 8]);
        handshake(&mut server, from, token);
        server.datagram(&[[DATA].as_slice(), &token].concat(), from, Instant::now(), &mut Vec::new());
        server.send(&token, &vec![0; 1 << 20]);
        let mut sent = 0;
        for round in 0..6 {
            server.flush();
            let c = server.connections.get_mut(&token).unwrap();
            assert_eq!(c.in_flight.len(), INITIAL_WINDOW, "round {round}");
            // Each resend round waits twice as long as the one before, up to the cap.
            let wait = RESEND * (1 << c.backoff.min(MAX_BACKOFF));
            assert_eq!(wait, RESEND * (1 << (round as u32).min(MAX_BACKOFF)));
            c.in_flight.iter_mut().for_each(|p| p.2 -= wait);
            sent += c.in_flight.len();
        }
        assert_eq!(sent, 6 * INITIAL_WINDOW);
        let c = server.connections.get_mut(&token).unwrap();
        c.progress -= STALL + Duration::from_secs(1);
        assert!(matches!(server.poll().as_slice(), [Event::Disconnected(t)] if *t == token));
        assert!(server.connections.is_empty());

        // An ack of the 4 in flight lets 8 go; acks keep it growing to the full window.
        let token = [12; 8];
        handshake(&mut server, from, token);
        server.datagram(&[[DATA].as_slice(), &token].concat(), from, Instant::now(), &mut Vec::new());
        server.send(&token, &vec![0; 1 << 20]);
        let mut window = INITIAL_WINDOW;
        while window < WINDOW {
            server.flush();
            let c = &server.connections[&token];
            assert_eq!(c.in_flight.len(), window);
            let last = c.sequence.wrapping_sub(1);
            let ack = [[DATA].as_slice(), &token, &[RELIABLE, ACK, 0, 0, 0, 0, 0], &last.to_le_bytes(), &u64::MAX.to_le_bytes()].concat();
            server.datagram(&ack, from, Instant::now(), &mut Vec::new());
            window = (window * 2).min(WINDOW);
            assert_eq!(server.connections[&token].window, window);
        }
    }

    /// A late-game world is several MiB: taking one packet's bytes off the queue must not move
    /// everything behind them (once per packet, that is gigabytes of copying per join).
    #[test]
    fn a_big_world_is_sent_in_linear_time() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let (from, token) = ("127.0.0.1:9".parse().unwrap(), [21; 8]);
        handshake(&mut server, from, token);
        server.datagram(&[[DATA].as_slice(), &token].concat(), from, Instant::now(), &mut Vec::new());
        server.send(&token, &vec![0; 8 << 20]);
        let start = Instant::now();
        let mut sent = 0;
        while !server.connections[&token].queued.is_empty() {
            server.flush();
            let c = &server.connections[&token];
            sent += c.in_flight.len();
            let ack = [[DATA].as_slice(), &token, &[RELIABLE, ACK, 0, 0, 0, 0, 0], &c.sequence.wrapping_sub(1).to_le_bytes(), &u64::MAX.to_le_bytes()].concat();
            server.datagram(&ack, from, Instant::now(), &mut Vec::new());
        }
        assert!(sent > 6000);
        assert!(start.elapsed() < Duration::from_secs(1), "{sent} packets took {:?}", start.elapsed());
    }

    /// Sequence numbers are 16 bits: a session that lasts past 65536 packets each way (80 MB
    /// at the full size) goes on delivering and acknowledging across the wrap.
    #[test]
    fn sequences_wrap_around() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let (from, token) = ("127.0.0.1:9".parse().unwrap(), [22; 8]);
        handshake(&mut server, from, token);
        let mut events = Vec::new();
        let one = [1i32.to_le_bytes().as_slice(), b"x"].concat();
        for i in 0..70_000u32 {
            let packet = [[DATA].as_slice(), &token, &[RELIABLE, PAYLOAD, 0, 0, 0], &(i as u16).to_le_bytes(), &u16::MAX.to_le_bytes(), &u64::MAX.to_le_bytes(), &one].concat();
            server.datagram(&packet, from, Instant::now(), &mut events);
        }
        assert_eq!(events.len(), 70_000);
        assert!(events.iter().all(|e| matches!(e, Event::Data(_, b) if b == b"x")));
        server.datagram(&[[DATA].as_slice(), &token].concat(), from, Instant::now(), &mut Vec::new());
        let mut acked = 0;
        for _ in 0..11 {
            server.send(&token, &vec![0; 8 << 20]);
            while !server.connections[&token].queued.is_empty() || !server.connections[&token].in_flight.is_empty() {
                server.flush();
                let c = &server.connections[&token];
                acked += c.in_flight.len();
                let ack = [[DATA].as_slice(), &token, &[RELIABLE, ACK, 0, 0, 0, 0, 0], &c.sequence.wrapping_sub(1).to_le_bytes(), &u64::MAX.to_le_bytes()].concat();
                server.datagram(&ack, from, Instant::now(), &mut Vec::new());
            }
        }
        assert!(acked > 65_536 && server.connections.contains_key(&token), "{acked} packets acknowledged");
    }

    /// No client acks what was never sent: a blind spoofer guessing ahead loses its session.
    #[test]
    fn an_ack_of_a_packet_never_sent_drops_the_session() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let (from, token) = ("127.0.0.1:9".parse().unwrap(), [13; 8]);
        handshake(&mut server, from, token);
        let ack = |acked: u16| [[DATA].as_slice(), &token, &[RELIABLE, ACK, 0, 0, 0, 0, 0], &acked.to_le_bytes(), &u64::MAX.to_le_bytes()].concat();
        let mut events = Vec::new();
        // Before anything is sent the client names sequence -1, as Unity Transport starts.
        server.datagram(&ack(u16::MAX), from, Instant::now(), &mut events);
        server.send(&token, b"world");
        server.flush();
        server.datagram(&ack(0), from, Instant::now(), &mut events);
        assert!(events.is_empty() && server.connections[&token].in_flight.is_empty());
        server.datagram(&ack(5), from, Instant::now(), &mut events);
        assert!(matches!(events.as_slice(), [Event::Disconnected(t)] if *t == token));
        assert!(server.connections.is_empty());
    }

    /// A handshake alone gets nothing but its accept (spoofed ones reflect nothing more), and
    /// expires without data.
    #[test]
    fn nothing_but_the_accept_before_data() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.socket.local_addr().unwrap();
        let c = client(0);
        c.connect(addr).unwrap();
        let token = [6; 8];
        c.send(&[[b'U', b'T', b'P', 1, REQUEST].as_slice(), &token].concat()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        server.poll();
        assert_eq!(recv(&c)[4], ACCEPT);
        server.connections.get_mut(&token).unwrap().last_send -= Duration::from_secs(1);
        server.flush();
        c.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        assert!(c.recv(&mut [0; 64]).is_err(), "no heartbeat before data");
        c.send(&[[DATA].as_slice(), &token].concat()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        server.poll();
        server.connections.get_mut(&token).unwrap().last_send -= Duration::from_secs(1);
        server.flush();
        assert_eq!(recv(&c), [[HEARTBEAT].as_slice(), &token].concat());

        let mute = [7; 8];
        handshake(&mut server, "127.0.0.1:9".parse().unwrap(), mute);
        server.connections.get_mut(&mute).unwrap().opened -= DATA_WITHIN + Duration::from_secs(1);
        assert!(matches!(server.poll().as_slice(), [Event::Disconnected(t)] if *t == mute));
    }

    /// The handshake fixes the address: nothing from elsewhere counts, not even a disconnect.
    #[test]
    fn datagrams_from_another_address_are_dropped() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let (home, other): (SocketAddr, SocketAddr) = ("127.0.0.1:9".parse().unwrap(), "127.0.0.3:9".parse().unwrap());
        let token = [8; 8];
        handshake(&mut server, home, token);
        let mut events = handshake(&mut server, other, token);
        server.datagram(&unreliable(&token, b"z"), other, Instant::now(), &mut events);
        server.datagram(&[[DISCONNECT].as_slice(), &token].concat(), other, Instant::now(), &mut events);
        assert!(events.is_empty());
        assert_eq!(server.connections[&token].addr, home);
        server.datagram(&unreliable(&token, b"z"), home, Instant::now(), &mut events);
        assert!(matches!(events.as_slice(), [Event::Data(_, b)] if b == b"z"));
    }

    /// Garbage datagrams, before and after a handshake, never crash the transport.
    #[test]
    fn garbage_datagrams_do_not_crash_the_transport() {
        let mut server = Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.socket.local_addr().unwrap();
        let c = client(0);
        c.connect(addr).unwrap();
        let token = [9, 8, 7, 6, 5, 4, 3, 2];
        c.send(&[b'U', b'T', b'P', 1, REQUEST, 9, 8, 7, 6, 5, 4, 3, 2]).unwrap();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..20_000u32 {
            let mut d: Vec<u8> = match next() % 4 {
                0 => vec![b'U', b'T', b'P', 1, (next() % 8) as u8],
                1 => {
                    let mut d = vec![DATA];
                    d.extend_from_slice(&token);
                    d.push((next() % 5) as u8);
                    d
                }
                2 => vec![DATA],
                _ => Vec::new(),
            };
            d.extend((0..next() % 1500).map(|_| next() as u8));
            let _ = c.send(&d);
            if round % 64 == 0 {
                server.poll();
                server.flush();
            }
        }
        server.poll();
        server.flush();
    }
}
