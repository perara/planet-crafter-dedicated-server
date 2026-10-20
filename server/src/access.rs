//! Who may play: IP allowlist (exact addresses or CIDR ranges), a whitelist of names or
//! SteamIDs, banned names/SteamIDs and banned IPs, as the plugin server's Access.cs. SteamIDs
//! only count for players who joined through Steam, where the id is verified. The lists live
//! in a JSON file next to the world and change through the admin API.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::game::Game;

pub const REFUSED: &str = "You are not allowed on this server";

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Lists {
    #[serde(default, rename = "allowedIPs")]
    pub allowed_ips: Vec<String>,
    #[serde(default)]
    pub whitelist: Vec<String>,
    #[serde(default)]
    pub banned_names: Vec<String>,
    #[serde(default, rename = "bannedIPs")]
    pub banned_ips: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Access {
    pub lists: Lists,
    path: Option<String>,
}

/// An address; an IPv4-mapped IPv6 one (a dual-stack socket's IPv4 client) as IPv4.
fn addr(ip: &str) -> Option<IpAddr> {
    ip.parse::<IpAddr>().ok().map(|a| a.to_canonical())
}

/// An IP rule: an address (all its bits) or an address/prefix range, IPv4 or IPv6.
fn rule(rule: &str) -> Option<(IpAddr, u32)> {
    let (network, bits) = match rule.split_once('/') {
        Some((a, b)) => (a, Some(b.parse::<u32>().ok()?)),
        None => (rule, None),
    };
    let network = addr(network)?;
    let max = if network.is_ipv4() { 32 } else { 128 };
    let bits = bits.unwrap_or(max);
    (bits <= max).then_some((network, bits))
}

/// A list entry that is a SteamID64.
fn steam_id(entry: &str) -> Option<u64> {
    entry.parse().ok().filter(|id| *id >= crate::players::FIRST_STEAM_ID)
}

/// Whether an address is in a rule: an exact address or a range (a.b.c.d/n, an IPv6 /n).
pub fn ip_matches(ip: &str, r: &str) -> bool {
    let (Some(ip), Some((network, bits))) = (addr(ip), rule(r)) else { return false };
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(network)) => {
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            u32::from(ip) & mask == u32::from(network) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(network)) => {
            let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
            u128::from(ip) & mask == u128::from(network) & mask
        }
        _ => false,
    }
}

impl Lists {
    /// Every IP entry is an address or a range: one that is not would match nobody (an
    /// allowlist of them refuses everyone, a ban of one bans no one).
    pub fn check(&self) -> Result<(), String> {
        for (list, entries) in [("allowedIPs", &self.allowed_ips), ("bannedIPs", &self.banned_ips)] {
            if let Some(bad) = entries.iter().find(|e| rule(e.trim()).is_none()) {
                return Err(format!("{list}: {bad:?} is not an IP address or range (a.b.c.d, a.b.c.d/n, an IPv6 address or range)"));
            }
        }
        Ok(())
    }
}

