//! The frames on their way to a bank, and the budgets that keep a stalled bank from taking the
//! front's memory: a frame goes straight to the live bank's link, or is held while the world
//! stands (a flip, a restart); a client with too much waiting, or the one with the most when all
//! together have too much, is dropped, not the world.

use std::collections::HashMap;

use super::{Front, Phase, KICK_DELAY, STOPPED};
use crate::ipc::{self, Link};

/// A client's frames waiting for the bank (a stalled or flipping world) past this: the client
/// is dropped, not the world.
const MAX_CLIENT_QUEUE: usize = 16 << 20;
/// All clients' frames waiting for the bank together (held while the world stands for a flip,
/// or queued to a bank that does not read): past this the client with the most waiting, the one
/// the stall hurts most, is dropped, until the new frame fits. 128 clients at MAX_CLIENT_QUEUE
/// each would hold two gigabytes.
const MAX_TOTAL_QUEUE: usize = 256 << 20;
/// A stall that had this much waiting, once over, gives the memory back to the system (the
/// allocator keeps freed memory of that size in the process).
const TRIM_AFTER: usize = 8 << 20;

/// A frame held for the bank: kind, n, body, and the client it counts against (0: none).
pub(super) type Held = (u8, u64, Vec<u8>, u64);

/// The frames held while the world stands, and what is waiting in all.
pub(super) struct Queue {
    pub held: Vec<Held>,
    /// The held bytes of each client.
    pub held_bytes: HashMap<u64, usize>,
    /// The sum of held_bytes.
    pub held_total: usize,
    /// The most that waited since the last time the memory was given back.
    pub waiting_peak: usize,
    /// MAX_TOTAL_QUEUE (a field for the tests).
    pub cap: usize,
}

impl Queue {
    pub fn new() -> Queue {
        Queue { held: Vec::new(), held_bytes: HashMap::new(), held_total: 0, waiting_peak: 0, cap: MAX_TOTAL_QUEUE }
    }

    /// A frame is held. A client's frame counts against its bound (its 13 bytes of header too).
    pub fn hold(&mut self, kind: u8, n: u64, body: &[u8], client: u64) {
        if client != 0 {
            *self.held_bytes.entry(client).or_default() += body.len() + 13;
            self.held_total += body.len() + 13;
        }
        self.held.push((kind, n, body.to_vec(), client));
    }

    /// Everything held, in order; nothing is counted as waiting after it.
    pub fn release(&mut self) -> Vec<Held> {
        let held = std::mem::take(&mut self.held);
        self.held_bytes.clear();
        self.held_total = 0;
        held
    }

    /// Everything held is dropped.
    pub fn clear(&mut self) {
        self.release();
    }

    /// A client's held frames are taken back (they are the memory).
    pub fn forget(&mut self, client: u64) {
        if let Some(bytes) = self.held_bytes.remove(&client) {
            self.held_total = self.held_total.saturating_sub(bytes);
            self.held.retain(|h| h.3 != client);
        }
    }

    /// What all clients have waiting for the bank: held, and queued on the live link.
    pub fn waiting_total(&self, live: Option<&Link>) -> usize {
        self.held_total + live.map_or(0, |l| l.queued_total)
    }

    /// The client with the most bytes waiting (held and queued), and how many.
    pub fn slowest(&self, live: Option<&Link>) -> Option<(u64, usize)> {
        let mut per: HashMap<u64, usize> = self.held_bytes.clone();
        for (client, bytes) in live.iter().flat_map(|l| l.queued.iter()) {
            *per.entry(*client).or_default() += bytes;
        }
        per.into_iter().max_by_key(|(client, bytes)| (*bytes, *client)).filter(|(_, bytes)| *bytes > 0)
    }

    /// A stall that held a lot of memory is over once little waits: the most that waited, once
    /// (and it starts again from nothing).
    pub fn settled(&mut self, waiting: usize) -> Option<usize> {
        self.waiting_peak = self.waiting_peak.max(waiting);
        if self.waiting_peak >= TRIM_AFTER && waiting < TRIM_AFTER / 64 {
            return Some(std::mem::take(&mut self.waiting_peak));
        }
        None
    }
}

