//! The bank's loop: what it does every FRAME, in this order. The front's frames first (a client
//! connects, sends, leaves; an admin request; a save, a stop, a chat reply, a flip), then the
//! game's tick, then what the tick made goes out (the batches of mail.rs, the kicks, the chat
//! events and rules the front acts on), then it waits for the next frame or the front's next
//! frame, whichever comes first. A flip (HANDOFF) is served in the middle of the frames, at a tick
//! boundary: what is queued goes out, the snapshot goes to the front, and the bank waits for EXIT
//! (the new bank runs) or RESUME (it failed).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::admin;
use super::forward::{forward_chat, forward_ops};
use super::mail::{check_loading, post, send};
use super::panic::{contained, guarded, taint, ticked};
use super::persist::{autosave, final_save, persist, persist_later, settle};
use super::plugins_glue;
use super::status::{Ctx, TickTimes};
use crate::game::Game;
use crate::idmap::FxHashMap;
use crate::ipc::{self, Frame, Link};
use crate::msg::{self, AdminCall, Connected, Ops, SnapshotNote};
use crate::{chathooks, plugins, wire};

pub(super) struct Bank {
    pub game: Game,
    pub link: Link,
    pub save_path: String,
    pub build_id: Option<String>,
    pub data_dir: String,
}

/// What the loop keeps from one turn to the next.
struct Turn {
    /// Who got ConnectionApproved: in the world for a restored game.
    approved: HashSet<u64>,
    /// Kicked, until the front says the connection is closed.
    kicked: HashSet<u64>,
    /// Bytes approved clients that are no players yet may still be sent.
    loading: HashMap<u64, i64>,
    mail: FxHashMap<u64, wire::Batch>,
    urgent: Vec<u64>,
    next_frame: Instant,
    movement_sent: Instant,
    /// Loops in this second, when the second began, loops in the last second.
    loops: (u32, Instant, u32),
    /// Work time and longest loop, this second and the last.
    busy: (Duration, Duration),
    busy_last: (Duration, Duration),
    ticks: TickTimes,
    last_ops: Option<Ops>,
}

impl Bank {
    /// The server loop's game half, its sends going to the front.
    pub(super) fn serve(mut self) {
        let mut turn = Turn {
            approved: self.game.clients.values().filter(|c| c.approved).map(|c| c.id).collect(),
            kicked: HashSet::new(),
            loading: HashMap::new(),
            mail: Default::default(),
            urgent: Vec::new(),
            next_frame: Instant::now(),
            movement_sent: Instant::now(),
            loops: (0u32, Instant::now(), 0u32),
            busy: (Duration::ZERO, Duration::ZERO),
            busy_last: (Duration::ZERO, Duration::ZERO),
            ticks: TickTimes::default(),
            last_ops: None,
        };
        // The hooks' settings, once at the start (a restored game's too).
        self.game.ext.chat.changed = true;
        loop {
            let woke = Instant::now();
            let frame = woke >= turn.next_frame;
            if frame {
                turn.next_frame = Instant::now() + crate::FRAME;
                turn.loops.0 += 1;
            }
            if turn.loops.1.elapsed() >= Duration::from_secs(1) {
                turn.loops = (0, Instant::now(), turn.loops.0);
                turn.busy_last = std::mem::take(&mut turn.busy);
            }
            while let Some(f) = self.link.next() {
                self.handle(&mut turn, f);
            }
            self.tick(&mut turn, frame);
            self.deliver(&mut turn, frame);
            let work = woke.elapsed();
            turn.busy = (turn.busy.0 + work, turn.busy.1.max(work));
            let pause = turn.next_frame.saturating_duration_since(Instant::now());
            if ipc::wait(&[self.link.fd()], &[], pause)[0] && !self.link.fill(usize::MAX) {
                println!("bank: the front is gone; saving and stopping");
                std::process::exit(final_save(&mut self.game, &self.save_path));
            }
        }
    }

    /// One frame from the front.
    fn handle(&mut self, t: &mut Turn, f: Frame) {
        let game = &mut self.game;
        match f.kind {
            ipc::CONNECTED => {
                let c: Connected = msg::parse(&f.body).unwrap_or_default();
                guarded(game, f.n, &self.save_path, |game| game.connect(f.n, c.steam_id, c.trusted, c.ip));
            }
            // Kicked this loop too (game.kicks goes out after it): nothing more counts.
            ipc::DATA if !t.kicked.contains(&f.n) && !game.kicks.iter().any(|(c, _)| *c == f.n) && game.clients.contains_key(&f.n) => {
                let before = game.net.out.len();
                guarded(game, f.n, &self.save_path, |game| game.receive(f.n, &f.body));
                if game.net.out[before.min(game.net.out.len())..].iter().any(|(to, _, _)| to.reaches(f.n)) && !t.urgent.contains(&f.n) {
                    t.urgent.push(f.n);
                }
            }
            ipc::GONE => {
                t.approved.remove(&f.n);
                t.kicked.remove(&f.n);
                if game.clients.contains_key(&f.n) {
                    println!("client {}: left the game", f.n);
                    guarded(game, f.n, &self.save_path, |game| game.disconnect(f.n));
                }
            }
            ipc::ADMIN => {
                let r: AdminCall = msg::parse(&f.body).unwrap_or_default();
                let ctx = Ctx { ticks: &t.ticks, save_path: &self.save_path, build_id: self.build_id.as_deref(), data_dir: &self.data_dir, loop_rate: t.loops.2, busy: t.busy_last };
                let request = || admin::request(game, &r.method, &r.path, &r.body, &ctx);
                let (status, body) = contained("an admin request", request).unwrap_or_else(|| admin::error(500, "the server failed on this request"));
                self.link.send(ipc::ANSWER, f.n, admin::frame(status, body).as_bytes());
            }
            ipc::SAVE => autosave(game, &self.save_path),
            ipc::STOP => std::process::exit(final_save(game, &self.save_path)),
            ipc::SAY => {
                contained("a chat hook's reply", || chathooks::say(game, &String::from_utf8_lossy(&f.body)));
            }
            ipc::HANDOFF => self.flip(t, f.n),
            _ => {}
        }
    }

