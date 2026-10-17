//! Who may play: IP allowlist (exact addresses or CIDR ranges), a whitelist of names or
//! SteamIDs, banned names/SteamIDs and banned IPs, as the plugin server's Access.cs. SteamIDs
//! only count for players who joined through Steam, where the id is verified. The lists live
//! in a JSON file next to the world and change through the admin API.

use serde::{Deserialize, Serialize};

pub const REFUSED: &str = "You are not allowed on this server";

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

pub struct Access {
    pub lists: Lists,
    path: Option<String>,
}

fn ipv4(ip: &str) -> Option<u32> {
    let addr: std::net::Ipv4Addr = ip.parse().ok()?;
    Some(u32::from(addr))
}

/// A list entry that is a SteamID64.
fn steam_id(entry: &str) -> Option<u64> {
    entry.parse().ok().filter(|id| *id >= crate::players::FIRST_STEAM_ID)
}

/// Exact IPv4 address or a.b.c.d/n range.
pub fn ip_matches(ip: &str, rule: &str) -> bool {
    let Some((network, bits)) = rule.split_once('/') else { return ip == rule };
    let (Some(ip), Some(network), Ok(bits)) = (ipv4(ip), ipv4(network), bits.parse::<u32>()) else { return false };
    if bits > 32 {
        return false;
    }
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    ip & mask == network & mask
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

    pub fn save(&self) {
        if let Some(p) = &self.path {
            let tmp = format!("{p}.tmp");
            if std::fs::write(&tmp, serde_json::to_string_pretty(&self.lists).unwrap_or_default()).and_then(|_| std::fs::rename(&tmp, p)).is_err() {
                eprintln!("access: could not write {p}");
            }
        }
    }

    pub fn set(&mut self, lists: Lists) {
        self.lists = Lists { allowed_ips: clean(lists.allowed_ips), whitelist: clean(lists.whitelist), banned_names: clean(lists.banned_names), banned_ips: clean(lists.banned_ips) };
        self.save();
    }

    pub fn ip_allowed(&self, ip: &str) -> bool {
        let l = &self.lists;
        (l.allowed_ips.is_empty() || l.allowed_ips.iter().any(|r| ip_matches(ip, r))) && !l.banned_ips.iter().any(|r| ip_matches(ip, r))
    }

    /// Names compare without case; SteamIDs only for verified (Steam) players.
    pub fn name_allowed(&self, name: &str, verified_steam_id: Option<u64>) -> bool {
        // A SteamID entry matches only a verified SteamID, never a name that looks like one.
        let listed = |list: &[String]| {
            list.iter().any(|v| match steam_id(v) {
                Some(entry) => verified_steam_id == Some(entry),
                None => v.eq_ignore_ascii_case(name),
            })
        };
        (self.lists.whitelist.is_empty() || listed(&self.lists.whitelist)) && !listed(&self.lists.banned_names)
    }

    /// Whether a verified SteamID is banned (checked as soon as it connects).
    pub fn steam_id_banned(&self, id: u64) -> bool {
        self.lists.banned_names.iter().any(|v| steam_id(v) == Some(id))
    }

    /// Bans a name and, when the player is online, their verified SteamID or their IP.
    pub fn ban(&mut self, name: &str, steam_id: Option<u64>, ip: Option<&str>) {
        let mut names = self.lists.banned_names.clone();
        names.push(name.to_string());
        if let Some(id) = steam_id {
            names.push(id.to_string());
        }
        let mut ips = self.lists.banned_ips.clone();
        if let (None, Some(ip)) = (steam_id, ip) {
            ips.push(ip.to_string());
        }
        self.lists.banned_names = clean(names);
        self.lists.banned_ips = clean(ips);
        self.save();
    }

    pub fn unban(&mut self, value: &str) {
        self.lists.banned_names.retain(|n| !n.eq_ignore_ascii_case(value));
        self.lists.banned_ips.retain(|ip| ip != value);
        self.save();
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
        let mut a = Access { lists: Lists::default(), path: None };
        assert!(a.ip_allowed("1.2.3.4") && a.name_allowed("Anyone", None));
        a.set(Lists { allowed_ips: vec!["10.0.0.0/8".into()], whitelist: vec!["Alice".into(), "76561198000000042".into()], ..Default::default() });
        assert!(a.ip_allowed("10.1.2.3") && !a.ip_allowed("1.2.3.4"));
        assert!(a.name_allowed("alice", None));
        assert!(a.name_allowed("Someone", Some(76561198000000042)));
        assert!(!a.name_allowed("Someone", None));
        a.ban("Alice", None, Some("10.1.2.3"));
        assert!(!a.name_allowed("Alice", None) && !a.ip_allowed("10.1.2.3"));
        a.unban("Alice");
        a.unban("10.1.2.3");
        assert!(a.name_allowed("Alice", None) && a.ip_allowed("10.1.2.3"));
    }
}