impl Front {
    /// A frame for the live bank, or held while the world stands. `client`: a client's own
    /// frame, counted against its bound.
    pub(super) fn to_bank(&mut self, kind: u8, n: u64, body: &[u8], client: u64) {
        // No world: a joining player is told so and let go; nothing else has a game to go to.
        if self.phase == Phase::Stopped {
            if kind == ipc::CONNECTED {
                self.to_client(n, &crate::bank::reason_batch(STOPPED));
                self.net.drop_later(n, std::time::Instant::now() + KICK_DELAY);
            }
            return;
        }
        let holding = matches!(self.phase, Phase::Snapshotting | Phase::Restoring | Phase::Recovering);
        let queued = if client == 0 {
            0
        } else {
            self.queue.held_bytes.get(&client).copied().unwrap_or(0) + self.live.as_ref().and_then(|b| b.link.queued.get(&client).copied()).unwrap_or(0)
        };
        if client != 0 && queued + body.len() > MAX_CLIENT_QUEUE {
            eprintln!("client {client}: over {} MiB waiting for the game; dropped", MAX_CLIENT_QUEUE >> 20);
            self.drop_waiting(client);
            return;
        }
        if client != 0 && !self.make_room(client, body.len() + 13) {
            return;
        }
        match (&mut self.live, holding) {
            (Some(b), false) => b.link.queue(kind, n, body, None, client),
            _ => self.queue.hold(kind, n, body, client),
        }
    }

    pub(super) fn waiting_total(&self) -> usize {
        self.queue.waiting_total(self.live.as_ref().map(|b| &b.link))
    }

    /// Room for `need` more bytes among all clients' waiting frames: the client with the most
    /// waiting is dropped, again and again, until it fits. False when that was `client` itself.
    fn make_room(&mut self, client: u64, need: usize) -> bool {
        while self.waiting_total() + need > self.queue.cap {
            let Some((slowest, bytes)) = self.queue.slowest(self.live.as_ref().map(|b| &b.link)) else { break };
            eprintln!(
                "client {slowest}: {} MiB waiting for the game, the most of the {} MiB all clients have waiting (at most {} MiB): dropped",
                bytes >> 20,
                self.waiting_total() >> 20,
                self.queue.cap >> 20
            );
            self.drop_waiting(slowest);
            if slowest == client {
                return false;
            }
        }
        true
    }

    /// A client dropped for what it has waiting: its frames not yet sent to the bank are
    /// taken back (they are the memory), then it is closed.
    fn drop_waiting(&mut self, client: u64) {
        self.queue.forget(client);
        if let Some(b) = self.live.as_mut() {
            b.link.purge(client);
        }
        self.net.mark_kicked(client);
        self.close(client);
    }

    /// After a stall that held a lot of memory, once it is over: the freed memory goes back to
    /// the system (glibc keeps what a burst of large frames freed).
    pub(super) fn release_memory(&mut self) {
        let waiting = self.waiting_total();
        if let Some(peak) = self.queue.settled(waiting) {
            println!("memory: {} MiB waited for the game at most; given back to the system", peak >> 20);
            // SAFETY: malloc_trim only returns free memory to the system.
            unsafe { libc::malloc_trim(0) };
        }
    }

