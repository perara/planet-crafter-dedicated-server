//! discord-notify: joins, leaves and `!report` messages, posted to a Discord webhook.
//!
//! It shows what `net.http` asks of a plugin:
//!
//! - **a request is not a call**: `http_request` returns a ticket at once and the answer comes as
//!   the event `HttpDone` in a later call, so the plugin keeps what it is waiting for (`sending`);
//! - **batching**: lines are collected as they happen and posted together on the next tick (every
//!   five seconds), one request instead of one for each line (the host allows two alive at once and ten
//!   a second, and Discord has a rate limit of its own);
//! - **failure is normal**: a request that did not get a 2xx answer leaves its lines in the buffer for
//!   the next tick, up to `KEEP` lines, and the buffer is part of the saved state, so a restart or a hot
//!   update loses nothing it had not sent;
//! - **the webhook is not here**: the URL ends in `${config.webhook}`, which the host fills in from
//!   what the admin set (`pcds plugin config set discord-notify webhook ID/TOKEN`).

use pcds_plugin_api::*;

/// Where the requests go (also the manifest's `[net] allow`), up to the admin's part.
const URL: &str = "https://discord.com/api/webhooks/${config.webhook}";
/// Discord takes 2000 characters in a message.
const MESSAGE: usize = 1900;
/// Lines kept for a Discord that does not answer; the oldest go first.
const KEEP: usize = 100;
const STATE_VERSION: u32 = 1;

#[derive(Default)]
struct Notify {
    lines: Vec<String>,
    /// The ticket of the request in flight and how many of the first lines it carries.
    sending: Option<(u64, usize)>,
}

/// `text` as the inside of a JSON string.
fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

impl Notify {
    fn add(&mut self, line: String) {
        if self.lines.len() >= KEEP {
            self.lines.remove(0);
            // The request in flight carried the first lines: they are gone from the front now.
            if let Some((_, n)) = self.sending.as_mut() {
                *n = n.saturating_sub(1);
            }
        }
        self.lines.push(line);
    }

    /// Posts what is waiting, if no request is out: as many lines as fit in one message.
    fn flush(&mut self, host: &dyn Host) {
        if self.sending.is_some() || self.lines.is_empty() {
            return;
        }
        let (mut text, mut n) = (String::new(), 0);
        for line in &self.lines {
            let escaped = json_escape(line);
            if n > 0 && text.len() + escaped.len() + 2 > MESSAGE {
                break;
            }
            if n > 0 {
                text.push_str("\\n");
            }
            text.push_str(&escaped.chars().take(MESSAGE).collect::<String>());
            n += 1;
        }
        let body = format!("{{\"content\":\"{text}\"}}");
        let request = HttpRequest { method: "POST", url: URL, headers: vec![("Content-Type", "application/json")], body: body.as_bytes() };
        match host.http_request(&request) {
            Ok(ticket) => self.sending = Some((ticket, n)),
            // Full: too many requests alive or this second. Not allowed or bad: the manifest or the code is wrong.
            Err(Error::Full) => {}
            Err(e) => host.log(Level::Error, &format!("could not send: {e:?} (is the webhook set? pcds plugin config set discord-notify webhook ID/TOKEN)")),
        }
    }
}

