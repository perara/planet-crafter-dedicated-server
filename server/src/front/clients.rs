//! The front's loop for the clients: what the transport (transport.rs) found is passed to the
//! live bank as frames (CONNECTED, DATA, GONE), and a bank's batches, kicks and closes are done
//! to the connections. The transport knows no bank and the queue (queue.rs) no socket; this is
//! where they meet.

use std::sync::atomic::Ordering;
use std::time::Instant;

use serde_json::json;

use super::transport::{steam, Sent};
use super::{Front, KICK_DELAY};
use crate::dashboard::{BYTES_IN, BYTES_OUT};
use crate::ipc;
use crate::utp;

impl Front {
    /// Steam's connections: new ones are told to the bank, batches and ends are relayed.
    pub(super) fn poll_steam(&mut self) {
        let Some(steam::Poll { joined, mut gone }) = self.net.steam_poll() else { return };
        for j in joined {
            let (client, steam_id) = (j.client, j.steam_id);
            self.net.add_steam(j);
            self.to_bank(ipc::CONNECTED, client, json!({"steamId": steam_id, "trusted": false}).to_string().as_bytes(), 0);
        }
        let incoming = self.net.steam_receive(&mut gone);
        for client in gone {
            self.left(client);
        }
        for (client, batch) in incoming {
            self.from_client(client, batch);
        }
    }

    /// Whether anything went out to a client.
    pub(super) fn poll_utp(&mut self) -> bool {
        let events = self.net.poll_utp();
        // A handshake or a disconnect answers at the next flush.
        let any = !events.is_empty();
        for event in events {
            match event {
                utp::Event::Connected(token, addr) => {
                    let (client, body) = self.net.accept_ip(token, addr);
                    self.to_bank(ipc::CONNECTED, client, body.as_bytes(), 0);
                }
                utp::Event::Data(token, batch) => {
                    if let Some(c) = self.net.ip_client(&token) {
                        self.from_client(c, batch);
                    }
                }
                utp::Event::Disconnected(token) => {
                    if let Some(c) = self.net.forget_ip(&token) {
                        self.left(c);
                    }
                }
            }
        }
        any
    }

    fn from_client(&mut self, client: u64, batch: Vec<u8>) {
        // Gone or kicked: nothing more from it reaches the game (the reason still goes out).
        if !self.net.accepts_from(client) {
            return;
        }
        BYTES_IN.fetch_add(batch.len() as u64, Ordering::Relaxed);
        self.to_bank(ipc::DATA, client, &batch, client);
    }

    /// The connection ended from the client's side (or was dropped).
    fn left(&mut self, client: u64) {
        if self.net.left(client) {
            self.to_bank(ipc::GONE, client, &[], 0);
        }
    }

    /// Closes a connection from this side.
    pub(super) fn close(&mut self, client: u64) {
        if self.net.close(client) {
            self.to_bank(ipc::GONE, client, &[], 0);
        }
    }

    /// A batch to a client. Whether a direct-IP session has something to flush.
    pub(super) fn to_client(&mut self, client: u64, batch: &[u8]) -> bool {
        BYTES_OUT.fetch_add(batch.len() as u64, Ordering::Relaxed);
        match self.net.send(client, batch) {
            Sent::Queued => true,
            Sent::Steam | Sent::Nobody => false,
            Sent::Closed => {
                self.to_bank(ipc::GONE, client, &[], 0);
                false
            }
        }
    }

    /// Every client told why (a batch with the reason), and closed a moment later. How many.
    pub(super) fn dismiss_all(&mut self, reason: &[u8], now: Instant) -> usize {
        let clients = self.net.clients();
        for c in &clients {
            self.to_client(*c, reason);
            self.net.drop_later(*c, now + KICK_DELAY);
        }
        clients.len()
    }

    /// Kicked clients whose reason has had its moment: closed. Whether any was.
    pub(super) fn drops(&mut self) -> bool {
        let due = self.net.take_due(Instant::now());
        for c in &due {
            self.close(*c);
        }
        !due.is_empty() || !self.net.pending_drop.is_empty()
    }
}
