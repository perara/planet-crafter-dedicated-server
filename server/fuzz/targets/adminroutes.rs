//! The bank's admin routes with hostile JSON bodies (and odd paths) on a running game: every
//! request is answered with a status below 500 (a 500 is the bank saying it broke itself, apart
//! from the ones that report a disk), the answer is JSON, and the game still snapshots.
use super::admin::request;
use super::status::Ctx;
use crate::game::Game;
use serde_json::Value;
use crate::game::fuzz::{tables, world};
use serde_json::{json, Map};
use std::cell::RefCell;

struct Cur<'a>(&'a [u8]);

impl Cur<'_> {
    fn u8(&mut self) -> u8 {
        let (b, rest) = self.0.split_first().map_or((0, self.0), |(b, r)| (*b, r));
        self.0 = rest;
        b
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
    fn pick<'b, T>(&mut self, items: &'b [T]) -> &'b T {
        &items[self.u8() as usize % items.len()]
    }
}

const PATHS: &[&str] = &[
    "/api/status", "/api/ping", "/api/save", "/api/plugins", "/api/plugins/reload", "/api/plugins/enable", "/api/plugins/disable", "/api/plugins/config", "/api/plugins/trust", "/api/plugins/untrust", "/api/plugins/tokens", "/api/plugins/tokens/revoke", "/api/plugins/events", "/api/kick", "/api/ban", "/api/unban", "/api/rules", "/api/config", "/api/teleport", "/api/worlds",
    "/api/worlds/new", "/api/worlds/remove", "/api/worlds/file", "/api/worlds/restore", "/api/backup", "/api/backup/s3", "/api/pause", "/api/map",
];

const KEYS: &[&str] = &[
    "id", "name", "value", "position", "world", "backup", "paused", "grant", "digest", "publicKey", "key", "scopes", "ratePerMinute", "planet", "start", "seed", "mode", "dying", "force", "after", "types", "limit", "consumer", "unregister", "wait", "text", "speed", "speed.growth", "limits.buildings", "access.whitelist", "access.bannedNames", "save.autosaveMinutes",
    "chat", "webhooks", "death.consequences", "version", "check", "onlyNewer", "rollback", "growth", "x", "",
];
const STRINGS: &[&str] = &["", "Ann", "Bob", "ann", "Prime", "x", "world", "w", "../x", "a/b", "rules", "access", ".", "..", "1", "-1", "0.5", "76561198000000001", "127.0.0.1", "10.0.0.0/8", "1.2.3.4-1.2.3.9", "\u{3000}", "Ａｎｎ", "a\u{0}b", "event.chat", "chat.say", "hello-stats", "welcome-rules", "discord-notify", "hook", "net.http", "Standard", "DropSomeItems", "Creative", "Chill", "Selenea", "{}", "[]", "true", "ffff", "https://x.example/hook", "ab", "pcds-plugin-public-key-v1\nabcd"];

fn string(c: &mut Cur) -> Value {
    match c.u8() % 12 {
        0 => json!("x".repeat(c.u8() as usize * 40)),
        1 => json!("é".repeat(c.u8() as usize)),
        2 => json!(format!("{}{}", c.pick(STRINGS), c.u32())),
        _ => json!(c.pick(STRINGS)),
    }
}

fn value(c: &mut Cur, depth: u32) -> Value {
    match c.u8() % if depth > 3 { 9 } else { 12 } {
        0 => Value::Null,
        1 => json!(c.u8() % 2 == 0),
        2 => json!(c.u8() as i64 - 3),
        3 => json!(c.u32() as i64),
        4 => json!(i64::MAX),
        5 => json!(i64::MIN),
        6 => json!(f32::from_bits(c.u32()) as f64),
        7 => json!([1e300, -1e300, 0.1][c.u8() as usize % 3]),
        8 => string(c),
        9 => Value::Array((0..c.u8() % 5).map(|_| value(c, depth + 1)).collect()),
        _ => Value::Object((0..c.u8() % 6).map(|_| (c.pick(KEYS).to_string(), value(c, depth + 1))).collect::<Map<_, _>>()),
    }
}

thread_local! {
    static GAME: RefCell<Option<(Game, u32)>> = const { RefCell::new(None) };
}

pub fn run(data: &[u8]) {
    let (t, _) = tables();
    let dir = std::env::temp_dir().join(format!("pcds-fuzz-routes-{}", std::process::id()));
    let save_path = dir.join("world.json").to_string_lossy().to_string();
    GAME.with(|g| {
        let mut g = g.borrow_mut();
        if g.as_ref().is_none_or(|(_, n)| *n >= 200) {
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("other.json"), crate::game::fuzz::save().to_text()).unwrap();
            // The example plugins, in the folder the plugin endpoints work on.
            let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins");
            for id in ["hello-stats", "welcome-rules", "discord-notify"] {
                let to = dir.join("plugins").join(id);
                std::fs::create_dir_all(&to).unwrap();
                for file in ["plugin.toml", "plugin.wasm", "plugin.sig", "rules.toml"] {
                    let _ = std::fs::copy(examples.join(id).join(file), to.join(file));
                }
            }
            let mut game = world(t, crate::game::fuzz::save());
            super::plugins_glue::attach(&mut game, super::plugins_glue::prepare(&save_path), false);
            *g = Some((game, 0));
        }
        let (game, n) = g.as_mut().unwrap();
        *n += 1;
        let mut c = Cur(data);
        let paths: &[&str] = PATHS;
        for _ in 0..1 + c.u8() % 6 {
            let method = *c.pick(&["GET", "POST", "POST", "PUT", "get"]);
            let mut path = c.pick(paths).to_string();
            if c.u8() % 4 == 0 {
                path.push_str(["", "/x", "/enable", "/tokens", "//", "/../etc"][c.u8() as usize % 6]);
            }
            let body = value(&mut c, 0);
            let ticks = Default::default();
            let ctx = Ctx { ticks: &ticks, save_path: &save_path, build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
            let (status, answer) = request(game, method, &path, &body, &ctx);
            assert!(serde_json::from_str::<Value>(&answer).is_ok(), "{method} {path} {body}: the answer is no JSON: {answer}");
            assert!((200..600).contains(&status), "{method} {path}: status {status}");
            assert!(status != 500 || answer.contains("disk") || answer.contains("has not started") || answer.contains("could not") || answer.contains("kept") || answer.contains("cannot"), "{method} {path} {body}: {status} {answer}");
            if c.u8() % 3 == 0 {
                game.time += 0.2;
                game.tick();
                game.net.out.clear();
            }
        }
        if *n % 50 == 0 {
            let mut out = Vec::new();
            if let Err(e) = crate::handoff::snapshot_to(game, &mut out) {
                panic!("the admin API made a world that cannot be snapshotted: {e}");
            }
        }
    });
}
