//! Private, built-in operator commands; ordinary chat and plugin commands keep their path.
use serde_json::{Value, json};

use crate::game::Game;
use crate::net::Target;

/// True when this is the reserved admin command, including a refused request.
pub(super) fn handle(game: &mut Game, client: u64, text: &str) -> bool {
    let prefix = game.rules.chat.command_prefix.as_deref().filter(|p| !p.is_empty()).unwrap_or("!");
    let Some((name, args)) = crate::chathooks::command(Some(prefix), text) else { return false };
    if !name.eq_ignore_ascii_case("admin") { return false; }
    let prefix = prefix.to_string();
    let identity = game.clients.get(&client).filter(|c| c.ready.is_some()).and_then(|c| c.verified_steam_id);
    let reply = if !game.access.is_operator(identity) {
        "Only server admins with a verified Steam identity may use this command.".to_string()
    } else {
        match execute(game, &args, &prefix) {
            Ok(message) => {
                info!("admin {} (client {client}): {}", identity.unwrap_or_default(), args.first().map_or("help", String::as_str));
                message
            }
            Err(error) => format!("Admin command refused: {error}"),
        }
    };
    game.net.rpc_scene(Target::Only(client), "ChatHandler.ReceiveChatMessageClientRpc", &[json!(0), json!(reply)]);
    true
}

fn target(game: &Game, name: &str) -> Result<u64, String> {
    let found = if let Some(id) = name.strip_prefix('#') {
        let id = id.parse::<u64>().map_err(|_| "use # followed by a client id".to_string())?;
        game.clients.contains_key(&id).then_some(id)
    } else {
        crate::access::named_client(game, name)?
    };
    found.ok_or_else(|| format!("player {name} is not online"))
}

