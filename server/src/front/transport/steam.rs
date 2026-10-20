//! The Steam side of the transport: the game server login and its P2P listen socket, the caps on
//! who may connect, and the game's Steam message framing ("Split<n>" chunks, the 2.5 MiB message
//! limit).

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use steamworks::networking_sockets::{ListenSocket, NetConnection};
use steamworks::networking_types::{AppNetConnectionEnd, ListenSocketEvent, NetConnectionEnd, NetworkingConfigEntry, NetworkingConfigValue, SendFlags};
use steamworks::{Server, ServerMode, SteamServersConnected};

use super::{Conn, Transport};
use crate::utp;

pub(in crate::front) const APP_ID: &str = "1284190";
/// The game's Steam transport splits messages above this size (see GameServerSockets.cs).
const CHUNK: usize = 524_288;
/// The game's Steam transport's receive buffer (SteamNetworkingSocketsTransport: 2621440).
const MAX_STEAM_MESSAGE: usize = 5 * CHUNK;
/// Steam's send buffer per connection: several joins' worth.
const SEND_BUFFER: i32 = 32 << 20;
/// The most a client's "Split" message may add up to (two of the game's chunks).
pub(in crate::front) const MAX_SPLIT: usize = 1 << 20;
/// Steam connections the server takes at once, and from one SteamID: the UDP side's caps
/// (utp::MAX_CONNECTIONS; a player has one connection, a few more while an old one closes).
const MAX_PER_STEAM_ID: usize = 4;

/// The server's Steam login and the P2P socket invite codes point to.
pub(super) type Login = (Server, ListenSocket);

/// A connection that was just accepted, not yet in the table.
pub(in crate::front) struct Joined {
    pub client: u64,
    pub steam_id: u64,
    connection: NetConnection,
}

/// What one look at the listen socket found.
pub(in crate::front) struct Poll {
    pub joined: Vec<Joined>,
    /// Clients whose connection ended.
    pub gone: Vec<u64>,
}

