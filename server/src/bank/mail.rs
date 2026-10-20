//! What the bank sends to the clients, through the front: the game's queued messages sorted into
//! each approved client's batch (`post`), the batches written to the link in the frames it holds
//! (`send`), a client that is no player yet bounded in what it is sent (`check_loading`), and the
//! message a kicked player is shown is `protocol::reason_batch`). The front only moves the bytes.

use std::collections::{HashMap, HashSet};

use crate::game::Game;
use crate::ipc::{self, Link};
use crate::net::Target;
use crate::{protocol, wire};

/// What a client may be sent before it is a player (PlayerReady): the world in its
/// ConnectionApproved and this much more (the world's changes while it loads). A session from a
/// forged address never gets further: its world is all a spoofer can have sent to the victim.
pub(super) const LOADING_ALLOWANCE: i64 = 8 << 20;

/// Kicks a client that is no player yet and was sent more than its allowance; forgets the
/// ones that became players or left.
pub(super) fn check_loading(game: &mut Game, loading: &mut HashMap<u64, i64>) {
    loading.retain(|c, left| match game.clients.get(c) {
        Some(client) if client.ready.is_none() => {
            if *left < 0 {
                info!("client {c}: sent {} MiB more than its world while not a player; dropped", LOADING_ALLOWANCE >> 20);
                game.kicks.push((*c, "MENU_Network_Error_Timeout".into()));
            }
            *left >= 0
        }
        _ => false,
    });
}

/// `loading`: what each client that is no player yet may still be sent, counted down.
pub(super) fn send(link: &mut Link, mail: &mut crate::idmap::FxHashMap<u64, wire::Batch>, clients: &[u64], loading: &mut HashMap<u64, i64>) {
    for c in clients {
        if let Some(b) = mail.remove(c).filter(|b| !b.is_empty()) {
            let batch = b.finish();
            // Over the frame limit the front would end the link: that client goes instead (it
            // cannot go on without the batch). A join batch is ~14 bytes per world object.
            if !ipc::fits(&batch) {
                warn!("client {c}: a batch of {} bytes is over the {} bytes a frame holds; it is dropped", batch.len(), ipc::MAX_FRAME);
                link.send(ipc::DATA, *c, &protocol::reason_batch("The world is too large for this server to send"));
                link.send(ipc::KICK, *c, &[]);
                continue;
            }
            if let Some(left) = loading.get_mut(c) {
                *left -= batch.len() as i64;
            }
            link.queue(ipc::DATA, *c, &batch, None, 0);
        }
    }
    // All the clients' batches in one write.
    link.drain();
}

/// Sorts the queued messages into each approved client's batch, in queue order. A client gets
/// messages queued after its ConnectionApproved: everything before is in the world it was sent.
pub(super) fn post(game: &mut Game, approved: &mut HashSet<u64>, mail: &mut crate::idmap::FxHashMap<u64, wire::Batch>, loading: &mut HashMap<u64, i64>) {
    // A join in the queue: who gets a message depends on whether it came before or after it.
    if game.net.out.iter().any(|(_, kind, _)| *kind == protocol::CONNECTION_APPROVED) {
        return post_in_order(game, approved, mail, loading);
    }
    // Client by client: each message with its header is written once into one buffer, and a
    // client's batch is that buffer's runs of the messages that reach it (nearly all of them reach
    // nearly everyone: a player's movement goes to all the others), so a batch is a few copies.
    // The movement of every player goes to every other: this is players squared.
    let mut out = std::mem::take(&mut game.net.out);
    if !approved.is_empty() && !out.is_empty() {
        let mut messages = wire::Writer::new();
        let ends: Vec<usize> = out
            .iter()
            .map(|(_, kind, body)| {
                messages.packed_u32(*kind).packed_u32(body.len() as u32).bytes(body);
                messages.len()
            })
            .collect();
        for c in approved.iter() {
            let mut give = |count: usize, from: usize, to: usize| {
                let batch = mail.entry(*c).or_default();
                if batch.is_empty() {
                    batch.reserve(messages.len());
                }
                batch.push_run(count, &messages.0[from..to]);
            };
            // The run of messages being gathered: (from, to, how many).
            let mut run: Option<(usize, usize, usize)> = None;
            let mut start = 0;
            for ((target, _, _), end) in out.iter().zip(&ends) {
                if target.reaches(*c) {
                    run = Some(match run {
                        Some((from, _, n)) => (from, *end, n + 1),
                        None => (start, *end, 1),
                    });
                } else if let Some((from, to, n)) = run.take() {
                    give(n, from, to);
                }
                start = *end;
            }
            if let Some((from, to, n)) = run {
                give(n, from, to);
            }
        }
    }
    out.clear();
    game.net.out = out;
}

