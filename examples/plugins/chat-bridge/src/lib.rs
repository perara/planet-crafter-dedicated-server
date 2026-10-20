//! chat-bridge, the game's half: chat, joins and leaves, sent to a Discord or Slack webhook.
//! (The other half, Discord to the game, is `../bridge_in.py`: a plugin cannot wait for a
//! network message, only send one.)
//!
//! What it shows beyond `discord-notify`:
//!
//! - **text from strangers**: a player chooses their name and their words, and the webhook is
//!   a public channel. `clean` removes control and direction characters, breaks mentions and
//!   markdown, and cuts the length. Discord gets `allowed_mentions: {"parse": []}` as well, so
//!   even a mention that got through pings nobody;
//! - **settings**: typed `[[config]]` keys, read once in `load` (a changed setting restarts the
//!   plugin, so they cannot change under it). The host fills in the defaults of the manifest;
//!   the defaults below are for a host that does not (and for the tests);
//! - **time without a timer**: `Tick` brings the game time since the last tick, so the plugin
//!   counts it, sends at most one request per `flush_seconds` and waits longer after each
//!   failure (twice as long, up to five minutes, or what a `Retry-After` says);
//! - **a world that stands still**: with `world.pauseWhenEmpty` there are no ticks while nobody
//!   is on. The line "Ann left" would wait for the next visitor, so the leave of the last player
//!   sends at once. (The count of players comes from the events and from every tick.)
//! - **no chat text in the log**: the log gets counts and statuses only.

use pcds_plugin_api::json::Object;
use pcds_plugin_api::*;

/// Lines kept for a webhook that does not answer; the oldest go first.
const KEEP: usize = 200;
/// Characters of a name and of a message that are sent.
const MAX_NAME: usize = 32;
const MAX_TEXT: usize = 300;
/// Bytes of one message (Discord takes 2000 characters).
const MESSAGE: usize = 1800;
/// The longest wait after failures, and how long a request may be out before it counts as lost.
const MAX_WAIT_MS: u32 = 300_000;
const LOST_AFTER_MS: u32 = 60_000;
const STATE_VERSION: u32 = 1;

/// Zero width space: put after a character to break what it would start.
const ZWSP: char = '\u{200b}';

#[derive(Clone, Debug, PartialEq)]
struct Settings {
    slack: bool,
    flush_ms: u32,
    chat: bool,
    joins: bool,
    prefix: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { slack: false, flush_ms: 3000, chat: true, joins: true, prefix: "!".into() }
    }
}

impl Settings {
    fn read(host: &dyn Host) -> Settings {
        let mut s = Settings::default();
        for (key, value) in host.settings().unwrap_or_default() {
            match key.as_str() {
                "style" => s.slack = value == "slack",
                "flush_seconds" => s.flush_ms = value.parse::<u32>().unwrap_or(3).clamp(1, 60) * 1000,
                "send_chat" => s.chat = value != "false",
                "send_joins" => s.joins = value != "false",
                "command_prefix" => s.prefix = value,
                _ => {}
            }
        }
        s
    }
}

