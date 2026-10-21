//! The access lists: their JSON, the IP rules and the name checks.
use super::*;

pub fn run(data: &[u8]) {
    let text = String::from_utf8_lossy(data).into_owned();
    let (rule_text, ip) = text.split_once('\n').unwrap_or((&text, "127.0.0.1"));
    let _ = ip_matches(ip, rule_text);
    if let Ok(lists) = serde_json::from_slice::<Lists>(data) {
        let _ = lists.check();
        let mut access = Access { lists, path: None };
        let _ = access.ip_allowed(ip);
        let _ = access.name_allowed(rule_text, Some(76561198000000001));
        let _ = access.name_allowed(rule_text, None);
        let _ = access.steam_id_banned(76561198000000001);
        let lists = Lists { allowed_ips: access.lists.allowed_ips.clone(), whitelist: access.lists.whitelist.clone(), banned_names: access.lists.banned_names.clone(), banned_ips: access.lists.banned_ips.clone(), operators: access.lists.operators.clone() };
        let _ = access.set(lists);
        let _ = access.ban(rule_text, Some(76561198000000002), Some(ip));
        let _ = access.unban(rule_text);
        let _ = access.unban(ip);
    }
}
