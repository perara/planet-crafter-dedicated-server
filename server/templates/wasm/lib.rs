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
            let _ = host.tell(*client, &format!("Hello {name}! I have said hello {} times.", self.greeted));
        }
        Verdict::Continue
    }

    /// What to keep across restarts and hot updates (asked at every world save and before a hot update).
    fn save(&mut self) -> Option<SavedState> {
        let mut w = Writer::new();
        w.u32(self.greeted);
        Some(SavedState { version: 1, bytes: w.0 })
    }
}

export!(MyPlugin);