impl Plugin for Notify {
    fn load(host: &dyn Host, state: Option<State<'_>>) -> Self {
        let mut me = Notify::default();
        let Some(state) = state else { return me };
        if state.version != STATE_VERSION {
            host.log(Level::Warn, &format!("state version {} is not known: starting empty", state.version));
            return me;
        }
        let mut r = Reader::new(state.bytes);
        let n = r.u32().unwrap_or(0).min(KEEP as u32);
        for _ in 0..n {
            match r.string() {
                Ok(line) => me.lines.push(line),
                Err(_) => break,
            }
        }
        me
    }

    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
        match event {
            Event::PlayerJoin { name, .. } => self.add(format!("**{name}** joined the game")),
            Event::PlayerLeave { name, .. } => self.add(format!("**{name}** left the game")),
            Event::Command { client, name, command: "report", text, .. } => {
                let said = text.split_once(' ').map_or("", |(_, rest)| rest.trim());
                if said.is_empty() {
                    let _ = host.tell(*client, "Say what to report: !report the door is stuck");
                } else {
                    self.add(format!("Report from **{name}**: {said}"));
                    let _ = host.tell(*client, "Thanks, the admins have it.");
                }
            }
            Event::Tick { .. } => self.flush(host),
            Event::HttpDone { ticket, status, error, .. } => {
                if let Some((t, n)) = self.sending
                    && t == *ticket
                {
                    self.sending = None;
                    if (200..300).contains(status) {
                        self.lines.drain(..n.min(self.lines.len()));
                    } else {
                        // Kept for the next tick. The reason is a status or a fixed phrase: no URL, nothing secret.
                        host.log(Level::Warn, &format!("not delivered ({}); {} lines wait", if error.is_empty() { format!("HTTP {status}") } else { error.to_string() }, self.lines.len()));
                    }
                }
            }
            _ => {}
        }
        Verdict::Continue
    }

    fn save(&mut self) -> Option<SavedState> {
        let mut w = Writer::new();
        w.u32(self.lines.len() as u32);
        for line in &self.lines {
            w.str(line);
        }
        Some(SavedState { version: STATE_VERSION, bytes: w.0 })
    }
}

