//! The datagram path of Unity Transport: handshakes, the reliable stream and its acks, fragments.
use super::*;
use std::cell::RefCell;

thread_local! {
    static UTP: RefCell<Utp> = RefCell::new(Utp::bind("127.0.0.1:0".parse().unwrap()).unwrap());
}

const TOKEN: Token = [1, 2, 3, 4, 5, 6, 7, 8];

/// The input is a series of datagrams: a length byte, a flag byte, the bytes. A flag with bit 0
/// set gives the datagram the live session's token (else it is hostile as it came); bits 1 and
/// 2 pick the sender (loopback, one address, or many); bit 3 flushes, bit 4 polls afterwards.
pub fn run(data: &[u8]) {
    UTP.with(|u| {
        let mut u = u.borrow_mut();
        u.connections.clear();
        u.handshakes.clear();
        u.outbox.clear();
        u.dropped.clear();
        let mut events = Vec::new();
        let now = Instant::now();
        let loopback: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        // The session the datagrams talk to.
        let mut hello = SIGNATURE.to_vec();
        hello.push(REQUEST);
        hello.extend_from_slice(&TOKEN);
        u.datagram(&hello, loopback, now, &mut events);
        let mut at = 0;
        let mut n = 0u32;
        while at + 2 <= data.len() {
            let len = data[at] as usize * 6 % 1500;
            let flags = data[at + 1];
            at += 2;
            let end = (at + len).min(data.len());
            let mut d = data[at..end].to_vec();
            at = end;
            if flags & 1 != 0 && d.len() >= 9 {
                // A reliable data packet to the live session is the interesting one.
                d[1..9].copy_from_slice(&TOKEN);
            }
            let from: SocketAddr = match (flags >> 1) & 3 {
                0 | 1 => loopback,
                2 => "10.9.8.7:40001".parse().unwrap(),
                _ => SocketAddr::new(IpAddr::from([10, 0, (n >> 8) as u8, n as u8]), 40000),
            };
            n += 1;
            u.datagram(&d, from, now, &mut events);
            if flags & 8 != 0 {
                u.send(&TOKEN, &[1, 2, 3]);
                u.flush();
            }
            if flags & 16 != 0 {
                events.extend(u.poll());
            }
        }
        for e in &events {
            if let Event::Data(_, b) = e {
                assert!(b.len() <= MAX_BATCH);
            }
        }
        assert!(u.connections.len() <= MAX_CONNECTIONS);
        for c in u.connections.values() {
            assert!(c.early.len() < WINDOW + 1);
            assert!(c.stream.len() <= MAX_BATCH + 4 + 2048);
        }
    });
}
