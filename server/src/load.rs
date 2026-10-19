//! Load test: PLAYERS scripted players join over direct IP and play for SECONDS. Each walks (a
//! transform every network tick) and every ACTION_MS does one of what players do most: crafts
//! an item into its backpack, drops one and picks it up again, builds a container, eats,
//! drinks, mines, chats, sorts or throws an item away. At the end it prints the round trip of
//! each call the server answers (craft, build, chat) as percentiles, and what players received.
//!
//! Usage: pcds load HOST:PORT DATA_DIR [PLAYERS=8] [SECONDS=60] [ACTION_MS=200] [NAME=Load]
//! (the players are named Load0, Load1...; NAME gives another prefix, for a second group)
//! Each player connects from its own 127.0.0.N address (the server allows 8 per address), so
//! HOST must be a loopback address of this machine.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::data::{stable_hash, NetData};
use crate::protocol::{self, CLIENT_RPC, CONNECTION_APPROVED, CONNECTION_REQUEST, CREATE_OBJECT, NETWORK_TRANSFORM, RPC};
use crate::rpcspec::Spec;
use crate::utp::{Event, Token, Utp};
use crate::wire::{self, Reader, Writer};

/// The calls whose answer is timed, and the answer that ends each.
const TIMED: [(&str, &str); 3] = [
    ("craft", "InventoriesHandler.AddNewItemClientRpc"),
    ("build", "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc"),
    ("chat", "ChatHandler.ReceiveChatMessageClientRpc"),
];

struct Bot<'a> {
    utp: Utp,
    token: Token,
    data: &'a NetData,
    spec: &'a Spec,
    client: u64,
    player: u64,
    inventory: i64,
    home: [f32; 3],
    items: Vec<i64>,
    dropped: Vec<i64>,
    builds: u32,
    mining: bool,
    rng: u64,
    sent: Vec<(u32, Vec<u8>)>,
    waiting: HashMap<&'static str, VecDeque<Instant>>,
    rtt: HashMap<&'static str, Vec<f64>>,
    received: (u64, u64, u64),
}

#[derive(Default)]
struct Report {
    rtt: HashMap<&'static str, Vec<f64>>,
    received: (u64, u64, u64),
    actions: u64,
    failed: Option<String>,
}