/// `text` made safe to show in a public channel: no control, direction or zero width characters,
/// no mention (`@everyone`, `<@123>`, `<!channel>`), no markdown, no backtick, at most `max`
/// characters of the original.
fn clean(text: &str, max: usize, slack: bool) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars().take(max) {
        match c {
            // Hidden or reordering characters.
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}' => {}
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => out.push(' '),
            // Look-alikes: `<@1>`, `<!here>` and `<https://x|y>` need the real brackets.
            '<' => out.push('\u{2039}'),
            '>' => out.push('\u{203a}'),
            '`' => out.push('\''),
            '@' => {
                out.push('@');
                out.push(ZWSP);
            }
            '&' if slack => out.push_str("&amp;"),
            // Discord reads a backslash as an escape; Slack has none, so these become look-alikes.
            '*' | '_' | '~' | '|' | '\\' if !slack => {
                out.push('\\');
                out.push(c);
            }
            '*' => out.push('\u{2217}'),
            '_' => out.push('\u{ff3f}'),
            '~' => out.push('\u{223c}'),
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

#[derive(Default)]
struct Bridge {
    cfg: Settings,
    lines: Vec<String>,
    /// Lines pushed out of a full buffer since the last message that said so.
    lost: u32,
    /// The request in flight: its ticket, how many of the first lines it carries, and how many
    /// lost lines its message reported.
    sending: Option<(u64, usize, u32)>,
    /// Players online, as far as the events tell (every tick sets it right).
    online: u32,
    /// Game time since the last request was started.
    idle_ms: u32,
    /// How long to wait between requests now (more after failures).
    wait_ms: u32,
    failures: u32,
}

impl Bridge {
    fn add(&mut self, line: String) {
        if self.lines.len() >= KEEP {
            self.lines.remove(0);
            self.lost = self.lost.saturating_add(1);
            // The request in flight carried the first lines: they are gone from the front now.
            if let Some((_, n, _)) = self.sending.as_mut() {
                *n = n.saturating_sub(1);
            }
        }
        self.lines.push(line);
    }

    fn name(&self, name: &str) -> String {
        let name = clean(name, MAX_NAME, self.cfg.slack);
        if name.is_empty() { "?".into() } else { name }
    }

    fn bold(&self, text: &str) -> String {
        let mark = if self.cfg.slack { "*" } else { "**" };
        format!("{mark}{text}{mark}")
    }

    /// The text of the next message and how many lines it carries.
    fn message(&self) -> (String, usize) {
        let mut text = String::new();
        if self.lost > 0 {
            text.push_str(&format!("({} earlier lines were lost)", self.lost));
        }
        let mut n = 0;
        for line in &self.lines {
            if n > 0 && text.len() + line.len() + 1 > MESSAGE {
                break;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(line);
            n += 1;
        }
        (text, n)
    }

    /// Starts the request for what is waiting, if none is out. `force`: whatever the wait.
    fn flush(&mut self, host: &dyn Host, force: bool) {
        if self.sending.is_some() || self.lines.is_empty() || (!force && self.idle_ms < self.wait_ms) {
            return;
        }
        let (text, n) = self.message();
        let (url, body) = if self.cfg.slack {
            ("https://hooks.slack.com/services/${config.webhook}", Object::new().str("text", &text).finish())
        } else {
            // No mention in the message pings anyone, whatever got through `clean`.
            ("https://discord.com/api/webhooks/${config.webhook}", Object::new().str("content", &text).raw("allowed_mentions", r#"{"parse":[]}"#).finish())
        };
        let request = HttpRequest { method: "POST", url, headers: vec![("Content-Type", "application/json")], body: body.as_bytes() };
        self.idle_ms = 0;
        match host.http_request(&request) {
            Ok(ticket) => self.sending = Some((ticket, n, self.lost)),
            // Too many requests alive or this second: the next tick tries again.
            Err(Error::Full) => {}
            Err(e) => {
                host.log(Level::Error, &format!("could not send: {e:?} (is the webhook set? pcds plugin config set chat-bridge webhook ID/TOKEN)"));
                self.fail(0);
            }
        }
    }

    /// Waits longer after each failure in a row: twice as long, at most five minutes, and at
    /// least what the server asked for (`retry_after_ms`).
    fn fail(&mut self, retry_after_ms: u32) {
        self.failures = self.failures.saturating_add(1);
        self.wait_ms = (self.cfg.flush_ms << self.failures.min(8)).min(MAX_WAIT_MS).max(retry_after_ms.min(MAX_WAIT_MS));
    }

    fn done(&mut self, host: &dyn Host, status: u32, error: &str, headers: &[(&str, &str)]) {
        let Some((_, n, lost)) = self.sending.take() else { return };
        let n = n.min(self.lines.len());
        if (200..300).contains(&status) {
            self.lines.drain(..n);
            self.lost = self.lost.saturating_sub(lost);
            self.failures = 0;
            self.wait_ms = self.cfg.flush_ms;
            // Nobody is on, so no tick will come: send what came meanwhile now.
            if self.online == 0 {
                self.flush(host, true);
            }
            return;
        }
        let retry = headers.iter().find(|(k, _)| *k == "retry-after").and_then(|(_, v)| v.trim().parse::<f64>().ok()).map_or(0, |s| (s.clamp(0.0, 300.0) * 1000.0) as u32);
        // A 4xx other than "slow down" and "too slow" will not get better by trying again (a
        // deleted webhook, a message the service refuses): the lines go, so one of them cannot block the rest.
        let rejected = (400..500).contains(&status) && status != 429 && status != 408;
        if rejected {
            self.lines.drain(..n);
        }
        self.fail(retry);
        // A status or one of the host's fixed phrases: never the URL, never a line.
        let why = if error.is_empty() { format!("HTTP {status}") } else { error.to_string() };
        host.log(Level::Warn, &format!("not delivered ({why}); {} lines {}, next try in {} s", n, if rejected { "dropped" } else { "wait" }, self.wait_ms / 1000));
    }
}

impl Plugin for Bridge {
    fn load(host: &dyn Host, state: Option<State<'_>>) -> Self {
        let cfg = Settings::read(host);
        let mut me = Bridge { wait_ms: cfg.flush_ms, idle_ms: cfg.flush_ms, cfg, ..Bridge::default() };
        let Some(state) = state else { return me };
        if state.version != STATE_VERSION {
            host.log(Level::Warn, &format!("state version {} is not known: starting empty", state.version));
            return me;
        }
        let mut r = Reader::new(state.bytes);
        me.lost = r.u32().unwrap_or(0);
        me.failures = r.u32().unwrap_or(0);
        let n = r.u32().unwrap_or(0).min(KEEP as u32);
        for _ in 0..n {
            match r.string() {
                Ok(line) => me.lines.push(line),
                Err(_) => break,
            }
        }
        if me.failures > 0 {
            me.failures -= 1;
            me.fail(0);
        }
        me
    }

    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
        match event {
            // `Chat` is what players type. What the server says (the other half of the bridge,
            // `say`) is not an event, so there is no echo to filter.
            Event::Chat { name, text, .. } if self.cfg.chat => {
                let command = !self.cfg.prefix.is_empty() && text.trim_start().starts_with(self.cfg.prefix.as_str());
                let said = clean(text, MAX_TEXT, self.cfg.slack);
                if !command && !said.is_empty() {
                    let line = format!("{}: {said}", self.bold(&self.name(name)));
                    self.add(line);
                }
            }
            Event::PlayerJoin { name, .. } => {
                self.online += 1;
                if self.cfg.joins {
                    let line = format!("{} joined", self.bold(&self.name(name)));
                    self.add(line);
                }
            }
            Event::PlayerLeave { name, .. } => {
                self.online = self.online.saturating_sub(1);
                if self.cfg.joins {
                    let line = format!("{} left", self.bold(&self.name(name)));
                    self.add(line);
                }
                if self.online == 0 {
                    self.flush(host, true);
                }
            }
            Event::Tick { players, since_ms, .. } => {
                self.online = *players;
                self.idle_ms = self.idle_ms.saturating_add(*since_ms);
                // An answer that never came (the host promises one, but a plugin keeps its own deadline).
                if self.sending.is_some() && self.idle_ms > LOST_AFTER_MS {
                    self.sending = None;
                    self.fail(0);
                    host.log(Level::Warn, "a request got no answer; sending again");
                }
                self.flush(host, false);
            }
            Event::HttpDone { ticket, status, error, headers, .. } if self.sending.is_some_and(|(t, _, _)| t == *ticket) => self.done(host, *status, error, headers),
            _ => {}
        }
        Verdict::Continue
    }

    fn save(&mut self) -> Option<SavedState> {
        let mut w = Writer::new();
        w.u32(self.lost).u32(self.failures).u32(self.lines.len() as u32);
        for line in &self.lines {
            w.str(line);
        }
        Some(SavedState { version: STATE_VERSION, bytes: w.0 })
    }
}

export!(Bridge);

#[cfg(test)]
mod tests {
    use super::*;
    use pcds_plugin_api::testing::{Harness, MockHost};

    const DISCORD: &str = "https://discord.com/api/webhooks/${config.webhook}";
    const SLACK: &str = "https://hooks.slack.com/services/${config.webhook}";

    fn start(settings: &[(&str, &str)]) -> Harness<Bridge> {
        let mut mock = MockHost::new();
        for (k, v) in settings {
            mock = mock.with_setting(k, v);
        }
        Harness::start(mock)
    }

    fn body(h: &Harness<Bridge>, i: usize) -> String {
        String::from_utf8(h.mock().http_requests()[i].body.clone()).unwrap()
    }

    fn done(h: &mut Harness<Bridge>, ticket: u64, status: u32, error: &str, headers: Vec<(&str, &str)>) {
        h.event(&Event::HttpDone { ticket, status, error, body: &[], headers });
    }

    #[test]
    fn lines_go_in_one_discord_request_per_interval_and_leave_the_buffer_when_delivered() {
        let mut h = start(&[]);
        h.join(1, "Ann");
        h.chat(1, "hello there");
        h.join(2, "Bo");
        h.leave(2);
        assert!(h.mock().http_requests().is_empty(), "nothing is sent between ticks");
        h.advance_ms(1000);
        let sent = h.mock().http_requests();
        assert_eq!((sent.len(), sent[0].method.as_str(), sent[0].url.as_str()), (1, "POST", DISCORD), "one request; the webhook is a placeholder");
        assert_eq!(sent[0].headers, vec![("Content-Type".to_string(), "application/json".to_string())]);
        assert_eq!(body(&h, 0), r#"{"content":"**Ann** joined\n**Ann**: hello there\n**Bo** joined\n**Bo** left","allowed_mentions":{"parse":[]}}"#);
        // While it is out, nothing else is sent; the answer clears what it carried.
        h.chat(1, "second");
        h.advance_ms(5000);
        assert_eq!(h.mock().http_requests().len(), 1);
        h.http_done(sent[0].ticket, 204, &[]);
        assert_eq!(h.plugin().lines, vec!["**Ann**: second".to_string()]);
        // The next one waits for the interval (3 s) after the first request.
        h.clear();
        h.advance_ms(1000);
        assert_eq!(h.mock().http_requests().len(), 1, "the wait since the first request is over");
        h.http_done(2, 204, &[]);
        assert!(h.plugin().lines.is_empty());
    }

    #[test]
    fn lines_that_come_close_together_are_sent_together() {
        let mut h = start(&[("flush_seconds", "10")]);
        h.join(1, "Ann");
        h.advance_ms(2000);
        h.http_done(1, 204, &[]);
        h.chat(1, "a");
        h.advance_ms(5000);
        h.chat(1, "b");
        h.advance_ms(3000);
        assert_eq!(h.mock().http_requests().len(), 1, "10 s have not passed");
        h.chat(1, "c");
        h.advance_ms(2000);
        assert_eq!(h.mock().http_requests().len(), 2);
        assert!(body(&h, 1).starts_with(r#"{"content":"**Ann**: a\n**Ann**: b\n**Ann**: c""#), "{}", body(&h, 1));
    }

    #[test]
    fn what_players_write_cannot_ping_format_or_hide() {
        let mut h = start(&[]);
        h.join(1, "@everyone <@123>");
        h.chat(1, "hi @everyone <@!5> <@&6> <#7> `code` **bold** __u__ ~~s~~ ||spoiler|| \\ \u{202e}rtl\u{200b}\u{0}\nnext\ttab");
        let line = h.plugin().lines[1].clone();
        assert!(!line.contains("@everyone") && !line.contains("<@") && !line.contains('`'), "{line}");
        assert_eq!(line, "**@\u{200b}everyone \u{2039}@\u{200b}123\u{203a}**: hi @\u{200b}everyone \u{2039}@\u{200b}!5\u{203a} \u{2039}@\u{200b}&6\u{203a} \u{2039}#7\u{203a} 'code' \\*\\*bold\\*\\* \\_\\_u\\_\\_ \\~\\~s\\~\\~ \\|\\|spoiler\\|\\| \\\\ rtl  next tab");
        h.advance_ms(1000);
        // In JSON the backslashes and quotes are escaped once more, by json::Object.
        assert!(body(&h, 0).contains(r#"\\*\\*bold\\*\\*"#) && body(&h, 0).ends_with(r#""allowed_mentions":{"parse":[]}}"#), "{}", body(&h, 0));
    }

    #[test]
    fn names_and_text_are_cut_and_an_empty_message_or_name_is_handled() {
        let mut h = start(&[]);
        h.join(1, &"N".repeat(100));
        h.chat(1, &"x".repeat(1000));
        h.chat(1, "  \u{200b}\n ");
        h.chat(1, "");
        assert_eq!(h.plugin().lines.len(), 2, "empty messages are not sent");
        assert_eq!(h.plugin().lines[0], format!("**{}** joined", "N".repeat(MAX_NAME)));
        assert_eq!(h.plugin().lines[1], format!("**{}**: {}", "N".repeat(MAX_NAME), "x".repeat(MAX_TEXT)));
        // A chat from a client the server has no name for.
        h.event(&Event::Chat { client: 9, name: "", text: "who", player_id: 0 });
        assert_eq!(h.plugin().lines[2], "**?**: who");
    }

    #[test]
    fn slack_gets_text_and_no_allowed_mentions() {
        let mut h = start(&[("style", "slack")]);
        h.join(1, "Ann & *Bo*");
        h.chat(1, "a <!channel> <https://evil|click> & _b_ ~c~");
        h.advance_ms(1000);
        assert_eq!(h.mock().http_requests()[0].url, SLACK);
        assert_eq!(body(&h, 0), "{\"text\":\"*Ann &amp; \u{2217}Bo\u{2217}* joined\\n*Ann &amp; \u{2217}Bo\u{2217}*: a \u{2039}!channel\u{203a} \u{2039}https://evil|click\u{203a} &amp; \u{ff3f}b\u{ff3f} \u{223c}c\u{223c}\"}");
    }

    #[test]
    fn commands_are_not_conversation_and_the_settings_choose_what_is_sent() {
        let mut h = start(&[]);
        h.join(1, "Ann");
        h.chat(1, "!report the door");
        h.chat(1, "  !home");
        h.chat(1, "it is not! a command");
        assert_eq!(h.plugin().lines.len(), 2, "{:?}", h.plugin().lines);
        // Another prefix; a message with `!` is then talk.
        let mut h = start(&[("command_prefix", "/")]);
        h.join(1, "Ann");
        h.chat(1, "/home");
        h.chat(1, "!wow");
        assert_eq!(h.plugin().lines, vec!["**Ann** joined".to_string(), "**Ann**: !wow".to_string()]);
        // No chat: joins only. No joins: chat only.
        let mut h = start(&[("send_chat", "false")]);
        h.join(1, "Ann");
        h.chat(1, "secret talk");
        assert_eq!(h.plugin().lines, vec!["**Ann** joined".to_string()]);
        let mut h = start(&[("send_joins", "false")]);
        h.join(1, "Ann");
        h.chat(1, "hi");
        h.leave(1);
        assert_eq!(h.plugin().lines, vec!["**Ann**: hi".to_string()]);
        // Odd values fall back to the defaults, in bounds.
        assert_eq!(Settings::read(&MockHost::new().with_setting("flush_seconds", "9999")).flush_ms, 60_000);
        assert_eq!(Settings::read(&MockHost::new().with_setting("flush_seconds", "x")).flush_ms, 3000);
    }

    #[test]
    fn a_player_cannot_make_the_bridge_echo_the_other_half() {
        // The inbound half says `[Discord] Name: text` as the server. That is not a Chat event
        // (events come from clients' chat RPCs), so nothing here filters it; a player who types the
        // same text is just a player with a long message.
        let mut h = start(&[]);
        h.join(1, "Ann");
        h.chat(1, "[Discord] Cy: hi");
        assert_eq!(h.plugin().lines[1], "**Ann**: [Discord] Cy: hi");
    }

    #[test]
    fn a_failure_keeps_the_lines_and_the_wait_doubles_up_to_five_minutes() {
        let mut h = start(&[]);
        h.join(1, "Ann");
        h.advance_ms(1000);
        done(&mut h, 1, 500, "", vec![]);
        assert_eq!(h.plugin().lines.len(), 1);
        assert!(h.mock().logs().iter().any(|(_, m)| m.contains("HTTP 500") && m.contains("next try in 6 s")), "{:?}", h.mock().logs());
        // 6 s, then 12 s, 24 s ...
        let mut waits = vec![];
        for ticket in 2..10 {
            let before = h.mock().http_requests().len();
            let mut waited = 0;
            while h.mock().http_requests().len() == before {
                h.advance_ms(1000);
                waited += 1;
                assert!(waited < 400);
            }
            waits.push(waited);
            done(&mut h, ticket, 0, "timed out", vec![]);
        }
        assert_eq!(waits, vec![6, 12, 24, 48, 96, 192, 300, 300]);
        assert_eq!(h.plugin().lines.len(), 1, "nothing was lost");
        // A good answer ends it: the next line goes after the normal wait.
        h.advance_ms(300_000);
        done(&mut h, 10, 204, "", vec![]);
        assert!(h.plugin().lines.is_empty() && h.plugin().failures == 0 && h.plugin().wait_ms == 3000);
    }

    #[test]
    fn a_retry_after_is_obeyed_a_refusal_drops_the_lines_and_no_chat_text_is_logged() {
        let mut h = start(&[]);
        h.join(1, "Ann");
        h.chat(1, "my password is hunter2");
        h.advance_ms(1000);
        done(&mut h, 1, 429, "", vec![("retry-after", "42.5")]);
        assert_eq!(h.plugin().lines.len(), 2, "429: kept");
        assert_eq!(h.plugin().wait_ms, 42_500);
        h.advance_ms(42_000);
        assert_eq!(h.mock().http_requests().len(), 1);
        h.advance_ms(1000);
        assert_eq!(h.mock().http_requests().len(), 2);
        done(&mut h, 2, 404, "", vec![]);
        assert!(h.plugin().lines.is_empty(), "404: the webhook is gone, the lines with it");
        let logs = h.mock().logs();
        assert!(logs.iter().any(|(_, m)| m.contains("dropped")));
        assert!(logs.iter().all(|(_, m)| !m.contains("hunter2") && !m.contains("Ann") && !m.contains("discord")), "{logs:?}");
    }

    #[test]
    fn a_webhook_that_cannot_be_used_is_one_log_line_and_then_quiet() {
        // The real host answers -2 ("Bad") to a `${config.webhook}` that is not set; the mock cannot
        // tell, so a denied capability stands in: any error that is not "full" takes the same path.
        let mut h: Harness<Bridge> = Harness::start(MockHost::new().deny(&["net.http"]));
        h.join(1, "Ann");
        h.advance_ms(1000);
        assert!(h.mock().logs().iter().any(|(l, m)| *l == Level::Error && m.contains("could not send")));
        let n = h.mock().logs().len();
        h.advance_ms(5000);
        assert_eq!(h.mock().logs().len(), n, "the wait grew: no second line within 6 s");
        assert_eq!(h.plugin().lines.len(), 1);
    }

    #[test]
    fn the_buffer_is_bounded_says_what_it_lost_and_a_message_fits_the_limit() {
        let mut h = start(&[]);
        h.join(1, "Ann");
        h.advance_ms(1000);
        let ticket = h.mock().http_requests()[0].ticket;
        for i in 0..KEEP + 20 {
            h.chat(1, &format!("line {i}"));
        }
        assert_eq!((h.plugin().lines.len(), h.plugin().lost), (KEEP, 21), "the join line and 20 more went");
        h.http_done(ticket, 204, &[]);
        assert_eq!(h.plugin().lines.len(), KEEP, "the answer must not drop lines that were not in the message");
        h.advance_ms(3000);
        let text = body(&h, 1);
        assert!(text.starts_with(r#"{"content":"(21 earlier lines were lost)\n**Ann**: line 20"#), "{text}");
        assert!(text.len() < MESSAGE + 250, "{} bytes: the message is at most {MESSAGE}, the rest is JSON (\\n, quotes)", text.len());
        let n = h.plugin().sending.unwrap().1;
        assert!((20..KEEP).contains(&n), "{n} lines in the first message");
        h.http_done(2, 204, &[]);
        assert_eq!((h.plugin().lines.len(), h.plugin().lost), (KEEP - n, 0), "the rest waits for the next interval");
        // Long lines, worst case, still fit in one message each.
        let mut h = start(&[]);
        h.join(1, &"@".repeat(MAX_NAME));
        h.chat(1, &"@".repeat(MAX_TEXT));
        let (text, n) = h.plugin().message();
        assert!(n == 2 && text.len() <= MESSAGE && text.chars().count() < 2000, "{} bytes, {} characters", text.len(), text.chars().count());
        h.chat(1, &"@".repeat(MAX_TEXT));
        let (text, n) = h.plugin().message();
        assert!(n == 2 && text.len() <= MESSAGE, "the third line waits for the next message");
        h.advance_ms(1000);
        assert_eq!(h.mock().http_requests().len(), 1);
    }

    #[test]
    fn the_last_player_leaving_sends_at_once_because_a_world_that_stands_still_has_no_ticks() {
        let mut h = start(&[]);
        h.join(1, "Ann");
        h.advance_ms(1000);
        h.http_done(1, 204, &[]);
        h.chat(1, "bye");
        h.leave(1);
        assert_eq!(h.mock().http_requests().len(), 2, "sent in the leave's own call");
        assert!(body(&h, 1).contains("**Ann**: bye\\n**Ann** left"));
        // Someone joins and leaves while that request is out: it goes when the answer comes, since no tick will.
        h.join(2, "Bo");
        h.leave(2);
        assert_eq!(h.mock().http_requests().len(), 2);
        h.http_done(2, 204, &[]);
        assert_eq!(h.mock().http_requests().len(), 3);
        assert!(body(&h, 2).contains("**Bo** joined\\n**Bo** left"), "{}", body(&h, 2));
        h.http_done(3, 204, &[]);
        assert!(h.plugin().lines.is_empty());
    }

    #[test]
    fn a_request_with_no_answer_is_given_up_after_a_minute_and_sent_again() {
        let mut h = start(&[]);
        h.join(1, "Ann");
        h.advance_ms(1000);
        assert_eq!(h.mock().http_requests().len(), 1);
        h.advance_ms(61_000);
        assert!(h.mock().logs().iter().any(|(_, m)| m.contains("no answer")));
        h.advance_ms(60_000);
        assert!(h.mock().http_requests().len() >= 2, "sent again");
        // An answer for a ticket it does not wait for changes nothing.
        let lines = h.plugin().lines.len();
        h.http_done(99, 204, &[]);
        assert_eq!(h.plugin().lines.len(), lines);
    }

    #[test]
    fn the_lines_and_the_failure_survive_a_reload_and_a_strange_state_starts_empty() {
        let mut h = start(&[]);
        h.join(1, "Ann");
        h.chat(1, "hello");
        h.advance_ms(1000);
        done(&mut h, 1, 500, "", vec![]);
        let before = h.plugin().wait_ms;
        h.reload();
        assert_eq!(h.plugin().lines.len(), 2);
        assert_eq!(h.plugin().wait_ms, before, "the wait after failures is kept too");
        h.advance_ms(before as u64);
        assert_eq!(h.mock().http_requests().len(), 2, "sent again after the wait");
        let host = MockHost::new();
        assert!(Bridge::load(&host, Some(State { version: 9, bytes: &[1, 0, 0, 0] })).lines.is_empty());
        assert!(Bridge::load(&host, Some(State { version: 1, bytes: &[1] })).lines.is_empty());
    }
}
