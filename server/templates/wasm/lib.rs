//! {id}: a plugin for the Planet Crafter dedicated server (docs/plugins-guide.md).

use pcds_plugin_api::*;

struct MyPlugin {
    greeted: u32,
}

impl Plugin for MyPlugin {
    /// Once at the start, after a reload and after a hot update. `state` is what `save` returned last time.
    fn load(_host: &dyn Host, state: Option<State<'_>>) -> Self {
        let greeted = state.and_then(|s| Reader::new(s.bytes).u32().ok()).unwrap_or(0);
        MyPlugin { greeted }
    }

    /// One event the manifest subscribed to (here: the command !hello). Keep it short: it has a fuel budget.
    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
        if let Event::Command { client, name, command: "hello", .. } = event {
            self.greeted += 1;
            let greeting = host.config("greeting").unwrap_or_else(|| "Hello".to_string());
            let _ = host.tell(*client, &format!("{greeting} {name}! I have said hello {} times.", self.greeted));
        }
        Verdict::Continue
    }

    /// What to keep across restarts and hot updates (asked at every world save and before a hot update).
    fn save(&mut self) -> Option<SavedState> {
        let mut w = Writer::new();
        w.u32(self.greeted);
        Some(SavedState { version: 1, bytes: w.0 })
    }

    // Optional: the last call before the plugin is switched off or removed (not at a reload; `save`
    // is asked right after it).
    // fn unload(&mut self) {}
}

export!(MyPlugin);

/// Your plugin is plain Rust: its tests run here, with no server and no WebAssembly (`cargo test`).
#[cfg(test)]
mod tests {
    use super::*;
    use pcds_plugin_api::testing::{Harness, MockHost};

    #[test]
    fn hello_counts_and_remembers_across_a_reload() {
        let mut plugin = Harness::<MyPlugin>::start(MockHost::new().with_setting("greeting", "Hi"));
        plugin.join(1, "Ann");
        plugin.command(1, "hello", &[]);
        assert_eq!(plugin.mock().told(), vec![(1, "Hi Ann! I have said hello 1 times.".to_string())]);
        plugin.reload();
        plugin.command(1, "hello", &[]);
        assert_eq!(plugin.mock().told().last().unwrap().1, "Hi Ann! I have said hello 2 times.");
    }
}