impl Bot<'_> {
    fn random(&mut self, n: u64) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng % n
    }

    fn behaviour(&self, class: &str) -> (u64, u16) {
        if let Some(b) = self.data.prefabs["Player"].behaviours.iter().find(|b| b.class == class) {
            return (self.player, b.behaviour_id);
        }
        self.data.scene_objects.iter().find_map(|o| o.behaviours.iter().find(|b| b.class == class).map(|b| (o.id, b.behaviour_id))).unwrap_or_else(|| panic!("no {class}"))
    }

    /// Queues an RPC for this tick's batch; `timed` starts its clock.
    fn call(&mut self, name: &str, args: &[Value], timed: Option<&'static str>) {
        let class = name.split('.').next().unwrap_or("");
        let (object, behaviour) = self.behaviour(class);
        let (method, body) = self.spec.encode(name, args);
        let mut w = Writer::new();
        w.packed_u64(self.client).packed_u64(object).packed_u16(behaviour).packed_u32(method).bytes(&body);
        self.sent.push((RPC, w.0));
        if let Some(kind) = timed {
            self.waiting.entry(kind).or_default().push_back(Instant::now());
        }
    }

    fn flush(&mut self) {
        if !self.sent.is_empty() {
            let batch = wire::batch(&std::mem::take(&mut self.sent));
            self.utp.send(&self.token, &batch);
        }
        self.utp.flush();
    }

    /// Reads what arrived; false once the server dropped us.
    fn receive(&mut self) -> bool {
        for event in self.utp.poll() {
            match event {
                Event::Data(_, batch) => {
                    self.received.1 += batch.len() as u64;
                    self.received.2 += 1;
                    for (kind, body) in wire::messages(&batch) {
                        self.received.0 += 1;
                        self.message(kind, body);
                    }
                }
                Event::Disconnected(_) => return false,
                Event::Connected(..) => {}
            }
        }
        true
    }

    fn message(&mut self, kind: u32, body: &[u8]) {
        match kind {
            CONNECTION_APPROVED => {
                if let Some((owner, objects)) = protocol::read_approved(body) {
                    self.client = owner;
                    self.find_player(&objects);
                }
            }
            CREATE_OBJECT => {
                if let Some(o) = protocol::read_scene_object(&mut Reader::new(&body[1..])) {
                    self.find_player(&[o]);
                }
            }
            CLIENT_RPC => {
                let mut r = Reader::new(body);
                let (Some(_), Some(_), Some(method)) = (r.packed_u64(), r.packed_u16(), r.packed_u32()) else { return };
                let Some(name) = self.spec.get(method).map(|m| m.name.clone()) else { return };
                let args = || self.spec.decode(method, r.rest()).unwrap_or_default();
                match name.as_str() {
                    "PlayerMainController.InitPlayerClientRpc" => {
                        let a = args();
                        self.inventory = a[0].as_i64().unwrap_or(0);
                        let p = &a[2];
                        self.home = [0, 1, 2].map(|i| p[i].as_f64().unwrap_or(0.0) as f32);
                    }
                    "InventoriesHandler.AddNewItemClientRpc" => {
                        let a = args();
                        if a[0].as_bool() == Some(true) {
                            self.items.push(a[1].as_i64().unwrap_or(0));
                        }
                        self.answered("craft");
                    }
                    "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc" => self.answered("build"),
                    "ChatHandler.ReceiveChatMessageClientRpc" if args()[0].as_u64() == Some(self.client) => self.answered("chat"),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn find_player(&mut self, objects: &[protocol::SceneObjectInfo]) {
        let hash = self.data.prefabs["Player"].hash;
        if let Some(o) = objects.iter().find(|o| o.hash == hash && o.owner == self.client && self.client != 0) {
            self.player = o.id;
        }
    }

    fn answered(&mut self, kind: &'static str) {
        if let Some(sent) = self.waiting.get_mut(kind).and_then(VecDeque::pop_front) {
            self.rtt.entry(kind).or_default().push(sent.elapsed().as_secs_f64() * 1000.0);
        }
    }

    fn walk(&mut self, t: f32) {
        let a = t / 8.0 + (self.rng % 628) as f32 / 100.0;
        let mut w = Writer::new();
        // Position x and z (flags 2 and 8) of our own player.
        w.packed_u64(self.player).packed_i32(0).packed_u32(2 | 8).packed_i32(1);
        for f in [self.home[0] + 6.0 * a.cos(), self.home[2] + 6.0 * a.sin()] {
            w.bytes(&f.to_le_bytes());
        }
        self.sent.push((NETWORK_TRANSFORM, w.0));
    }

    fn act(&mut self, seq: u64) {
        let (iron, container) = (stable_hash("Iron"), stable_hash("Container1"));
        let near = |b: &Bot, r: f32| json!([b.home[0] + r, b.home[1] + 0.5, b.home[2] + r]);
        if self.mining {
            self.mining = false;
            return self.call("PlayerShareState.StopMiningServerRpc", &[], None);
        }
        match self.random(100) {
            0..=24 if self.items.len() < 10 => self.call("InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(iron), json!(self.inventory)], Some("craft")),
            0..=24 => {
                let wo = self.items.remove(0);
                self.call("InventoriesHandler.RemoveItemFromInventoryServerRpc", &[json!(wo), json!(self.inventory), json!(true)], None);
            }
            25..=39 if !self.items.is_empty() => {
                let wo = self.items.remove(0);
                let at = near(self, 1.5);
                self.call("WorldObjectsHandler.DropOnFloorServerRpc", &[json!(wo), at, json!(0.6), json!(true), json!(false), json!(0), json!(0.0)], None);
                self.dropped.push(wo);
            }
            40..=54 if !self.dropped.is_empty() => {
                let wo = self.dropped.remove(0);
                self.call("InventoriesHandler.AddWorldObjectToInventoryServerRpc", &[json!(wo), json!(iron), json!(self.inventory), json!(true)], None);
                self.items.push(wo);
            }
            55..=59 if self.builds < 20 => {
                self.builds += 1;
                let at = near(self, 3.0 + self.builds as f32 * 2.5);
                self.call("WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc", &[json!(container), at, json!([0.0, 0.0, 0.0, 1.0]), json!(true), json!(false), json!(true), json!(true)], Some("build"));
            }
            60..=64 => self.call("PlayerGaugesHandler.EatServerRpc", &[json!(5)], None),
            65..=69 => self.call("PlayerGaugesHandler.DrinkServerRpc", &[json!(5)], None),
            70..=79 => {
                self.mining = true;
                self.call("PlayerShareState.StartMiningServerRpc", &[json!(0), json!(1.0), json!(0)], None);
            }
            80..=89 => self.call("ChatHandler.SendChatMessageServerRpc", &[json!(format!("load {} {seq}", self.client))], Some("chat")),
            _ => self.call("InventoriesHandler.SortInventoryServerRpc", &[json!(self.inventory)], None),
        }
    }
}

fn play(n: usize, server: SocketAddr, data: &NetData, spec: &Spec, request: &[u8], seconds: f64, action_ms: u64, prefix: &str) -> Report {
    let mut report = Report::default();
    let local: SocketAddr = format!("127.0.0.{}:0", 2 + n % 250).parse().unwrap();
    let (utp, token) = match Utp::connect(local, server, Duration::from_secs(10)) {
        Ok(c) => c,
        Err(e) => {
            report.failed = Some(format!("connect: {e}"));
            return report;
        }
    };
    let mut bot = Bot { utp, token, data, spec, client: 0, player: 0, inventory: 0, home: [0.0; 3], items: Vec::new(), dropped: Vec::new(), builds: 0, mining: false, rng: 0x9e37_79b9_7f4a_7c15 ^ (n as u64 + 1).wrapping_mul(0x2545_f491_4f6c_dd1d), sent: vec![(CONNECTION_REQUEST, request.to_vec())], waiting: HashMap::new(), rtt: HashMap::new(), received: (0, 0, 0) };
    bot.flush();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready_sent = false;
    while bot.inventory == 0 {
        if Instant::now() > deadline || !bot.receive() {
            report.failed = Some(format!("no player after 30 s (client {}, player {})", bot.client, bot.player));
            return report;
        }
        if bot.player != 0 && !ready_sent {
            ready_sent = true;
            bot.call("PlayerMainController.PlayerReadyServerRpc", &[json!(0), json!(format!("{prefix}{n}"))], None);
        }
        bot.flush();
        std::thread::sleep(Duration::from_millis(2));
    }
    let start = Instant::now();
    let tick = Duration::from_secs_f64(1.0 / data.tick_rate as f64);
    let (mut next_walk, mut next_act, mut seq) = (start, start + Duration::from_millis(bot.random(action_ms.max(1))), 0u64);
    while start.elapsed().as_secs_f64() < seconds {
        let now = Instant::now();
        if now >= next_walk {
            next_walk += tick;
            bot.walk(start.elapsed().as_secs_f32());
        }
        if now >= next_act {
            next_act += Duration::from_millis(action_ms);
            seq += 1;
            bot.act(seq);
        }
        bot.flush();
        if !bot.receive() {
            report.failed = Some(format!("dropped after {:.1} s", start.elapsed().as_secs_f64()));
            break;
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    bot.utp.disconnect(&bot.token);
    report.rtt = bot.rtt;
    report.received = bot.received;
    report.actions = seq;
    report
}

fn percentiles(mut v: Vec<f64>) -> String {
    if v.is_empty() {
        return "no answers".into();
    }
    v.sort_by(f64::total_cmp);
    let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    format!("p50 {:.2} ms, p95 {:.2}, p99 {:.2}, max {:.2} (n={})", at(0.5), at(0.95), at(0.99), v[v.len() - 1], v.len())
}

pub fn run(args: &[String]) {
    let (Some(addr), Some(dir)) = (args.first(), args.get(1)) else {
        eprintln!("usage: pcds load HOST:PORT DATA_DIR [PLAYERS] [SECONDS] [ACTION_MS] [NAME]");
        std::process::exit(2);
    };
    let number = |i: usize, default: u64| args.get(i).and_then(|v| v.parse().ok()).unwrap_or(default);
    let (players, seconds, action_ms) = (number(2, 8) as usize, number(3, 60) as f64, number(4, 200));
    let prefix = args.get(5).map_or("Load", String::as_str);
    let server: SocketAddr = addr.parse().expect("HOST:PORT");
    let (data, spec) = crate::network_tables(dir);
    let request = crate::world::hex(std::fs::read_to_string(format!("{dir}/connection-request.hex")).expect("connection-request.hex").trim());
    let reports: Vec<Report> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..players)
            .map(|n| {
                let (data, spec, request) = (&data, &spec, &request);
                // Joins spread over a moment, as players arrive.
                std::thread::sleep(Duration::from_millis(50));
                scope.spawn(move || play(n, server, data, spec, request, seconds, action_ms, prefix))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    });
    let failed: Vec<String> = reports.iter().enumerate().filter_map(|(i, r)| r.failed.as_ref().map(|f| format!("player {i}: {f}"))).collect();
    println!("load: {players} players for {seconds} s, an action every {action_ms} ms each: {} actions", reports.iter().map(|r| r.actions).sum::<u64>());
    for (kind, _) in TIMED {
        println!("  {kind:5} round trip: {}", percentiles(reports.iter().flat_map(|r| r.rtt.get(kind).cloned().unwrap_or_default()).collect()));
    }
    let (messages, bytes, batches) = reports.iter().fold((0, 0, 0), |(m, b, n), r| (m + r.received.0, b + r.received.1, n + r.received.2));
    let per = |x: u64| x as f64 / players as f64 / seconds;
    println!("  received per player: {:.0} messages/s in {:.0} batches/s, {:.1} KB/s", per(messages), per(batches), per(bytes) / 1024.0);
    if !failed.is_empty() {
        println!("  failed: {}", failed.join("; "));
        std::process::exit(1);
    }
}
