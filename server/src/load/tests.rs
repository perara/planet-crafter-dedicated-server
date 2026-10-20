//! Churn and rejoin: load-bot sessions that behave badly, to see what a running server does with
//! them. Ignored tests, not commands: they need a server to talk to (tools/chaos/churn.py starts
//! one), and the release has none of this.
//!
//!   PCDS_LOAD_SERVER=127.0.0.1:8421 PCDS_TEST_DATA=run/data/2.103 \
//!     cargo test --release load::tests::churn -- --ignored --nocapture
//!
//! `churn`: workers that keep starting sessions of a kind drawn from their own seeded stream:
//! play a few seconds and say goodbye, play and vanish (the socket just dropped), get approved
//! and never become a player (and say goodbye or vanish), only handshake, or join and leave at
//! once. One JSON line per session, and a summary at the end.
//! `rejoin`: players that join again when they are dropped (a restarted world, a kick), as a
//! game client does; one JSON line per join.
//!
//! Set by hand: PCDS_LOAD_SERVER (HOST:PORT, a loopback address; required), PCDS_LOAD_PLAYERS
//! (workers or players, default 8), PCDS_LOAD_SECONDS (default 60), PCDS_LOAD_SEED (churn's
//! random stream, default 1), PCDS_LOAD_REUSE_NAMES (churn: each worker always joins under the
//! same name), PCDS_LOAD_CHAT_BYTES (every chat line this long).
//! The bots receive worlds of any size (the 1 MiB limit of the batches is lifted for them).

use super::*;

