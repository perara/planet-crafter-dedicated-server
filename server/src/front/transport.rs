//! The clients' connections: the direct-IP sessions (utp.rs) and the Steam connections (steam.rs),
//! one table of client ids over both, and what is done to a client from this side: send it a
//! batch, kick it a moment later, close it. It knows nothing of the banks; the front's loop asks
//! it what happened and passes the frames between it and the banks (mod.rs, bank_link.rs).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::os::fd::RawFd;
use std::time::Instant;

use steamworks::networking_types::{AppNetConnectionEnd, NetConnectionEnd};
use steamworks::networking_sockets::NetConnection;

use crate::msg::{self, Connected};
use crate::utp::{self, Utp};

pub(super) mod steam;


pub(super) enum Conn {
    /// Steam networking: the connection carries the verified SteamID.
    Steam { connection: NetConnection, split: Option<(Vec<u8>, usize)>, steam_id: u64 },
    Ip(utp::Token),
}

/// What sending a batch to a client came to.
#[derive(Debug, PartialEq)]
pub(super) enum Sent {
    /// Handed to a direct-IP session: the transport has something to flush.
    Queued,
    /// Handed to Steam.
    Steam,
    /// No such client.
    Nobody,
    /// Steam refused (the client has part of a batch): the client was closed, loudly.
    Closed,
}

pub(super) struct Transport {
    pub utp: Option<Utp>,
    pub(super) steam: Option<steam::Login>,
    /// The Steam invite code, when there is a Steam login.
    pub invite_code: Option<String>,
    pub port: u16,
    /// Every client, by the id the banks know it by.
    pub conns: HashMap<u64, Conn>,
    ip_clients: HashMap<utp::Token, u64>,
    next_client: u64,
    /// Kicked clients and when to close their connection (their reason goes out first).
    pub pending_drop: Vec<(u64, Instant)>,
    /// Kicked or dropped: nothing more from it reaches the bank.
    kicked: HashSet<u64>,
    /// Who may connect over Steam (the caps of the UDP side).
    gate: steam::Gate,
}

impl Transport {
    pub fn new(utp: Option<Utp>, port: u16) -> Transport {
        Transport { utp, steam: None, invite_code: None, port, conns: HashMap::new(), ip_clients: HashMap::new(), next_client: 1, pending_drop: Vec::new(), kicked: HashSet::new(), gate: Default::default() }
    }