    /// The held frames to the live bank.
    pub(super) fn release_held(&mut self) -> usize {
        let held = self.queue.release();
        let n = held.len();
        if let Some(b) = self.live.as_mut() {
            for (kind, num, body, client) in held {
                b.link.queue(kind, num, &body, None, client);
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fake_bank, test_front};
    use super::super::transport::Conn;
    use super::*;
    use std::time::Duration;

    /// All the clients' frames waiting for the bank have a total cap: past it the client with the most
    /// waiting is dropped (its frames taken back), whether it is the sender or another, and the
    /// accounting follows the frames in and out.
    #[test]
    fn what_all_clients_have_waiting_for_the_bank_is_capped_by_dropping_the_slowest() {
        let mut front = test_front();
        front.queue.cap = 4 << 20;
        let (live, _bank) = fake_bank("a");
        front.live = Some(live);
        // Three clients (their connections are only entries here).
        for client in 1..=3u64 {
            front.net.conns.insert(client, Conn::Ip([client as u8; 8]));
        }
        let mib = vec![7u8; (1 << 20) - 13];
        // The bank is not reading: frames pile up in the link's queue. 2 MiB, 1 MiB, 0.
        for (client, count) in [(1u64, 2), (2, 1)] {
            for _ in 0..count {
                front.to_bank(ipc::DATA, client, &mib, client);
            }
        }
        assert_eq!(front.waiting_total(), 3 << 20);
        assert_eq!(front.queue.slowest(front.live.as_ref().map(|b| &b.link)), Some((1, 2 << 20)));
        // 1 MiB more from client 3 does not fit beside the 3: client 1 has the most and goes.
        front.to_bank(ipc::DATA, 3, &mib, 3);
        front.to_bank(ipc::DATA, 3, &mib, 3);
        assert!(!front.net.conns.contains_key(&1) && front.net.conns.contains_key(&2) && front.net.conns.contains_key(&3), "client 1 was dropped");
        assert_eq!(front.live.as_ref().unwrap().link.queued.get(&1), None, "its frames were taken back");
        assert_eq!(front.waiting_total(), 3 << 20, "client 2's and client 3's: {:?}", front.live.as_ref().unwrap().link.queued);
        // Held while the world stands (a flip): the same, from the held frames.
        front.phase = Phase::Snapshotting;
        front.live.as_mut().unwrap().link.purge(2);
        front.live.as_mut().unwrap().link.purge(3);
        assert_eq!(front.waiting_total(), 0);
        for _ in 0..3 {
            front.to_bank(ipc::DATA, 2, &mib, 2);
        }
        front.to_bank(ipc::DATA, 3, &mib, 3);
        assert_eq!((front.queue.held_total, front.queue.held_bytes.len()), (4 << 20, 2));
        // Client 3 asks for more: 2 has the most, 2 goes (held frames removed, total follows).
        front.to_bank(ipc::DATA, 3, &mib, 3);
        assert!(!front.net.conns.contains_key(&2) && front.net.conns.contains_key(&3));
        assert_eq!((front.queue.held_total, front.queue.held.iter().filter(|h| h.3 == 2).count()), (2 << 20, 0));
        // A client alone that is over the cap itself is dropped, the frame not kept.
        for _ in 0..3 {
            front.to_bank(ipc::DATA, 3, &mib, 3);
        }
        assert!(!front.net.conns.contains_key(&3));
        assert_eq!(front.queue.held_total, 0);
        // Released: only the GONE frames of the two clients that were dropped in the hold are left, and nothing is counted as waiting.
        assert_eq!(front.release_held(), 2);
        assert_eq!(front.waiting_total(), 0);
        for mut child in [front.live.take()].into_iter().flatten().map(|b| b.child) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// A measurement, not a check (run by hand: cargo test --release front::queue::tests::measure_ -- --ignored
    /// --nocapture --test-threads=1): a stall that holds 192 MiB of client frames (12000 of 16 KiB
    /// from 64 clients), released into a bank that reads again, and the resident memory of the
    /// process before, at the peak, after the queue drained, and after release_memory().
    #[test]
    #[ignore]
    fn measure_memory_after_a_stall() {
        let rss = || std::fs::read_to_string("/proc/self/statm").unwrap().split_whitespace().nth(1).unwrap().parse::<usize>().unwrap() * 4096 >> 20;
        let mut front = test_front();
        front.queue.cap = usize::MAX;
        let (live, mut peer) = fake_bank("a");
        front.live = Some(live);
        front.phase = Phase::Snapshotting;
        let reader = std::thread::spawn(move || {
            let mut seen = 0usize;
            // The bank reads again once the front lets the frames go.
            while peer.fill(usize::MAX) {
                while peer.next().is_some() {
                    seen += 1;
                }
            }
            seen
        });
        let before = rss();
        let body = vec![5u8; 16 << 10];
        let mut small = Vec::new();
        for i in 0..12_000u64 {
            front.to_bank(ipc::DATA, 1 + i % 64, &body, 1 + i % 64);
            // What the front allocates meanwhile and keeps (connections, log lines, tables): the heap
            // cannot shrink from its end past it.
            if i % 8 == 0 {
                small.push(vec![1u8; 200]);
            }
        }
        let peak = rss();
        assert_eq!(front.queue.held_total >> 20, 12_000 * (16 << 10) >> 20);
        front.phase = Phase::Idle;
        front.release_held();
        while front.live.as_ref().unwrap().link.pending() {
            front.flush_banks();
            std::thread::sleep(Duration::from_millis(2));
        }
        let drained = rss();
        front.queue.waiting_peak = front.queue.waiting_peak.max(TRIM_AFTER);
        front.release_memory();
        let trimmed = rss();
        println!("resident memory of the front: {before} MiB before, {peak} MiB holding 192 MiB of frames, {drained} MiB after the queue drained, {trimmed} MiB after release_memory()");
        drop(small);
        let live = front.live.take().unwrap();
        drop(live.link);
        let mut child = live.child;
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(reader.join().unwrap(), 12_000);
    }
}