    /// A flip: a tick boundary with nothing queued, the snapshot to the front, and the wait for what
    /// the front decides.
    fn flip(&mut self, t: &mut Turn, n: u64) {
        let game = &mut self.game;
        // What is waiting goes out first.
        post(game, &mut t.approved, &mut t.mail, &mut t.loading);
        let all: Vec<u64> = t.mail.keys().copied().collect();
        send(&mut self.link, &mut t.mail, &all, &mut t.loading);
        forward_chat(game, &mut self.link);
        let (started, got_at) = (Instant::now(), ipc::now_ns());
        // The plugins' states go into the snapshot (within their budget altogether).
        plugins_glue::checkpoint_for_flip(game);
        match crate::handoff::snapshot_memfd(game, &mut Vec::new()) {
            Ok(fd) => {
                let len = std::fs::File::from(fd.try_clone().expect("dup")).metadata().map(|m| m.len()).unwrap_or(0);
                let head = SnapshotNote { bytes: Some(len), ms: Some(started.elapsed().as_secs_f64() * 1000.0), got_at, done_at: ipc::now_ns() };
                self.link.send_fd(ipc::SNAPSHOT, n, &msg::body(&head), Some(fd));
                // The world stays in memory until the new bank runs.
                match self.link.recv() {
                    Some(f) if f.kind == ipc::RESUME => {
                        game.resume_after_stand();
                        println!("bank: resumed, the new bank failed")
                    }
                    Some(f) if f.kind == ipc::EXIT => {
                        settle(game, true);
                        std::process::exit(0)
                    }
                    _ => std::process::exit(final_save(game, &self.save_path)),
                }
            }
            Err(e) => {
                eprintln!("bank: no snapshot ({e}); going on");
                self.link.send(ipc::SNAPSHOT, n, &[]);
            }
        }
    }

    /// The game's tick (once per frame), the plugin folder, and a save that is asked for.
    fn tick(&mut self, t: &mut Turn, frame: bool) {
        let game = &mut self.game;
        let tick_started = Instant::now();
        let ok = !frame || ticked(game);
        if frame {
            t.ticks.record(tick_started, tick_started.elapsed());
        }
        if !ok {
            // The whole world may be half-updated: keep what there is; the front starts the
            // bank again from the save.
            eprintln!("the game tick panicked: saving and stopping");
            taint();
            let _ = persist(game, &self.save_path);
            std::process::exit(1);
        }
        // Plugins appear, change and go while the world runs: the folder is looked at between ticks.
        if frame {
            plugins_glue::poll(game);
        }
        settle(game, false);
        if std::mem::take(&mut game.save_requested) && !persist_later(game, &self.save_path) {
            game.save_requested = true;
        }
    }

    /// What the tick made goes out: the mail, the kicks, and what the front acts on.
    fn deliver(&mut self, t: &mut Turn, frame: bool) {
        let game = &mut self.game;
        // What a client's own packets brought it goes out now; the rest (other players'
        // movement and doings, the world's changes) every MOVEMENT_EVERY.
        post(game, &mut t.approved, &mut t.mail, &mut t.loading);
        if frame && t.movement_sent.elapsed() >= crate::MOVEMENT_EVERY {
            t.movement_sent = Instant::now();
            let all: Vec<u64> = t.mail.keys().copied().collect();
            send(&mut self.link, &mut t.mail, &all, &mut t.loading);
        } else if !t.urgent.is_empty() {
            send(&mut self.link, &mut t.mail, &t.urgent, &mut t.loading);
        }
        t.urgent.clear();
        check_loading(game, &mut t.loading);
        // Kicked or refused: what it was sent first, then the reason; the front closes the
        // connection a moment later and says GONE.
        for (client, reason) in std::mem::take(&mut game.kicks) {
            if !t.kicked.insert(client) {
                continue;
            }
            println!("client {client}: kicked ({reason})");
            send(&mut self.link, &mut t.mail, &[client], &mut HashMap::new());
            self.link.send(ipc::DATA, client, &crate::protocol::reason_batch(&reason));
            self.link.send(ipc::KICK, client, &[]);
        }
        forward_chat(game, &mut self.link);
        // The plugin event stream's doorbell, for the front's long polls (ipc::STREAM).
        if let Some(bell) = plugins::stream_bell(game) {
            self.link.send(ipc::STREAM, 0, bell.as_bytes());
        }
        forward_ops(game, &mut self.link, &mut t.last_ops);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The snapshot's head prints the bytes the json! macro made of it.
    #[test]
    fn the_snapshot_head_prints_what_the_json_macro_did() {
        for (bytes, ms, got_at, done_at) in [(0u64, 0.0f64, 0u64, 0u64), (123_456_789, 41.7000000001, 1_234_567_890_123, 1_234_567_990_123), (u64::from(u32::MAX) + 1, 1e-7, 7, 8)] {
            let old = json!({"bytes": bytes, "ms": ms, "gotAt": got_at, "doneAt": done_at}).to_string();
            assert_eq!(msg::text(&SnapshotNote { bytes: Some(bytes), ms: Some(ms), got_at, done_at }), old);
        }
    }
}