    pub fn network(&self) -> &'static str {
        match (self.steam.is_some(), self.utp.is_some()) {
            (true, true) => "both",
            (true, false) => "steam",
            _ => "ip",
        }
    }

    /// The direct-IP socket the loop waits on.
    pub fn fd(&self) -> Option<RawFd> {
        self.utp.as_ref().map(|u| u.fd())
    }

    /// The direct-IP sessions' datagrams out, when there is something new to answer.
    pub fn flush(&mut self, wanted: bool) {
        if let Some(utp) = self.utp.as_mut().filter(|_| wanted) {
            utp.flush();
        }
    }

    pub fn clients(&self) -> Vec<u64> {
        self.conns.keys().copied().collect()
    }

    /// Whether a batch from `client` still reaches the game: it is connected and not kicked.
    pub fn accepts_from(&self, client: u64) -> bool {
        self.conns.contains_key(&client) && !self.kicked.contains(&client)
    }

    /// The clients that came over Steam.
    pub fn steam_clients(&self) -> HashSet<u64> {
        self.conns.iter().filter(|(_, c)| matches!(c, Conn::Steam { .. })).map(|(c, _)| *c).collect()
    }

    // ---- direct IP --------------------------------------------------------------------------

    pub fn poll_utp(&mut self) -> Vec<utp::Event> {
        self.utp.as_mut().map(Utp::poll).unwrap_or_default()
    }

    /// A direct-IP session that connected: its client id, and the body of the CONNECTED frame
    /// for the bank.
    pub fn accept_ip(&mut self, token: utp::Token, addr: SocketAddr) -> (u64, String) {
        let client = self.next_client;
        self.next_client += 1;
        info!("client" = client, "addr" = addr.to_string(); "client {client}: connected from {addr}");
        self.ip_clients.insert(token, client);
        self.conns.insert(client, Conn::Ip(token));
        let body = Connected { steam_id: None, ip: Some(addr.ip().to_string()), trusted: trusted(addr.ip(), crate::config::dev("trust-loopback").is_some()) };
        (client, msg::text(&body))
    }

    pub fn ip_client(&self, token: &utp::Token) -> Option<u64> {
        self.ip_clients.get(token).copied()
    }

    pub fn forget_ip(&mut self, token: &utp::Token) -> Option<u64> {
        self.ip_clients.remove(token)
    }

    // ---- what is done to a client -------------------------------------------------------------

    /// The connection ended from the client's side (or was dropped). Whether it was open.
    pub fn left(&mut self, client: u64) -> bool {
        if self.conns.remove(&client).is_none() {
            return false;
        }
        info!("client" = client; "client {client}: disconnected");
        self.ip_clients.retain(|_, c| *c != client);
        self.kicked.remove(&client);
        self.pending_drop.retain(|(c, _)| *c != client);
        true
    }

    /// Closes a connection from this side. Whether it was open.
    pub fn close(&mut self, client: u64) -> bool {
        match self.conns.remove(&client) {
            // Lingering: what was sent (a kick's reason) is delivered before the close.
            Some(Conn::Steam { connection, .. }) => {
                connection.close(NetConnectionEnd::App(AppNetConnectionEnd::generic_normal()), Some("closed by the server"), true);
            }
            Some(Conn::Ip(token)) => {
                self.ip_clients.remove(&token);
                if let Some(utp) = self.utp.as_mut() {
                    utp.close(&token);
                }
            }
            None => return false,
        }
        self.kicked.remove(&client);
        true
    }

    /// A batch to a client.
    pub fn send(&mut self, client: u64, batch: &[u8]) -> Sent {
        match self.conns.get(&client) {
            Some(Conn::Steam { connection, .. }) => {
                // A batch it got only part of would be garbage to it: the client goes, loudly.
                if let Err(e) = steam::send(connection, batch) {
                    warn!("client {client}: {e}; dropped");
                    self.kicked.insert(client);
                    self.close(client);
                    return Sent::Closed;
                }
                Sent::Steam
            }
            Some(Conn::Ip(token)) => {
                if let Some(utp) = self.utp.as_mut() {
                    utp.send(token, batch);
                }
                Sent::Queued
            }
            None => Sent::Nobody,
        }
    }

    /// Nothing more from the client reaches the game from here on.
    pub fn mark_kicked(&mut self, client: u64) {
        self.kicked.insert(client);
    }

    /// The client is closed `after` this moment, its reason having gone out. False when it was
    /// kicked already.
    pub fn drop_later(&mut self, client: u64, at: Instant) -> bool {
        let first = self.kicked.insert(client);
        if first {
            self.pending_drop.push((client, at));
        }
        first
    }

    /// The bank kicked a client: closed a moment later, if it is still here.
    pub fn kick(&mut self, client: u64, at: Instant) {
        if self.conns.contains_key(&client) {
            self.drop_later(client, at);
        }
    }

    /// The kicked clients whose moment has come, taken off the list.
    pub fn take_due(&mut self, now: Instant) -> Vec<u64> {
        let due: Vec<u64> = self.pending_drop.iter().filter(|(_, at)| *at <= now).map(|(c, _)| *c).collect();
        self.pending_drop.retain(|(_, at)| *at > now);
        due
    }

    /// Every direct-IP session told the server is going away.
    pub fn disconnect_all(&mut self) {
        if let Some(utp) = &mut self.utp {
            utp.tokens().iter().for_each(|t| utp.disconnect(t));
        }
        // Steam's connections, then its listen socket, then the login, in that order: leaving any of
        // them open at exit makes Steam's own shutdown assert ("we still have sockets open") and the
        // process end with a segmentation fault, a minidump in /tmp/dumps and a failed unit.
        self.conns.clear();
        drop(self.steam.take());
    }
}

/// Whether the SteamID a direct-IP client reports is believed: only from this machine, and
/// only when asked (PCDS_DEV=trust-loopback: tests, the load and probe tools). Behind a UDP
/// relay (playit.gg, frp, ssh -R) every player comes from 127.0.0.1.
fn trusted(ip: std::net::IpAddr, opted_in: bool) -> bool {
    opted_in && utp::is_loopback(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_trusted_only_when_asked() {
        let local = "127.0.0.1".parse().unwrap();
        assert!(!trusted(local, false), "a relay's players all come from 127.0.0.1");
        assert!(trusted(local, true) && trusted("::ffff:127.0.0.1".parse().unwrap(), true));
        assert!(!trusted("10.0.0.2".parse().unwrap(), true));
    }

    /// A kick is closed once, a moment later; a client that left meanwhile is not kicked again.
    #[test]
    fn a_kicked_client_is_dropped_once_when_its_moment_comes() {
        let mut net = Transport::new(None, 0);
        net.conns.insert(1, Conn::Ip([1; 8]));
        net.ip_clients.insert([1; 8], 1);
        let now = Instant::now();
        let later = now + std::time::Duration::from_millis(300);
        net.kick(1, later);
        net.kick(1, later);
        net.kick(9, later);
        assert_eq!(net.pending_drop.len(), 1, "once, and only for a client that is here");
        assert!(!net.accepts_from(1), "nothing more from it reaches the game");
        assert!(net.take_due(now).is_empty());
        assert_eq!(net.take_due(later), [1]);
        assert!(net.pending_drop.is_empty());
        assert!(net.close(1) && !net.close(1), "closed once");
        assert!(!net.conns.contains_key(&1) && net.ip_client(&[1; 8]).is_none());
        // Gone on its own: nothing is left of it, and it is not closed a second time.
        net.conns.insert(2, Conn::Ip([2; 8]));
        net.drop_later(2, later);
        assert!(net.left(2) && !net.left(2) && net.pending_drop.is_empty());
    }
}