export!(Notify);

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A host that remembers the requests and answers `reply` to the next one.
    struct Mock {
        requests: RefCell<Vec<(String, String, Vec<u8>)>>,
        reply: RefCell<Result<u64, Error>>,
        told: RefCell<Vec<String>>,
        logged: RefCell<Vec<String>>,
    }

    impl Mock {
        fn new() -> Mock {
            Mock { requests: Default::default(), reply: RefCell::new(Ok(7)), told: Default::default(), logged: Default::default() }
        }
    }

    impl Host for Mock {
        fn log(&self, _: Level, m: &str) {
            self.logged.borrow_mut().push(m.to_string());
        }
        fn world(&self) -> Result<WorldInfo, Error> {
            Err(Error::Denied)
        }
        fn players(&self) -> Result<Vec<PlayerInfo>, Error> {
            Err(Error::Denied)
        }
        fn rules(&self) -> Result<String, Error> {
            Err(Error::Denied)
        }
        fn say(&self, _: &str) -> Result<(), Error> {
            Ok(())
        }
        fn tell(&self, _: u64, text: &str) -> Result<(), Error> {
            self.told.borrow_mut().push(text.to_string());
            Ok(())
        }
        fn kick(&self, _: u64, _: &str) -> Result<(), Error> {
            Err(Error::Denied)
        }
        fn teleport(&self, _: u64, _: [f32; 3]) -> Result<(), Error> {
            Err(Error::Denied)
        }
        fn rules_patch(&self, _: &str) -> Result<(), Error> {
            Err(Error::Denied)
        }
        fn http_request(&self, r: &HttpRequest<'_>) -> Result<u64, Error> {
            let reply = *self.reply.borrow();
            if reply.is_ok() {
                self.requests.borrow_mut().push((r.method.to_string(), r.url.to_string(), r.body.to_vec()));
            }
            reply
        }
    }

    fn tick() -> Event<'static> {
        Event::Tick { game_ms: 5000, tick: 1, players: 1, since_ms: 5000, unix_ms: 0 }
    }

    fn done(ticket: u64, status: u32, error: &'static str) -> Event<'static> {
        Event::HttpDone { ticket, status, error, body: &[], headers: vec![] }
    }

    #[test]
    fn lines_are_batched_into_one_request_and_dropped_when_delivered() {
        let host = Mock::new();
        let mut p = Notify::load(&host, None);
        p.event(&host, &Event::PlayerJoin { client: 1, name: "Ann \"the\" Bo", steam_id: "", player_id: 1 });
        p.event(&host, &Event::PlayerLeave { client: 1, name: "Ann", player_id: 1 });
        p.event(&host, &Event::Command { client: 1, name: "Ann", command: "report", args: vec!["door"], text: "!report the door\nis stuck", player_id: 1 });
        assert_eq!(host.told.borrow()[0], "Thanks, the admins have it.");
        assert!(host.requests.borrow().is_empty(), "nothing is sent until the tick");
        p.event(&host, &tick());
        let sent = host.requests.borrow();
        assert_eq!((sent.len(), sent[0].0.as_str(), sent[0].1.as_str()), (1, "POST", URL), "one request, and the webhook is a placeholder");
        let body = String::from_utf8(sent[0].2.clone()).unwrap();
        assert_eq!(body, "{\"content\":\"**Ann \\\"the\\\" Bo** joined the game\\n**Ann** left the game\\nReport from **Ann**: the door\\nis stuck\"}");
        drop(sent);
        // While the request is out, another tick sends nothing; the answer drops the lines it carried.
        p.event(&host, &tick());
        assert_eq!(host.requests.borrow().len(), 1);
        p.event(&host, &Event::PlayerJoin { client: 2, name: "Bo", steam_id: "", player_id: 1 });
        p.event(&host, &done(7, 204, ""));
        assert_eq!(p.lines, vec!["**Bo** joined the game".to_string()]);
        // An answer for another ticket changes nothing.
        p.event(&host, &done(99, 204, ""));
        assert_eq!(p.lines.len(), 1);
    }

    #[test]
    fn a_request_that_failed_is_tried_again_at_the_next_tick_and_a_restart_loses_nothing() {
        let host = Mock::new();
        let mut p = Notify::load(&host, None);
        p.event(&host, &Event::PlayerJoin { client: 1, name: "Ann", steam_id: "", player_id: 1 });
        p.event(&host, &tick());
        p.event(&host, &done(7, 500, ""));
        assert_eq!(p.lines.len(), 1);
        assert!(host.logged.borrow()[0].contains("HTTP 500"), "{:?}", host.logged.borrow());
        // A hot update between: the saved state carries the line.
        let saved = p.save().unwrap();
        let mut again = Notify::load(&host, Some(State { version: saved.version, bytes: &saved.bytes }));
        assert_eq!(again.lines.len(), 1);
        again.event(&host, &tick());
        assert_eq!(host.requests.borrow().len(), 2, "sent again");
        again.event(&host, &done(7, 0, "timed out"));
        assert!(host.logged.borrow()[1].contains("timed out"));
        assert_eq!(again.lines.len(), 1);
        // "Full" is not an error to log; a refusal is.
        *host.reply.borrow_mut() = Err(Error::Full);
        again.event(&host, &tick());
        assert!(again.sending.is_none() && host.logged.borrow().len() == 2);
        *host.reply.borrow_mut() = Err(Error::NotAllowed);
        again.event(&host, &tick());
        assert!(host.logged.borrow()[2].contains("could not send"));
        // A state of a version this does not know starts empty.
        assert!(Notify::load(&host, Some(State { version: 9, bytes: &[1, 0, 0, 0] })).lines.is_empty());
    }

    #[test]
    fn the_buffer_is_bounded_and_a_message_fits_discords_limit() {
        let host = Mock::new();
        let mut p = Notify::default();
        for i in 0..KEEP + 20 {
            p.add(format!("line {i}"));
        }
        assert_eq!((p.lines.len(), p.lines[0].as_str()), (KEEP, "line 20"));
        let mut q = Notify::default();
        for _ in 0..50 {
            q.add("x".repeat(100));
        }
        q.event(&host, &tick());
        let body = String::from_utf8(host.requests.borrow()[0].2.clone()).unwrap();
        assert!(body.len() < MESSAGE + 40, "{} bytes", body.len());
        let (_, n) = q.sending.unwrap();
        assert!(n < 50 && n >= 15, "{n} lines in the first message");
        q.event(&host, &done(7, 204, ""));
        assert_eq!(q.lines.len(), 50 - n, "the rest waits for the next tick");
        // `!report` with nothing to report says how.
        q.event(&host, &Event::Command { client: 3, name: "Cy", command: "report", args: vec![], text: "!report", player_id: 1 });
        assert!(host.told.borrow()[0].starts_with("Say what to report"));
    }
}