impl Transport {
    /// Logs in as a Steam game server (anonymously, or with a login token for a fixed SteamID)
    /// and opens the P2P socket invite codes point to. Err: Steam failed and there is no direct
    /// IP to go on with.
    pub(in crate::front) fn start_steam(&mut self, game_version: &str) -> Result<(), String> {
        if crate::env("PCDS_STEAM").as_deref() == Some("off") {
            return Ok(());
        }
        let session = crate::config::env_number("PCDS_SESSION") as u32;
        match login(game_version, session) {
            Ok((steam, listen, code)) => {
                self.invite_code = Some(code);
                self.steam = Some((steam, listen));
                Ok(())
            }
            Err(e) if self.utp.is_some() => {
                eprintln!("steam: {e}; continuing with direct IP only");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Steam's callbacks run, and the listen socket's events read: connection requests are
    /// accepted or refused by the caps, new connections and ended ones are returned. None
    /// without Steam.
    pub(in crate::front) fn steam_poll(&mut self) -> Option<Poll> {
        let (steam, listen) = self.steam.as_ref()?;
        steam.run_callbacks();
        let (mut joined, mut gone, mut accepted): (Vec<Joined>, Vec<u64>, Vec<u64>) = (Vec::new(), Vec::new(), Vec::new());
        while let Some(event) = listen.try_receive_event() {
            match event {
                ListenSocketEvent::Connecting(request) => {
                    let steam_id = request.remote().steam_id().map(|id| id.raw()).unwrap_or(0);
                    let open: Vec<u64> = self.conns.values().filter_map(|c| if let Conn::Steam { steam_id, .. } = c { Some(*steam_id) } else { None }).chain(joined.iter().map(|j| j.steam_id)).chain(accepted.iter().copied()).collect();
                    match self.gate.admit(steam_id, Instant::now(), &open) {
                        Ok(()) => {
                            accepted.push(steam_id);
                            let _ = request.accept();
                        }
                        Err(why) => {
                            println!("{steam_id}: Steam connection refused ({why})");
                            request.reject(NetConnectionEnd::App(AppNetConnectionEnd::generic_normal()), Some("the server cannot take this connection now"));
                        }
                    }
                }
                ListenSocketEvent::Connected(event) => {
                    let steam_id = event.remote().steam_id().map(|id| id.raw()).unwrap_or(0);
                    let client = self.next_client;
                    self.next_client += 1;
                    // Its disconnect names the connection by this (not by SteamID: the same
                    // player may have an old connection still closing).
                    let connection = event.take_connection();
                    let _ = connection.set_connection_user_data(client as i64);
                    joined.push(Joined { client, steam_id, connection });
                }
                ListenSocketEvent::Disconnected(event) => {
                    let peer = event.remote().steam_id().map(|id| id.raw()).unwrap_or(0);
                    println!("{peer}: disconnected ({:?})", event.end_reason());
                    let fresh: Vec<(u64, u64)> = joined.iter().map(|j| (j.client, j.steam_id)).collect();
                    gone.extend(disconnected(event.user_data(), peer, &fresh));
                }
            }
        }
        Some(Poll { joined, gone })
    }

    /// A new Steam connection into the table.
    pub(in crate::front) fn add_steam(&mut self, joined: Joined) {
        let Joined { client, steam_id, connection } = joined;
        println!("client {client}: connected over Steam as {steam_id}");
        self.conns.insert(client, Conn::Steam { connection, split: None, steam_id });
    }

    /// The batches the Steam clients sent (up to 64 messages each), put together from their
    /// splits. A client with a split the game never sends is dropped: added to `gone`.
    pub(in crate::front) fn steam_receive(&mut self, gone: &mut Vec<u64>) -> Vec<(u64, Vec<u8>)> {
        let mut incoming = Vec::new();
        for (client, conn) in self.conns.iter_mut() {
            if let Conn::Steam { connection, split, .. } = conn {
                for message in connection.receive_messages(64).unwrap_or_default() {
                    match reassemble(split, message.data()) {
                        Ok(batch) => incoming.extend(batch.map(|b| (*client, b))),
                        Err(e) => {
                            println!("client {client}: dropped ({e})");
                            gone.push(*client);
                            break;
                        }
                    }
                }
            }
        }
        incoming
    }
}

fn login(version: &str, session_number: u32) -> Result<(Server, ListenSocket, String), String> {
    let (steam, client) = Server::init(Ipv4Addr::UNSPECIFIED, 0, 0xFFFF, ServerMode::NoAuthentication, version)
        .map_err(|e| format!("Steam game server init failed ({e}); is steamclient.so in ~/.steam/sdk64? Install the Steam client, or run 'pcds setup' again to fetch it; PCDS_STEAM=off in server.env serves direct IP only"))?;
    steam.set_product(APP_ID);
    steam.set_mod_dir("Planet Crafter");
    steam.set_game_description("Planet Crafter standalone server");
    steam.set_dedicated_server(true);
    let connected = Arc::new(AtomicBool::new(false));
    let logon = connected.clone();
    let _logon = steam.register_callback(move |_: SteamServersConnected| logon.store(true, Ordering::SeqCst));
    match crate::env("PCDS_STEAM_TOKEN") {
        Some(token) => steam.log_on(&token),
        None => steam.log_on_anonymous(),
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !connected.load(Ordering::SeqCst) {
        if Instant::now() > deadline {
            return Err(format!("Steam game server logon timed out after 30 s: is this machine online{}? PCDS_STEAM=off in server.env serves direct IP only", if crate::env("PCDS_STEAM_TOKEN").is_some() { ", and is PCDS_STEAM_TOKEN a valid login token for app 1284190 (steamcommunity.com/dev/managegameservers)" } else { "" }));
        }
        steam.run_callbacks();
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(client);
    // Room for the largest join queued at once (the default is 512 KiB); connections take it
    // from the listen socket.
    let buffer = NetworkingConfigEntry::new_int32(NetworkingConfigValue::SendBufferSize, SEND_BUFFER);
    let listen = steam.networking_sockets().create_listen_socket_p2p(0, [buffer]).map_err(|_| "P2P listen socket failed")?;
    let steam_id = steam.steam_id().raw();
    let code = format!("{}{:04}", crate::protocol::invite_code(steam_id), session_number);
    println!("steam: game server {steam_id}, invite code {code}{}", if crate::env("PCDS_STEAM_TOKEN").is_some() { "" } else { " (it changes at every start: PCDS_STEAM_TOKEN in server.env keeps it)" });
    Ok((steam, listen, code))
}

/// The game's Steam transport frames batches over 512 KiB as "Split<n>" plus n chunks. Its
/// receive buffer holds MAX_STEAM_MESSAGE, and it reads n as one digit: bigger batches go as
/// several, split between messages. Err when Steam refuses a send (the client then has part
/// of a batch) or one message is too big for the game.
pub(super) fn send(connection: &NetConnection, batch: &[u8]) -> Result<(), String> {
    for part in parts(batch)? {
        let refused = |e| format!("Steam refused a send of {} bytes ({e:?})", part.len());
        if part.len() <= CHUNK {
            connection.send_message(&part, SendFlags::RELIABLE).map_err(refused)?;
            continue;
        }
        let chunks: Vec<&[u8]> = part.chunks(CHUNK).collect();
        connection.send_message(format!("Split{}", chunks.len()).as_bytes(), SendFlags::RELIABLE).map_err(refused)?;
        for c in chunks {
            connection.send_message(c, SendFlags::RELIABLE).map_err(refused)?;
        }
    }
    Ok(())
}

/// A batch as batches the game's Steam transport takes (each at most MAX_STEAM_MESSAGE).
pub(in crate::front) fn parts(batch: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    if batch.len() <= MAX_STEAM_MESSAGE {
        return Ok(vec![batch.to_vec()]);
    }
    let (mut parts, mut part, mut size) = (Vec::new(), crate::wire::Batch::default(), 16);
    for (kind, body) in crate::wire::messages(batch) {
        // The message's kind and length are at most 5 bytes each.
        let n = body.len() + 10;
        if 16 + n > MAX_STEAM_MESSAGE {
            return Err(format!("a message of {} bytes (type {kind}), and the game's Steam transport takes at most {MAX_STEAM_MESSAGE} at once: this world can be joined over direct IP only", body.len()));
        }
        if size + n > MAX_STEAM_MESSAGE {
            parts.push(std::mem::take(&mut part).finish());
            size = 16;
        }
        part.push(kind, body);
        size += n;
    }
    if !part.is_empty() {
        parts.push(part.finish());
    }
    Ok(parts)
}

/// A client's messages back into batches. Err for a split the game never sends (a count other
/// than 1..=9, or over MAX_SPLIT in all): the caller drops the client.
pub(in crate::front) fn reassemble(split: &mut Option<(Vec<u8>, usize)>, data: &[u8]) -> Result<Option<Vec<u8>>, &'static str> {
    if let Some((buffer, left)) = split {
        if buffer.len() + data.len() > MAX_SPLIT {
            return Err("split message over 1 MiB");
        }
        buffer.extend_from_slice(data);
        *left -= 1;
        if *left == 0 {
            return Ok(split.take().map(|(buffer, _)| buffer));
        }
        return Ok(None);
    }
    if data.len() == 6 && &data[..5] == b"Split" {
        let n = char::from(data[5]).to_digit(10).filter(|n| (1..=9).contains(n)).ok_or("bad split count")?;
        *split = Some((Vec::new(), n as usize));
        return Ok(None);
    }
    Ok(Some(data.to_vec()))
}

/// The client a Steam disconnect is about: the one whose connection carries its id; one that
/// closed before its id was set (connected and gone within one poll) is the one of that
/// SteamID that connected in this poll.
pub(in crate::front) fn disconnected(user_data: i64, peer: u64, joined_now: &[(u64, u64)]) -> Vec<u64> {
    match u64::try_from(user_data).ok().filter(|c| *c > 0) {
        Some(client) => vec![client],
        None => joined_now.iter().filter(|(_, s)| *s == peer).map(|(c, _)| *c).collect(),
    }
}

/// Whether a Steam connection request is accepted: the caps direct-IP sessions have (utp.rs):
/// connections in all, connections of one SteamID, and new connections of one SteamID within
/// a window. Steam gives every request a verified identity but no address, so the SteamID
/// stands where the IP does there.
#[derive(Default)]
pub(super) struct Gate {
    handshakes: HashMap<u64, (Instant, u32)>,
}

impl Gate {
    /// `open`: the SteamIDs of the connections there are (and the ones accepted just now).
    /// Err says which cap.
    pub(super) fn admit(&mut self, steam_id: u64, now: Instant, open: &[u64]) -> Result<(), String> {
        if open.len() >= utp::MAX_CONNECTIONS {
            return Err(format!("{} connections already", open.len()));
        }
        let same = open.iter().filter(|s| **s == steam_id).count();
        if same >= MAX_PER_STEAM_ID {
            return Err(format!("{same} connections of this SteamID already"));
        }
        let (since, n) = self.handshakes.entry(steam_id).or_insert((now, 0));
        if now.saturating_duration_since(*since) > utp::HANDSHAKE_WINDOW {
            (*since, *n) = (now, 0);
        }
        if *n >= utp::MAX_HANDSHAKES {
            return Err(format!("more than {} new connections in {} s", utp::MAX_HANDSHAKES, utp::HANDSHAKE_WINDOW.as_secs()));
        }
        *n += 1;
        // Connections are capped, so this holds the SteamIDs of the last window and little more.
        if self.handshakes.len() > 4 * utp::MAX_CONNECTIONS {
            self.handshakes.retain(|_, (t, _)| now.saturating_duration_since(*t) <= utp::HANDSHAKE_WINDOW);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steam connections have the caps direct-IP sessions have: in all, per SteamID, and how often a
    /// SteamID may connect. (The event loop that asks it needs a Steam login and is not run by the tests.)
    #[test]
    fn steam_connections_have_the_udp_caps() {
        let now = Instant::now();
        let mut gate = Gate::default();
        // One SteamID: four at once (a reconnect while the old one closes), the fifth refused.
        let mut open: Vec<u64> = Vec::new();
        for _ in 0..MAX_PER_STEAM_ID {
            gate.admit(7, now, &open).unwrap();
            open.push(7);
        }
        assert!(gate.admit(7, now, &open).unwrap_err().contains("4 connections of this SteamID"));
        // Others are not held back by it.
        gate.admit(8, now, &open).unwrap();
        // The total: 128, whoever asks.
        let full: Vec<u64> = (0..utp::MAX_CONNECTIONS as u64).map(|i| 1000 + i).collect();
        assert!(gate.admit(9, now, &full).unwrap_err().contains("128 connections already"));
        assert!(gate.admit(9, now, &full[1..]).is_ok());
        // The rate: four new connections within the window, whatever became of them.
        let mut gate = Gate::default();
        for i in 0..utp::MAX_HANDSHAKES {
            gate.admit(5, now + Duration::from_secs(u64::from(i)), &[]).unwrap();
        }
        assert!(gate.admit(5, now + Duration::from_secs(5), &[]).unwrap_err().contains("new connections in 10 s"));
        assert!(gate.admit(6, now + Duration::from_secs(5), &[]).is_ok());
        assert!(gate.admit(5, now + utp::HANDSHAKE_WINDOW + Duration::from_secs(1), &[]).is_ok(), "the window moves on");
        // Many SteamIDs asking once each do not make the table grow without end.
        let mut gate = Gate::default();
        for i in 0..2000u64 {
            gate.admit(i, now + Duration::from_secs(i / 10), &[]).unwrap();
        }
        assert!(gate.handshakes.len() <= 4 * utp::MAX_CONNECTIONS + 1);
    }

    #[test]
    fn a_steam_disconnect_names_its_connection_not_the_steam_id() {
        // Client 3 is the same player's new connection: the old one's close is client 2's.
        assert_eq!(disconnected(2, 76561198000000042, &[(3, 76561198000000042)]), [2]);
        assert_eq!(disconnected(-1, 76561198000000042, &[(3, 76561198000000042), (4, 1)]), [3]);
        assert!(disconnected(-1, 5, &[]).is_empty());
    }

    /// Batches past the game's 2.5 MiB go as several, split between messages, so "Split" never
    /// needs two digits; one message over it is an error, not garbage.
    #[test]
    fn steam_batches_fit_the_games_transport() {
        let small = crate::wire::batch(&[(1, vec![7; 100])]);
        assert_eq!(parts(&small).unwrap(), [small.clone()]);
        let big = crate::wire::batch(&[(1, vec![1; 1 << 20]), (2, vec![2; 1 << 20]), (3, vec![3; 1 << 20]), (4, vec![4; 10])]);
        let pieces = parts(&big).unwrap();
        assert_eq!(pieces.len(), 2);
        assert!(pieces.iter().all(|p| p.len() <= MAX_STEAM_MESSAGE && p.len().div_ceil(CHUNK) <= 9));
        let back: Vec<(u32, Vec<u8>)> = pieces.iter().flat_map(|p| crate::wire::messages(p).into_iter().map(|(k, b)| (k, b.to_vec())).collect::<Vec<_>>()).collect();
        assert_eq!(back, crate::wire::messages(&big).into_iter().map(|(k, b)| (k, b.to_vec())).collect::<Vec<_>>());
        let world = crate::wire::batch(&[(1, vec![0; 3 << 20])]);
        assert!(parts(&world).unwrap_err().contains("direct IP"));
    }

    #[test]
    fn steam_splits_are_bounded() {
        let mut split = None;
        assert_eq!(reassemble(&mut split, b"batch"), Ok(Some(b"batch".to_vec())));
        for bad in [b"Split0", b"Split/", b"Split:"] {
            assert!(reassemble(&mut None, bad).is_err());
        }
        assert_eq!(reassemble(&mut split, b"Split2"), Ok(None));
        assert_eq!(reassemble(&mut split, b"ab"), Ok(None));
        assert_eq!(reassemble(&mut split, b"cd"), Ok(Some(b"abcd".to_vec())));
        assert!(split.is_none());
        reassemble(&mut split, b"Split3").unwrap();
        reassemble(&mut split, &vec![0; MAX_SPLIT]).unwrap();
        assert!(reassemble(&mut split, b"x").is_err());
    }
}
