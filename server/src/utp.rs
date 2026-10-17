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
use std::net::{SocketAddr, UdpSocket};
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
const HEARTBEAT_AFTER: Duration = Duration::from_millis(500);
const TIMEOUT: Duration = Duration::from_secs(30);
const FIRST: u16 = 0x8000;
const LAST: u16 = 0x4000;

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
    addr: SocketAddr,
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
    queued: Vec<u8>,
    fragments: [Fragments; 2],
}

impl Connection {
    fn new(addr: SocketAddr, now: Instant) -> Connection {
        Connection {
            addr,
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
            queued: Vec::new(),
            fragments: Default::default(),
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
        } else if (sequence.wrapping_sub(self.next) as i16) > 0 {
            self.early.insert(sequence, payload.to_vec());
        }
    }

    fn acknowledge(&mut self, acked: u16, mask: u64) {
        self.in_flight.retain(|(s, _, _)| {
            let back = acked.wrapping_sub(*s) as i16;
            !((0..64).contains(&back) && mask & (1 << back) != 0)
        });
    }
}

/// Splits off every complete length-prefixed batch at the front of `buffer`.
fn pop_batches(buffer: &mut Vec<u8>, out: &mut Vec<Vec<u8>>) {
    let mut at = 0;
    while buffer.len() - at >= 4 {
        let n = i32::from_le_bytes(buffer[at..at + 4].try_into().unwrap()).max(0) as usize;
        if buffer.len() - at - 4 < n {
            break;
        }
        out.push(buffer[at + 4..at + 4 + n].to_vec());
        at += 4 + n;
    }
    buffer.drain(..at);
}

pub struct Utp {
    socket: UdpSocket,
    connections: HashMap<Token, Connection>,
}

impl Utp {
    pub fn bind(addr: SocketAddr) -> std::io::Result<Utp> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        Ok(Utp { socket, connections: HashMap::new() })
    }

    fn transmit(&mut self, token: &Token, kind: u8, body: &[u8]) {
        let Some(c) = self.connections.get_mut(token) else { return };
        let mut datagram = Vec::with_capacity(9 + body.len());
        datagram.push(kind);
        datagram.extend_from_slice(token);
        datagram.extend_from_slice(body);
        let _ = self.socket.send_to(&datagram, c.addr);
        c.last_send = Instant::now();
    }

    /// Client side: connects to a server and returns once it accepted, with our token.
    pub fn connect(server: SocketAddr, timeout: Duration) -> std::io::Result<(Utp, Token)> {
        let mut utp = Utp::bind("0.0.0.0:0".parse().unwrap())?;
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        let token: Token = (seed ^ (std::process::id() as u64).rotate_left(32)).to_le_bytes();
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
                        utp.connections.insert(token, Connection::new(server, Instant::now()));
                        return Ok((utp, token));
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer from the server"))
    }

    /// Reads every waiting datagram; also reports connections that went silent.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let mut buffer = [0u8; 2048];
        let now = Instant::now();
        while let Ok((n, from)) = self.socket.recv_from(&mut buffer) {
            self.datagram(&buffer[..n], from, now, &mut events);
        }
        let silent: Vec<Token> = self.connections.iter().filter(|(_, c)| now - c.last_receive > TIMEOUT).map(|(t, _)| *t).collect();
        for token in silent {
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
            if !self.connections.contains_key(&token) {
                self.connections.insert(token, Connection::new(from, now));
                events.push(Event::Connected(token, from));
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
        let Some(c) = self.connections.get_mut(&token) else { return };
        c.last_receive = now;
        c.addr = from;
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
                        c.acknowledge(acked, u64::from_le_bytes(body[8..16].try_into().unwrap()));
                        if body[0] == PAYLOAD {
                            c.receive_reliable(sequence, &body[16..]);
                            c.ack_due = true;
                            pop_batches(&mut c.stream, &mut batches);
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
                            if !f.broken {
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

    /// Queues one Netcode batch on the reliable stream.
    pub fn send(&mut self, token: &Token, batch: &[u8]) {
        if let Some(c) = self.connections.get_mut(token) {
            c.queued.extend_from_slice(&(batch.len() as i32).to_le_bytes());
            c.queued.extend_from_slice(batch);
        }
    }

    /// Sends what fits in the window, resends what was not acknowledged, acks, heartbeats.
    pub fn flush(&mut self) {
        let now = Instant::now();
        let tokens: Vec<Token> = self.connections.keys().copied().collect();
        for token in tokens {
            let mut datagrams: Vec<(u8, Vec<u8>)> = Vec::new();
            {
                let c = self.connections.get_mut(&token).unwrap();
                for i in 0..c.in_flight.len() {
                    if now - c.in_flight[i].2 >= RESEND {
                        let (sequence, payload) = (c.in_flight[i].0, c.in_flight[i].1.clone());
                        c.in_flight[i].2 = now;
                        datagrams.push((DATA, [vec![RELIABLE], c.reliable_header(PAYLOAD, sequence), payload].concat()));
                    }
                }
                while c.in_flight.len() < WINDOW && !c.queued.is_empty() {
                    let payload: Vec<u8> = c.queued.drain(..CHUNK.min(c.queued.len())).collect();
                    let sequence = c.sequence;
                    c.sequence = c.sequence.wrapping_add(1);
                    datagrams.push((DATA, [vec![RELIABLE], c.reliable_header(PAYLOAD, sequence), payload.clone()].concat()));
                    c.in_flight.push_back((sequence, payload, now));
                }
                if c.ack_due && datagrams.is_empty() {
                    datagrams.push((DATA, [vec![RELIABLE], c.reliable_header(ACK, 0), vec![0]].concat()));
                }
                c.ack_due = false;
                if datagrams.is_empty() && now - c.last_send > HEARTBEAT_AFTER {
                    datagrams.push((HEARTBEAT, Vec::new()));
                }
            }
            for (kind, body) in datagrams {
                self.transmit(&token, kind, &body);
            }
        }
    }

    /// Tells the client it is disconnected and forgets it.
    pub fn disconnect(&mut self, token: &Token) {
        self.transmit(token, DISCONNECT, &[]);
        self.connections.remove(token);
    }

    pub fn tokens(&self) -> Vec<Token> {
        self.connections.keys().copied().collect()
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
}