/// Characters that show nothing and change no letter: a soft hyphen, zero-width spaces and
/// joiners, direction marks, a byte order mark, variation selectors. A name with one in it must
/// not be another name than without.
fn invisible(c: char) -> bool {
    matches!(c, '\u{ad}' | '\u{34f}' | '\u{61c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
}

/// A name as the access lists compare it: NFKC (fullwidth and circled letters, ligatures and
/// the like become the letters they stand for), the invisible characters dropped, and full
/// case folding (upper case, then lower: ß is ss, a final sigma is sigma, the long s is s).
/// ponytail: no confusables table (a Cyrillic a is still not a Latin a); add one when bans are
/// met by lookalike letters.
pub fn fold_name(name: &str) -> String {
    let normal: String = name.nfkc().filter(|c| !invisible(*c)).collect();
    normal.to_uppercase().nfkc().collect::<String>().to_lowercase()
}

/// Whether two names are the same name to the access lists and to the players online.
pub fn same_name(a: &str, b: &str) -> bool {
    fold_name(a) == fold_name(b)
}

fn clean(values: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in values.into_iter().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

impl Access {
    pub fn load(path: Option<String>) -> Access {
        let lists = path.as_deref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Access { lists, path }
    }

    /// The lists into their file. Err: not kept (they would be lost at the next start); the
    /// lists in memory stay as they were.
    fn save(&mut self, lists: Lists) -> Result<(), String> {
        if let Some(p) = &self.path {
            crate::save::write_atomic(p, serde_json::to_string_pretty(&lists).unwrap_or_default().as_bytes()).map_err(|e| {
                eprintln!("access: could not write {p}: {e}");
                format!("could not write the access lists ({p}: {e}); nothing changed")
            })?;
        }
        self.lists = lists;
        Ok(())
    }

    pub fn set(&mut self, lists: Lists) -> Result<(), String> {
        lists.check()?;
        self.save(Lists { allowed_ips: clean(lists.allowed_ips), whitelist: clean(lists.whitelist), banned_names: clean(lists.banned_names), banned_ips: clean(lists.banned_ips) })
    }

    pub fn ip_allowed(&self, ip: &str) -> bool {
        let l = &self.lists;
        (l.allowed_ips.is_empty() || l.allowed_ips.iter().any(|r| ip_matches(ip, r))) && !l.banned_ips.iter().any(|r| ip_matches(ip, r))
    }

    /// Names compare as fold_name() folds them; SteamIDs only for verified (Steam) players.
    pub fn name_allowed(&self, name: &str, verified_steam_id: Option<u64>) -> bool {
        // A SteamID entry matches only a verified SteamID, never a name that looks like one.
        let listed = |list: &[String]| {
            list.iter().any(|v| match steam_id(v) {
                Some(entry) => verified_steam_id == Some(entry),
                None => same_name(v, name),
            })
        };
        (self.lists.whitelist.is_empty() || listed(&self.lists.whitelist)) && !listed(&self.lists.banned_names)
    }

    /// Whether a verified SteamID is banned (checked as soon as it connects).
    pub fn steam_id_banned(&self, id: u64) -> bool {
        self.lists.banned_names.iter().any(|v| steam_id(v) == Some(id))
    }

    /// Bans a name and, when the player is online, their verified SteamID or their IP.
    pub fn ban(&mut self, name: &str, steam_id: Option<u64>, ip: Option<&str>) -> Result<(), String> {
        let mut names = self.lists.banned_names.clone();
        names.push(name.to_string());
        if let Some(id) = steam_id {
            names.push(id.to_string());
        }
        let mut ips = self.lists.banned_ips.clone();
        if let (None, Some(ip)) = (steam_id, ip) {
            // Behind a UDP relay every player is 127.0.0.1: banning that bans them all.
            if ip.parse().is_ok_and(crate::utp::is_loopback) {
                eprintln!("access: {name} connects from {ip} (a local relay): banned by name only, as that address is every player's");
            } else {
                ips.push(ip.to_string());
            }
        }
        self.save(Lists { banned_names: clean(names), banned_ips: clean(ips), ..self.lists.clone() })
    }

    /// Removes a name, SteamID or IP from the bans; whether there was one to remove (nothing is
    /// written when there was not).
    pub fn unban(&mut self, value: &str) -> Result<bool, String> {
        let mut lists = self.lists.clone();
        lists.banned_names.retain(|n| !same_name(n, value));
        lists.banned_ips.retain(|ip| ip != value);
        let found = lists.banned_names.len() + lists.banned_ips.len() < self.lists.banned_names.len() + self.lists.banned_ips.len();
        if found {
            self.save(lists)?;
        }
        Ok(found)
    }
}

/// Kicks everyone the access lists no longer let in.
pub fn enforce(game: &mut Game) {
    let refused: Vec<u64> = game
        .clients
        .values()
        .filter(|c| {
            let ip_refused = c.verified_steam_id.is_none() && c.ip.as_deref().is_some_and(|ip| !game.access.ip_allowed(ip));
            let name_refused = c.ready.as_ref().is_some_and(|s| !game.access.name_allowed(&s.name, c.verified_steam_id));
            ip_refused || name_refused
        })
        .map(|c| c.id)
        .collect();
    for c in refused {
        game.kick(c, REFUSED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_match_the_plugin() {
        assert!(ip_matches("192.168.1.7", "192.168.1.0/24"));
        assert!(!ip_matches("192.168.2.7", "192.168.1.0/24"));
        assert!(ip_matches("10.0.0.1", "10.0.0.1"));
        // A dual-stack socket's IPv4 client, IPv6 ranges, and every prefix length.
        assert!(ip_matches("::ffff:192.168.1.7", "192.168.1.0/24") && ip_matches("192.168.1.7", "::ffff:192.168.1.7"));
        assert!(ip_matches("2001:db8::1", "2001:db8::/32") && !ip_matches("2001:db9::1", "2001:db8::/32"));
        assert!(ip_matches("8.8.8.8", "0.0.0.0/0") && ip_matches("1.2.3.4", "1.2.3.4/32") && !ip_matches("1.2.3.5", "1.2.3.4/32"));
        for bad in ["192.168.1.0/33", "1.2.3.4/abc", "999.1.1.1/8", "10.0.0.*", "2001:db8::/129"] {
            let lists = Lists { banned_ips: vec![bad.into()], ..Default::default() };
            assert!(lists.check().unwrap_err().contains(bad), "{bad}");
        }
        assert!(serde_json::from_str::<Lists>(r#"{"bannedIps": ["1.2.3.4"]}"#).is_err(), "a misspelt list is refused");
        let mut a = Access { lists: Lists::default(), path: None };
        assert!(a.ip_allowed("1.2.3.4") && a.name_allowed("Anyone", None));
        a.set(Lists { allowed_ips: vec!["10.0.0.0/8".into()], whitelist: vec!["Alice".into(), "76561198000000042".into()], ..Default::default() }).unwrap();
        assert!(a.ip_allowed("10.1.2.3") && !a.ip_allowed("1.2.3.4"));
        assert!(a.name_allowed("alice", None));
        assert!(a.name_allowed("Someone", Some(76561198000000042)));
        assert!(!a.name_allowed("Someone", None));
        a.ban("Alice", None, Some("10.1.2.3")).unwrap();
        assert!(!a.name_allowed("Alice", None) && !a.ip_allowed("10.1.2.3"));
        assert!(a.unban("Alice").unwrap() && a.unban("10.1.2.3").unwrap());
        assert!(!a.unban("Alice").unwrap(), "nothing left to remove");
        assert!(a.name_allowed("Alice", None) && a.ip_allowed("10.1.2.3"));
    }

    /// Names are compared by their letters, not their bytes: case in every script, fullwidth and
    /// other compatibility forms, composed or not composed accents, and characters that show nothing.
    #[test]
    fn names_compare_by_unicode_case_folding_and_normalisation() {
        let mut a = Access { lists: Lists::default(), path: None };
        a.ban("Mallory", None, None).unwrap();
        a.ban("Straße", None, None).unwrap();
        a.ban("Zoë", None, None).unwrap();
        a.ban("ΟΔΥΣΣΕΥΣ", None, None).unwrap();
        a.ban("İstanbul", None, None).unwrap();
        for name in [
            "MALLORY", "mallory", "ＭＡＬＬＯＲＹ", "Ⓜallory", "Mal\u{200b}lory", "Mal\u{ad}lory", "M\u{feff}allory", "\u{202e}Mallory", "mALLORy",
            "STRASSE", "strasse", "Straße", "ſtraſſe",
            "Zoë", "Zoe\u{308}", "ZOË", "zoe\u{308}",
            "οδυσσευς", "Οδυσσευσ",
            "İSTANBUL", "i\u{307}stanbul",
        ] {
            assert!(!a.name_allowed(name, None), "{name:?} is a banned name");
        }
        // Other names stay allowed; a lookalike letter of another script is another letter.
        for name in ["Mallory2", "Mallo", "Zoe", "Strasse1", "Мallory"] {
            assert!(a.name_allowed(name, None), "{name:?}");
        }
        // The whitelist and the unban fold the same way; a SteamID entry still needs a verified id.
        a.set(Lists { whitelist: vec!["Ｂｏｂ".into()], banned_names: vec!["Eve".into()], ..Default::default() }).unwrap();
        assert!(a.name_allowed("BOB", None) && a.name_allowed("bob\u{200b}", None) && !a.name_allowed("Alice", None));
        assert!(a.unban("ＥＶＥ").unwrap() && a.lists.banned_names.is_empty());
        assert!(same_name("Ａｎｎ", "ann") && !same_name("Ann", "Anna") && fold_name("").is_empty());
    }

    /// The lists are kept in their file and read back at the next start; a Steam player's ban
    /// takes their SteamID (not an IP); malformed ranges match nothing.
    #[test]
    fn the_lists_survive_a_restart_and_steam_bans_take_the_steam_id() {
        let dir = std::env::temp_dir().join(format!("pcds-access-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("access.json").to_string_lossy().into_owned();
        let mut a = Access::load(Some(path.clone()));
        assert_eq!(a.lists.banned_names.len(), 0, "no file: empty lists");
        a.ban("Mallory", Some(76561198000000099), Some("10.9.9.9")).unwrap();
        assert!(a.steam_id_banned(76561198000000099) && a.ip_allowed("10.9.9.9"), "the SteamID, not the IP");
        a.set(Lists { banned_names: vec![" Eve ".into(), "Eve".into(), "".into()], ..a.lists.clone() }).unwrap();
        let back = Access::load(Some(path));
        assert_eq!(back.lists.banned_names, vec!["Eve".to_string()], "trimmed, once, kept");
        assert!(!back.name_allowed("eve", None));
        for rule in ["10.0.0.0/33", "10.0.0.0/x", "nonsense/8"] {
            assert!(!ip_matches("10.0.0.1", rule), "{rule}");
        }
        assert!(ip_matches("1.2.3.4", "0.0.0.0/0"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Players behind a UDP relay all come from 127.0.0.1 (README, Known limits): banning one
    /// bans their name, not the address every other player and the admin's own tests share.
    #[test]
    fn a_ban_never_takes_the_loopback_address_everyone_behind_a_relay_has() {
        let mut a = Access { lists: Lists::default(), path: None };
        a.ban("Mallory", None, Some("127.0.0.1")).unwrap();
        a.ban("Eve", None, Some("::ffff:127.0.0.1")).unwrap();
        assert!(a.lists.banned_ips.is_empty());
        assert!(a.ip_allowed("127.0.0.1") && !a.name_allowed("mallory", None) && !a.name_allowed("Eve", None));
    }

    /// A ban that cannot be written is refused, not kept in memory only (and lost at restart).
    #[test]
    fn a_ban_that_cannot_be_kept_is_refused() {
        let mut a = Access::load(Some("/nonexistent-dir-pcds/access.json".into()));
        assert!(a.ban("Mallory", None, None).unwrap_err().contains("nothing changed"));
        assert!(a.name_allowed("Mallory", None));
    }
}

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../fuzz/targets/access.rs"]
pub mod fuzz;
