//! hello-stats: two chat commands and a counter that survives restarts and hot updates.
//!
//!   !stats   the world's numbers (players, planet, terraformation, time), told to the asker
//!   !uses    how many times the two commands were used on this world
//!
//! It shows the four things most plugins do: read the world (`world.read`), answer a chat
//! command (`chat.say`), react to an event (a player joining is logged), and keep state across
//! restarts, reloads and hot updates, with a version number and a migration from the version before.

use pcds_plugin_api::*;

#[derive(Default)]
struct Hello {
    stats_uses: u32,
    uses_uses: u32,
}

/// Version 1 of the state held one counter (all uses); version 2 holds one per command.
const STATE_VERSION: u32 = 2;

impl Plugin for Hello {
    fn load(host: &dyn Host, state: Option<State<'_>>) -> Self {
        let mut me = Hello::default();
        let Some(state) = state else { return me };
        let mut r = Reader::new(state.bytes);
        match state.version {
            // The migration: version 1's single counter counts as !stats uses.
            1 => me.stats_uses = r.u32().unwrap_or(0),
            2 => {
                me.stats_uses = r.u32().unwrap_or(0);
                me.uses_uses = r.u32().unwrap_or(0);
            }
            v => host.log(Level::Warn, &format!("state version {v} is newer than this plugin knows: starting empty")),
        }
        me
    }

    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
        match event {
            Event::Command { client, command: "stats", .. } => {
                self.stats_uses += 1;
                let text = match host.world() {
                    Ok(w) => format!(
                        "{} on {}: {} players, terraformation {:.0}, world time {}h {:02}m{}",
                        "Server",
                        w.planet,
                        w.players,
                        w.terraformation,
                        w.game_ms / 3_600_000,
                        w.game_ms / 60_000 % 60,
                        if w.standing_still { " (standing still)" } else { "" }
                    ),
                    Err(e) => format!("No stats: {e:?}"),
                };
                let _ = host.tell(*client, &text);
            }
            Event::Command { client, command: "uses", .. } => {
                self.uses_uses += 1;
                let _ = host.tell(*client, &format!("!stats was used {} times, !uses {} times on this world", self.stats_uses, self.uses_uses));
            }
            Event::PlayerJoin { name, .. } => host.log(Level::Info, &format!("{name} joined")),
            _ => {}
        }
        Verdict::Continue
    }

    fn save(&mut self) -> Option<SavedState> {
        let mut w = Writer::new();
        w.u32(self.stats_uses).u32(self.uses_uses);
        Some(SavedState { version: STATE_VERSION, bytes: w.0 })
    }
}

export!(Hello);

/// A plugin is plain Rust against the `Host` trait: its tests run on a PC with the mock server of
/// `pcds_plugin_api::testing`, no server and no WebAssembly (`cargo test`, or `make test` where there is a Makefile).
#[cfg(test)]
mod tests {
    use super::*;
    use pcds_plugin_api::testing::{Harness, MockHost};

    fn world() -> MockHost {
        MockHost::new().with_world(WorldInfo { planet: "Prime".into(), game_ms: 3_725_000, terraformation: 1500.4, ..Default::default() })
    }

    #[test]
    fn stats_tells_the_asker_the_worlds_numbers() {
        let mut plugin = Harness::<Hello>::start(world());
        plugin.join(7, "Ann");
        plugin.command(7, "stats", &[]);
        let told = plugin.mock().told();
        assert_eq!(told[0].0, 7);
        assert!(told[0].1.contains("Prime: 1 players") && told[0].1.contains("1h 02m"), "{}", told[0].1);
    }

    #[test]
    fn the_counters_survive_a_reload_and_the_old_state_version_is_migrated() {
        let mut plugin = Harness::<Hello>::start(world());
        plugin.join(7, "Ann");
        plugin.command(7, "stats", &[]);
        plugin.command(7, "uses", &[]);
        plugin.reload();
        plugin.command(7, "uses", &[]);
        assert_eq!(plugin.mock().told().last().unwrap().1, "!stats was used 1 times, !uses 2 times on this world");
        // Version 1 had one counter, which counted !stats.
        let old = Hello::load(plugin.mock(), Some(State { version: 1, bytes: &[5, 0, 0, 0] }));
        assert_eq!((old.stats_uses, old.uses_uses), (5, 0));
    }
}
