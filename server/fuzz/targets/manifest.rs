//! What an admin or a plugin author can put in a plugin's folder as text: the manifest, a data
//! plugin's rules, and the JSON files beside the plugins (approvals, trusted keys, config values,
//! scoped tokens, the state a flip carries). The first byte picks the parser.
use super::*;
use crate::plugins::ctx::{Ctx, Grant, World};
use crate::plugins::runtime::Runtime;
use pcds_plugin_api::query::{PlayerInfo, WorldInfo};
use pcds_plugin_api::Event;

struct Fixed(u32);

impl World for Fixed {
    fn info(&self) -> WorldInfo {
        WorldInfo { game_ms: 3_725_000, planet: "Prime".into(), players: self.0, ..Default::default() }
    }
    fn players(&self) -> Vec<PlayerInfo> {
        vec![]
    }
    fn rules(&self) -> String {
        "{}".into()
    }
}

pub fn run(data: &[u8]) {
    let Some((&which, rest)) = data.split_first() else { return };
    let text = String::from_utf8_lossy(rest);
    match which % 5 {
        0 => {
            if let Ok(m) = Manifest::parse(&text) {
                assert!(valid_id(m.id()));
                // What is shown to the admin is plain text.
                let p = &m.plugin;
                for shown in [&p.name, &p.description, &p.license] {
                    assert_eq!(crate::plugins::clean(shown), **shown, "a manifest line with a control character got through");
                }
                assert!(version_triple(&p.version).is_some());
                assert!(!p.entry.starts_with('/') && !p.entry.split('/').any(|c| c == ".." || c.is_empty()));
                for c in &p.capabilities {
                    assert!(pcds_plugin_api::caps::known(c));
                }
                // The kinds it subscribes to are within the capabilities it asks for.
                let grant = Grant::from_names(p.capabilities.iter().map(String::as_str));
                for k in m.kinds() {
                    assert!(grant.has(pcds_plugin_api::caps::for_event(k)), "{k:?} without its capability");
                }
                let limits = crate::plugins::limits::Limits::resolve(&m.budget, &Default::default());
                assert!(limits.fuel_per_event >= 1 && limits.memory_pages >= 1 && limits.tick_every_ms >= crate::plugins::limits::MIN_TICK_MS);
            }
        }
        1 => {
            let Ok(rules) = crate::plugins::data::Rules::parse(&text) else { return };
            let caps = rules.needed_caps();
            let _ = rules.kinds();
            let names = rules.command_names();
            let mut d = crate::plugins::data::Data::new(rules, Ctx::new("t", Grant::from_names(["chat.say", "world.read", "rules.write"])));
            let world = Fixed(3);
            let mut events: Vec<Event<'_>> = vec![
                Event::PlayerJoin { client: 1, name: "Ann", steam_id: "1", player_id: 1 },
                Event::PlayerLeave { client: 1, name: "Ann", player_id: 1 },
                Event::Tick { game_ms: 1000, tick: 1, players: 1, since_ms: 5, unix_ms: 1_700_000_000_000 },
                Event::Tick { game_ms: 100_000_000, tick: 2, players: 9, since_ms: 5, unix_ms: 1_700_000_000_000 },
                Event::Tick { game_ms: 4_000_000_000_000, tick: 3, players: 0, since_ms: 5, unix_ms: 1_700_000_000_000 },
            ];
            for n in &names {
                let command: &'static str = Box::leak(n.clone().into_boxed_str());
                events.push(Event::Command { client: 2, name: "Bo", command, args: vec!["{name}", "}", "{"], text: "!x", player_id: 2 });
            }
            for e in &events {
                d.ctx().begin(&world, 0);
                let _ = d.event(e, &[]);
                for a in d.ctx().end() {
                    if let crate::plugins::ctx::Action::Say(t) | crate::plugins::ctx::Action::Tell(_, t) = &a {
                        assert!(t.chars().count() <= crate::plugins::limits::SAY_CHARS);
                    }
                }
            }
            let _ = caps;
        }
        2 => {
            if let Ok(a) = serde_json::from_slice::<crate::plugins::approvals::Approvals>(rest) {
                let digest = "00";
                for (_, ap) in a.0.iter() {
                    let _ = crate::plugins::approvals::standing(Some(ap), digest, &["chat.say".to_string()], &crate::plugins::sign::Signed::No);
                }
                let again = serde_json::to_string(&a).unwrap();
                assert!(serde_json::from_str::<crate::plugins::approvals::Approvals>(&again).unwrap() == a);
            }
            if let Ok(k) = serde_json::from_slice::<crate::plugins::sign::Keys>(rest) {
                let again = serde_json::to_string(&k).unwrap();
                assert!(serde_json::from_str::<crate::plugins::sign::Keys>(&again).unwrap() == k);
                for key in &k.keys {
                    let _ = k.find(&key.id);
                    let _ = k.find(&key.name);
                }
            }
            if let Ok(c) = serde_json::from_slice::<crate::plugins::config::Configs>(rest) {
                for values in c.0.values() {
                    for v in values.values() {
                        let _ = crate::plugins::config::value_ok(v);
                    }
                }
            }
            if let Ok(t) = serde_json::from_slice::<crate::tokens::Tokens>(rest) {
                for token in &t.tokens {
                    let _ = token.public();
                    let _ = t.find(&token.id);
                }
            }
            if let Ok(s) = serde_json::from_slice::<crate::plugins::state::Saved>(rest) {
                let again = serde_json::to_string(&s).unwrap();
                assert!(serde_json::from_str::<crate::plugins::state::Saved>(&again).is_ok(), "state written by the server does not read back");
            }
        }
        3 => {
            // A plugin's state file and a signature file.
            let _ = crate::plugins::sign::parse_public(&text);
            let _ = serde_json::from_slice::<crate::plugins::sign::Sig>(rest);
            let _ = crate::plugins::clean(&text);
        }
        _ => {
            // A command line of the chat-command and the manifest's pieces.
            let _ = valid_id(&text);
            let _ = version_triple(&text);
        }
    }
}