/// `post` message by message, for a queue with a ConnectionApproved in it.
fn post_in_order(game: &mut Game, approved: &mut HashSet<u64>, mail: &mut crate::idmap::FxHashMap<u64, wire::Batch>, loading: &mut HashMap<u64, i64>) {
    for (to, kind, body) in game.net.out.drain(..) {
        if kind == protocol::CONNECTION_APPROVED {
            if let Target::Only(c) = to {
                approved.insert(c);
                loading.insert(c, body.len() as i64 + LOADING_ALLOWANCE);
                mail.entry(c).or_default().push(kind, &body);
            }
            continue;
        }
        for c in approved.iter().filter(|c| to.reaches(**c)) {
            mail.entry(*c).or_default().push(kind, &body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game;
    use std::os::unix::net::UnixStream;

    /// A client that is no player yet gets its world and LOADING_ALLOWANCE more, then is
    /// dropped; a player has no such bound.
    #[test]
    fn a_client_that_never_becomes_a_player_is_sent_its_world_and_a_bound() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || std::io::copy(&mut theirs, &mut std::io::sink()));
        let mut link = Link::new(ours);
        let (mut approved, mut mail, mut loading) = (HashSet::new(), Default::default(), HashMap::new());
        game::tests::approve(&mut game, 1);
        game::tests::join(&mut game, 2, "Ann");
        post(&mut game, &mut approved, &mut mail, &mut loading);
        assert!(loading[&1] > LOADING_ALLOWANCE && loading[&2] > LOADING_ALLOWANCE, "both got their world");
        send(&mut link, &mut mail, &[1, 2], &mut loading);
        check_loading(&mut game, &mut loading);
        assert!(!loading.contains_key(&2), "a player is not counted");
        assert!(game.kicks.is_empty() && loading[&1] > 0 && loading[&1] < LOADING_ALLOWANCE);
        // The world's changes while it loads, past the allowance.
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..=(LOADING_ALLOWANCE >> 20) {
            game.out_to(Target::All, (protocol::CLIENT_RPC, chunk.clone()));
            post(&mut game, &mut approved, &mut mail, &mut loading);
            send(&mut link, &mut mail, &[1, 2], &mut loading);
            check_loading(&mut game, &mut loading);
        }
        assert_eq!(game.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [1]);
    }

    /// The batches `post` makes client by client are those the message-by-message way makes
    /// (post_in_order, the old code), whoever the messages go to: runs of messages reaching
    /// everyone, one client left out, one client only, clients not in the game, empty bodies.
    #[test]
    fn post_makes_the_same_batches_as_going_message_by_message() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut random = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let mut queue = |game: &mut Game, messages: u64| {
            game.net.out.clear();
            for _ in 0..messages {
                let to = match random(6) {
                    0 => Target::Only(1 + random(7)),
                    1 | 2 => Target::Except(1 + random(7)),
                    _ => Target::All,
                };
                let size = [0, 1, 28, 28, 28, 200, 1500, 31][random(8) as usize];
                let body = (0..size).map(|_| random(256) as u8).collect();
                game.out_to(to, (protocol::NETWORK_TRANSFORM + random(300) as u32, body));
            }
        };
        let finish = |mail: crate::idmap::FxHashMap<u64, wire::Batch>| {
            let mut all: Vec<(u64, Vec<u8>)> = mail.into_iter().map(|(c, b)| (c, b.finish())).collect();
            all.sort();
            all
        };
        // Clients 1 to 5 are in the game; 6 and 7 are addressed but are not.
        let approved: HashSet<u64> = (1..=5).collect();
        for messages in [1, 2, 3, 10, 64, 300] {
            let (mut fast, mut slow) = (Default::default(), Default::default());
            queue(&mut game, messages);
            let kept = game.net.out.clone();
            post(&mut game, &mut approved.clone(), &mut fast, &mut HashMap::new());
            assert!(game.net.out.is_empty());
            game.net.out = kept;
            post_in_order(&mut game, &mut approved.clone(), &mut slow, &mut HashMap::new());
            let (fast, slow) = (finish(fast), finish(slow));
            assert_eq!(fast, slow, "{messages} messages");
            assert!(fast.iter().all(|(c, _)| approved.contains(c)));
        }
        // Nobody approved: nothing is kept; a client no message reaches has no batch.
        let mut none = Default::default();
        queue(&mut game, 5);
        post(&mut game, &mut HashSet::new(), &mut none, &mut HashMap::new());
        assert!(none.is_empty() && game.net.out.is_empty());
        let mut few = Default::default();
        game.net.out.clear();
        game.out_to(Target::Only(1), (protocol::NETWORK_TRANSFORM, vec![7]));
        post(&mut game, &mut [1, 2].into_iter().collect(), &mut few, &mut HashMap::new());
        assert_eq!(few.len(), 1);
    }

    /// A batch over the frame limit never reaches the socket (the front would end the link and
    /// the bank with it): its client is told why and dropped; the link goes on.
    #[test]
    fn a_batch_over_the_frame_limit_drops_its_client_not_the_bank() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let (mut front, mut bank) = (Link::new(a), Link::new(b));
        let mut mail: crate::idmap::FxHashMap<u64, wire::Batch> = Default::default();
        mail.entry(4).or_default().push(protocol::RPC, &vec![0; ipc::MAX_FRAME]);
        mail.entry(5).or_default().push(protocol::RPC, b"small");
        send(&mut bank, &mut mail, &[4, 5], &mut HashMap::new());
        // Any frame over the limit, whoever sends it: not sent.
        bank.send(ipc::DATA, 6, &vec![0; ipc::MAX_FRAME]);
        bank.send(ipc::GONE, 0, &[]);
        let mut frames = Vec::new();
        while frames.last() != Some(&(ipc::GONE, 0)) {
            assert!(front.fill(usize::MAX));
            while let Some(f) = front.next() {
                frames.push((f.kind, f.n));
            }
        }
        assert!(front.broken.is_none());
        assert_eq!(frames, [(ipc::DATA, 4), (ipc::KICK, 4), (ipc::DATA, 5), (ipc::GONE, 0)]);
    }
}