fn number(name: &str, default: u64) -> u64 {
    crate::env(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn chat_pad() -> usize {
    number("PCDS_LOAD_CHAT_BYTES", 0) as usize
}

/// How a session ends: a disconnect message, or the socket just dropped.
#[derive(Clone, Copy, PartialEq)]
enum End {
    Clean,
    Vanish,
}

/// What a session does after connecting: become a player (or only be approved), for how long.
#[derive(Clone, Copy)]
struct Plan {
    ready: bool,
    seconds: f64,
    end: End,
    action_ms: u64,
}

struct Outcome {
    /// Seconds from the first packet to having a player (None: never).
    joined: Option<f64>,
    actions: u64,
    failed: Option<String>,
}

/// `play`, with a name, a plan for how it ends, and a time to a player.
fn session(n: usize, name: &str, server: SocketAddr, tables: (&NetData, &Spec), request: &[u8], plan: Plan) -> Outcome {
    let (data, spec) = tables;
    let mut out = Outcome { joined: None, actions: 0, failed: None };
    let local: SocketAddr = format!("127.0.0.{}:0", 2 + n % 250).parse().unwrap();
    let began = Instant::now();
    let (utp, token) = match Utp::connect(local, server, Duration::from_secs(10)) {
        Ok(c) => c,
        Err(e) => {
            out.failed = Some(format!("connect: {e}"));
            return out;
        }
    };
    let mut bot = Bot { utp, token, data, spec, client: 0, player: 0, inventory: 0, home: [0.0; 3], items: Vec::new(), dropped: Vec::new(), builds: 0, mining: false, rng: 0x9e37_79b9_7f4a_7c15 ^ (n as u64 + 1).wrapping_mul(0x2545_f491_4f6c_dd1d), sent: vec![(CONNECTION_REQUEST, request.to_vec())], waiting: HashMap::new(), rtt: HashMap::new(), received: (0, 0, 0) };
    bot.flush();
    let finish = |mut bot: Bot, mut out: Outcome, actions: u64| {
        if plan.end == End::Clean {
            bot.utp.disconnect(&bot.token);
        }
        out.actions = actions;
        out
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready_sent = false;
    while bot.inventory == 0 {
        if !plan.ready && bot.player != 0 {
            // Approved, with its world, and never a player: holds the slot.
            let hold = Instant::now() + Duration::from_secs_f64(plan.seconds);
            while Instant::now() < hold && bot.receive() {
                bot.flush();
                std::thread::sleep(Duration::from_millis(5));
            }
            return finish(bot, out, 0);
        }
        if Instant::now() > deadline || !bot.receive() {
            out.failed = Some(format!("no player after 30 s (client {}, player {})", bot.client, bot.player));
            return out;
        }
        if bot.player != 0 && !ready_sent && plan.ready {
            ready_sent = true;
            bot.call("PlayerMainController.PlayerReadyServerRpc", &[json!(0), json!(name)], None);
        }
        bot.flush();
        std::thread::sleep(Duration::from_millis(2));
    }
    out.joined = Some(began.elapsed().as_secs_f64());
    let start = Instant::now();
    let tick = Duration::from_secs_f64(1.0 / data.tick_rate as f64);
    let (mut next_walk, mut next_act, mut seq) = (start, start + Duration::from_millis(bot.random(plan.action_ms.max(1))), 0u64);
    while start.elapsed().as_secs_f64() < plan.seconds {
        let now = Instant::now();
        if now >= next_walk {
            next_walk += tick;
            bot.walk(start.elapsed().as_secs_f32());
        }
        if plan.action_ms > 0 && now >= next_act {
            next_act += Duration::from_millis(plan.action_ms);
            seq += 1;
            bot.act(seq);
        }
        bot.flush();
        if !bot.receive() {
            out.failed = Some(format!("dropped after {:.1} s", start.elapsed().as_secs_f64()));
            return finish(bot, out, seq);
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    finish(bot, out, seq)
}

/// The server, the tables and the connection request the bots need; None without game tables.
fn target() -> Option<(SocketAddr, NetData, Spec, Vec<u8>)> {
    let dir = crate::test_data()?;
    let server: SocketAddr = crate::env("PCDS_LOAD_SERVER").expect("PCDS_LOAD_SERVER=HOST:PORT").parse().expect("HOST:PORT");
    crate::utp::allow_big_batches();
    let (data, spec) = crate::network_tables(dir);
    let request = crate::world::hex(std::fs::read_to_string(format!("{dir}/connection-request.hex")).expect("connection-request.hex").trim());
    Some((server, data, spec, request))
}

fn line(v: Value) {
    println!("{v}");
}

#[test]
#[ignore]
fn rejoin() {
    let Some((server, data, spec, request)) = target() else { return };
    let (players, seconds) = (number("PCDS_LOAD_PLAYERS", 8) as usize, number("PCDS_LOAD_SECONDS", 60) as f64);
    let began = Instant::now();
    std::thread::scope(|scope| {
        for n in 0..players {
            let (data, spec, request) = (&data, &spec, &request);
            std::thread::sleep(Duration::from_millis(50));
            scope.spawn(move || {
                while began.elapsed().as_secs_f64() < seconds {
                    let left = seconds - began.elapsed().as_secs_f64();
                    let r = session(n, &format!("Load{n}"), server, (data, spec), request, Plan { ready: true, seconds: left, end: End::Clean, action_ms: 200 });
                    let t = (began.elapsed().as_secs_f64() * 10.0).round() / 10.0;
                    line(json!({"t": t, "n": n, "joined": r.joined, "actions": r.actions, "failed": r.failed}));
                    if r.failed.is_some() {
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            });
        }
    });
}

#[test]
#[ignore]
fn churn() {
    let Some((server, data, spec, request)) = target() else { return };
    let (workers, seconds, seed) = (number("PCDS_LOAD_PLAYERS", 8) as usize, number("PCDS_LOAD_SECONDS", 60) as f64, number("PCDS_LOAD_SEED", 1));
    let reuse = crate::env("PCDS_LOAD_REUSE_NAMES").is_some();
    let began = Instant::now();
    let counts = std::sync::Mutex::new(std::collections::BTreeMap::<String, (u64, u64)>::new());
    std::thread::scope(|scope| {
        for w in 0..workers {
            let (data, spec, request, counts) = (&data, &spec, &request, &counts);
            scope.spawn(move || {
                let mut rng = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (w as u64 + 1).wrapping_mul(0x2545_f491_4f6c_dd1d);
                let mut random = |n: u64| {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    rng % n
                };
                let mut i = 0u64;
                while began.elapsed().as_secs_f64() < seconds {
                    i += 1;
                    let pick = random(100);
                    let secs = 1.0 + random(9000) as f64 / 1000.0;
                    let name = if reuse { format!("Churn{w}") } else { format!("Churn{w}x{i}") };
                    let (kind, ready, end, hold) = match pick {
                        0..=34 => ("play", true, End::Clean, secs),
                        35..=54 => ("play-vanish", true, End::Vanish, secs),
                        55..=69 => ("noready", false, End::Clean, 3.0 + secs),
                        70..=79 => ("noready-vanish", false, End::Vanish, 3.0 + secs),
                        80..=89 => ("handshake", false, End::Vanish, secs),
                        _ => ("quick", true, End::Clean, 0.2),
                    };
                    let started = Instant::now();
                    let (ok, joined, failed) = if kind == "handshake" {
                        let local: SocketAddr = format!("127.0.0.{}:0", 2 + w % 250).parse().unwrap();
                        match Utp::connect(local, server, Duration::from_secs(10)) {
                            Ok(c) => {
                                std::thread::sleep(Duration::from_secs_f64(hold));
                                drop(c);
                                (true, None, None)
                            }
                            Err(e) => (false, None, Some(e.to_string())),
                        }
                    } else {
                        let r = session(w, &name, server, (data, spec), request, Plan { ready, seconds: hold, end, action_ms: 200 });
                        (r.failed.is_none(), r.joined, r.failed)
                    };
                    let t = (began.elapsed().as_secs_f64() * 10.0).round() / 10.0;
                    line(json!({"t": t, "w": w, "i": i, "kind": kind, "name": name, "ok": ok, "joined": joined, "failed": failed, "took": started.elapsed().as_secs_f64()}));
                    let mut c = counts.lock().unwrap();
                    let e = c.entry(kind.to_string()).or_default();
                    e.0 += 1;
                    e.1 += u64::from(!ok);
                }
            });
        }
    });
    line(json!({"summary": counts.lock().unwrap().iter().map(|(k, (n, f))| (k.clone(), json!({"sessions": n, "failed": f}))).collect::<serde_json::Map<_, _>>()}));
}