fn execute(game: &mut Game, args: &[String], prefix: &str) -> Result<String, String> {
    let command = args.first().map_or("help".into(), |s| s.to_ascii_lowercase());
    let rest = &args[usize::from(!args.is_empty())..];
    match command.as_str() {
        "help" if rest.is_empty() => Ok(format!("Admin commands:\n{prefix}admin rule SECTION.RULE=VALUE\n{prefix}admin save\n{prefix}admin kick PLAYER\n{prefix}admin ban PLAYER\n{prefix}admin unban NAME_OR_STEAMID_OR_IP\n{prefix}admin tp PLAYER X Y Z\n{prefix}admin repair-keycards [INVENTORY_ID]\nUse a player's name or #client-id. Admins are managed in the dashboard.")),
        "repair-keycards" if rest.len() <= 1 => {
            let source = rest.first().map(|s| s.parse::<i32>().map_err(|_| "INVENTORY_ID must be an integer".to_string())).transpose()?;
            game.repair_selenea_keycard_door(source)
        }
        "rule" => {
            let assignment = rest.join(" ");
            let (key, value) = assignment.split_once('=').ok_or_else(|| "use rule SECTION.RULE=VALUE".to_string())?;
            let key = key.trim();
            // Chat administrators control the game, not host programs, secrets or admin membership.
            let allowed = ["meteo", "gauges", "speed", "death", "loot", "limits", "world", "drones", "rockets", "save", "update"];
            let section = key.split_once('.').map(|(s, _)| s);
            if !section.is_some_and(|s| allowed.contains(&s)) || key == "world.dlcPlanets" || key.starts_with("world.dlcPlanets.") {
                return Err("that setting is managed only through the dashboard or server console".into());
            }
            let value = value.trim();
            if value.is_empty() { return Err("a rule value is required".into()); }
            let value: Value = serde_json::from_str(value).unwrap_or_else(|_| json!(value));
            crate::config::set(game, &json!({key: value})).map_err(|e| e["error"].as_str().unwrap_or("the rule change failed").to_string())?;
            Ok(format!("Set {key}."))
        }
        "save" if rest.is_empty() => {
            game.save_requested = true;
            Ok("Save requested. The dashboard shows the save result.".into())
        }
        "kick" if !rest.is_empty() => {
            let id = target(game, &rest.join(" "))?;
            game.kick(id, "Kicked by a server admin");
            Ok("Player kicked.".into())
        }
        "ban" if !rest.is_empty() => {
            let name = rest.join(" ");
            let found = if name.starts_with('#') { Some(target(game, &name)?) } else { crate::access::named_client(game, &name)? };
            if let Some(id) = found {
                let c = &game.clients[&id];
                let name = c.ready.as_ref().ok_or("that player is still joining; use kick instead")?.name.clone();
                let (steam, ip) = (c.verified_steam_id, c.ip.clone());
                game.access.ban(&name, steam, ip.as_deref())?;
                game.kick(id, "Banned by a server admin");
            } else {
                game.access.ban(&name, None, None)?;
            }
            Ok("Player banned.".into())
        }
        "unban" if !rest.is_empty() => {
            if !game.access.unban(&rest.join(" "))? { return Err("that entry is not banned".into()); }
            Ok("Ban entry removed.".into())
        }
        "tp" if rest.len() >= 4 => {
            let at = rest.len() - 3;
            let id = target(game, &rest[..at].join(" "))?;
            let mut position = [0.0; 3];
            for (coordinate, value) in position.iter_mut().zip(&rest[at..]) {
                *coordinate = value.parse::<f32>().ok().filter(|v| v.is_finite()).ok_or("X, Y and Z must be finite numbers")?;
            }
            if !crate::status::teleport(game, id, position) { return Err("the player cannot be teleported yet".into()); }
            Ok("Player teleported.".into())
        }
        _ => Err(format!("unknown command or arguments; use {prefix}admin help")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{self, tests::join};
    use crate::testkit::sent;
    const ADMIN: u64 = 76561198000000001;

    fn world() -> Option<Game> {
        let mut g = game::test_game(game::tests::save())?;
        join(&mut g, 1, "Ann");
        join(&mut g, 2, "Bo Smith");
        g.clients.get_mut(&1).unwrap().verified_steam_id = Some(ADMIN);
        g.access.lists.operators.push(ADMIN.to_string());
        g.net.out.clear();
        Some(g)
    }

    fn say(g: &mut Game, text: &str) -> String {
        g.net.out.clear();
        assert!(handle(g, 1, text));
        let messages = sent(g, "ChatHandler.ReceiveChatMessageClientRpc");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].to, Target::Only(1), "admin feedback is private");
        messages[0].args[1].as_str().unwrap().to_string()
    }

    #[test]
    fn keycard_door_repair_survives_a_hot_restore_without_a_reconnect() {
        let Some(mut g) = world() else { return };
        g.planet = g.net_data.planet_index("Selenea").unwrap();
        let card = g.data.group("KeyCard2").unwrap();
        let old = g.create_inventory(3, 0, 0, &[card; 3], &[]);
        let cached_empty = g.create_inventory(3, 0, 0, &[], &[]);
        g.watchers.entry(old).or_default().insert(1, 1);
        g.watchers.entry(cached_empty).or_default().insert(2, 1);
        let cards = g.inventories[&old].items.clone();
        let counts = (g.wos.len(), g.inventories.len());
        let players: Vec<_> = g.clients.values().map(|c| (c.id, c.player)).collect();
        let snapshot = crate::handoff::snapshot(&mut g).unwrap();
        let (n, d, s, scenes) = crate::game::test_tables().unwrap();
        let mut g = crate::handoff::restore(n, d, s, &snapshot, scenes).unwrap();
        g.net.out.clear();
        crate::testkit::send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("!admin repair-keycards")]);
        assert!(sent(&g, "ChatHandler.ReceiveChatMessageClientRpc")[0].args[1].as_str().unwrap().contains("restored"));
        assert_eq!(g.inventories[&108_742_892].items, cards);
        assert!(cards.iter().all(|id| g.holder(*id) == Some(108_742_892)));
        assert!(!g.inventories.contains_key(&old));
        assert_eq!((g.wos.len(), g.inventories.len()), counts, "no replacement or duplicated cards");
        assert_eq!(g.clients.values().map(|c| (c.id, c.player)).collect::<Vec<_>>(), players);
        assert!(g.kicks.is_empty() && g.save_requested);
        let refresh = sent(&g, "WorldObjectsHandler.DestroyWorldObjectGOClientRpc");
        assert_eq!(refresh.len(), 1);
        assert_eq!((refresh[0].to, refresh[0].args.clone()), (Target::All, vec![json!(108_742_892), json!(crate::data::stable_hash("WreckEntryLocked1")), json!(0.0)]));
        assert_eq!(sent(&g, "InventoriesHandler.CloseInventoryOnWatchersClientRpc")[0].to, Target::Only(1));
        assert!(say(&mut g, "!admin repair-keycards").contains("restored"));
        assert_eq!((g.wos.len(), g.inventories.len()), counts, "repeating recovery is idempotent");
        let save = crate::save::SaveFile::parse(&g.snapshot().to_text()).unwrap();
        let g = crate::game::test_game(save).unwrap();
        assert_eq!(g.inventories[&108_742_892].items, cards);
    }

    #[test]
    fn keycard_door_repair_requires_an_unlinked_unambiguous_inventory() {
        let Some(mut g) = world() else { return };
        let version = g.net_data.game_version.clone();
        g.net_data.game_version = "future".into();
        assert!(say(&mut g, "!admin repair-keycards").contains("verified for game 2.103 only"));
        g.net_data.game_version = version;
        assert!(say(&mut g, "!admin repair-keycards").contains("travel to Selenea"));
        g.planet = g.net_data.planet_index("Selenea").unwrap();
        assert!(say(&mut g, "!admin repair-keycards").contains("no stranded"));
        assert!(say(&mut g, "!admin repair-keycards nope").contains("must be an integer"));
        let card = g.data.group("KeyCard2").unwrap();
        let a = g.create_inventory(3, 0, 0, &[card; 3], &[]);
        let b = g.create_inventory(3, 0, 0, &[card; 3], &[]);
        let cards = g.inventories[&a].items.clone();
        assert!(say(&mut g, "!admin repair-keycards").contains("multiple matching"));
        assert!(sent(&g, "WorldObjectsHandler.DestroyWorldObjectGOClientRpc").is_empty());
        assert!(!g.inventories.contains_key(&108_742_892));
        let wo = *g.wos.keys().next().unwrap();
        g.wos.get_mut(&wo).unwrap().secondary_inventories.push(a);
        assert!(say(&mut g, &format!("!admin repair-keycards {a}")).contains("not an unlinked"));
        g.wos.get_mut(&wo).unwrap().secondary_inventories.clear();
        g.create_inventory(3, 108_742_892, 0, &[card], &[]);
        assert!(say(&mut g, &format!("!admin repair-keycards {b}")).contains("not empty"));
        assert_eq!(g.inventories[&a].items, cards);
        assert_eq!(g.inventories[&b].items.len(), 3);
        g.destroy_inventory(108_742_892, false);
        assert!(say(&mut g, &format!("!admin repair-keycards {b}")).contains("restored"));
        assert_eq!(g.inventories[&a].items, cards, "explicit choice preserves other candidates");
        g.clients.get_mut(&2).unwrap().ready.as_mut().unwrap().inventory = 108_742_892;
        assert!(say(&mut g, "!admin repair-keycards").contains("reserved by a player"));
        g.clients.get_mut(&1).unwrap().verified_steam_id = None;
        assert!(say(&mut g, "!admin repair-keycards").contains("Only server admins"));
        assert!(sent(&g, "WorldObjectsHandler.DestroyWorldObjectGOClientRpc").is_empty());
    }

    #[test]
    fn permission_uses_verified_identity_and_revocation_is_immediate() {
        let Some(mut g) = world() else { return };
        assert!(say(&mut g, "!admin help").contains("Admin commands"));
        g.clients.get_mut(&1).unwrap().verified_steam_id = None;
        // A claimed id, a matching name or a locally trusted connection is not verified Steam.
        g.clients.get_mut(&1).unwrap().ready.as_mut().unwrap().id = ADMIN;
        g.clients.get_mut(&1).unwrap().ready.as_mut().unwrap().name = ADMIN.to_string();
        g.clients.get_mut(&1).unwrap().trusted = true;
        assert!(say(&mut g, "!admin save").contains("Only server admins"));
        assert!(!g.save_requested);
        g.clients.get_mut(&1).unwrap().verified_steam_id = Some(ADMIN);
        g.access.lists.operators.clear();
        assert!(say(&mut g, "!admin rule speed.growth=2").contains("Only server admins"));
        assert_eq!(g.rules.speed.growth, 1.0);
    }

    #[test]
    fn rules_use_existing_validation_and_keep_host_controls_out_of_chat() {
        let Some(mut g) = world() else { return };
        let dir = std::env::temp_dir().join(format!("pcds-admin-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.json");
        g.rules_path = Some(path.to_string_lossy().into_owned());
        assert_eq!(say(&mut g, "!admin rule rockets.serverFlight=true"), "Set rockets.serverFlight.");
        assert!(g.rules.rockets.server_flight);
        assert!(crate::rules::Rules::parse(&std::fs::read_to_string(&path).unwrap()).unwrap().rockets.server_flight);
        for bad in ["speed.growth=-1", "speed.growth=oops", "speed.growth", "chat.exec=[\"/bin/sh\"]", "backup.s3SecretKey=secret", "access.operators=[]", "players.forgetAfterDays=1", "PCDS_ADMIN_TOKEN=x", "world.dlcPlanets=[]"] {
            assert!(say(&mut g, &format!("!admin rule {bad}")).starts_with("Admin command refused:"), "{bad}");
        }
        assert_eq!(g.rules.speed.growth, 1.0);
        assert!(g.rules.chat.exec.is_empty());
        assert_eq!(g.access.lists.operators, [ADMIN.to_string()]);
        g.rules_path = Some("/dev/null/pcds-rules.json".into());
        assert!(say(&mut g, "!admin rule speed.growth=3").contains("refused"));
        assert_eq!(g.rules.speed.growth, 1.0, "a failed persistence cannot apply the rule");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn moderation_save_and_teleport_use_the_existing_game_actions() {
        let Some(mut g) = world() else { return };
        assert_eq!(say(&mut g, "!admin save"), "Save requested. The dashboard shows the save result.");
        assert!(g.save_requested);
        assert_eq!(say(&mut g, "!admin tp Bo Smith 10 20 -30"), "Player teleported.");
        assert_eq!(sent(&g, "PlayerStatus.DieAndRespawnClientRpc")[0].to, Target::Only(2));
        assert_eq!(say(&mut g, "!admin kick #2"), "Player kicked.");
        assert!(g.kicks.iter().any(|(id, _)| *id == 2));
        g.kicks.clear();
        let bo = 76561198000000002u64;
        g.clients.get_mut(&2).unwrap().verified_steam_id = Some(bo);
        assert_eq!(say(&mut g, "!admin ban Bo Smith"), "Player banned.");
        assert!(!g.access.name_allowed("Bo Smith", None));
        assert_eq!(say(&mut g, "!admin unban Bo Smith"), "Ban entry removed.");
        assert!(!g.access.name_allowed("Bo Smith", Some(bo)), "the SteamID ban is a separate entry");
        assert_eq!(say(&mut g, &format!("!admin unban {bo}")), "Ban entry removed.");
        assert!(g.access.name_allowed("Bo Smith", Some(bo)));
        assert!(say(&mut g, "!admin kick Nobody").contains("not online"));
        assert!(say(&mut g, "!admin unban Nobody").contains("not banned"));
    }

    #[test]
    fn ambiguous_names_and_non_finite_coordinates_are_refused() {
        let Some(mut g) = world() else { return };
        g.clients.get_mut(&2).unwrap().ready.as_mut().unwrap().name = "ann".into();
        assert!(say(&mut g, "!admin kick Ann").contains("2 players online"));
        assert!(g.kicks.is_empty());
        for bad in ["NaN 0 0", "0 inf 0", "0 0 -inf", "0 0"] {
            assert!(say(&mut g, &format!("!admin tp #2 {bad}")).starts_with("Admin command refused:"));
            assert!(sent(&g, "PlayerStatus.DieAndRespawnClientRpc").is_empty());
        }
    }

    #[test]
    fn the_configured_prefix_is_respected_and_other_commands_are_untouched() {
        let Some(mut g) = world() else { return };
        for ordinary in ["hello", "!help", "!administer", "! admin save"] {
            assert!(!handle(&mut g, 1, ordinary), "{ordinary}");
        }
        g.rules.chat.command_prefix = Some("/".into());
        assert!(!handle(&mut g, 1, "!admin save"));
        assert!(say(&mut g, "/ADMIN help").contains("/admin rule"));
        assert!(g.ext.chat.outbox.is_empty(), "admin commands are not forwarded to chat hooks");
    }
}
