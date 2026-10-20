//! Who an API call names, and what it does to them: kick, ban, unban, teleport. The access policy
//! (`enforce`, who the lists refuse) is `access::enforce`, not here.

use super::{error, field, kept, ok, Reply, Request};
use crate::dashboard;
use crate::game::Game;

/// Who an API call names: {"id": N} is that client id exactly; {"name": S} a player's name in
/// any case.
enum Who {
    Id(u64),
    Name(String),
}

fn who(body: &serde_json::Value) -> Result<Who, Reply> {
    if let Some(id) = body.get("id").filter(|v| !v.is_null()) {
        return id.as_u64().map(Who::Id).ok_or_else(|| error(400, "'id': a client id"));
    }
    let name = body["name"].as_str().map(str::trim).filter(|s| !s.is_empty());
    name.map(|n| Who::Name(n.to_string())).ok_or_else(|| error(400, "'id' or 'name' is required"))
}

/// The online client named; None: nobody online is. Two players with that name (any case) are
/// refused: the caller must give the id.
fn find_client(game: &Game, who: &Who) -> Result<Option<u64>, Reply> {
    match who {
        Who::Id(id) => Ok(game.clients.contains_key(id).then_some(*id)),
        Who::Name(name) => {
            let found: Vec<u64> = game.clients.values().filter(|c| c.ready.as_ref().is_some_and(|s| crate::access::same_name(&s.name, name))).map(|c| c.id).collect();
            match found[..] {
                [] => Ok(None),
                [one] => Ok(Some(one)),
                _ => Err(error(400, format!("{} players online are named {name}: give the client id", found.len()))),
            }
        }
    }
}

pub(super) fn kick(game: &mut Game, r: &Request) -> Reply {
    match who(r.body).and_then(|w| find_client(game, &w)) {
        Ok(Some(c)) => {
            game.kick(c, "Kicked");
            ok()
        }
        Ok(None) => error(400, "player not online"),
        Err(e) => e,
    }
}

pub(super) fn ban(game: &mut Game, r: &Request) -> Reply {
    let named = match who(r.body) {
        Ok(w) => w,
        Err(e) => return e,
    };
    match find_client(game, &named) {
        Ok(Some(c)) => {
            let client = &game.clients[&c];
            let name = client.ready.as_ref().map(|s| s.name.clone());
            let name = match (name, &named) {
                (Some(n), _) => n,
                (None, Who::Name(n)) => n.clone(),
                (None, Who::Id(_)) => return error(400, "that client has no name yet: kick it instead"),
            };
            let (steam_id, ip) = (client.verified_steam_id, client.ip.clone());
            if let Err(e) = game.access.ban(&name, steam_id, ip.as_deref()) {
                return error(500, e);
            }
            game.kick(c, "Banned");
            ok()
        }
        Ok(None) => match named {
            Who::Name(n) => kept(game.access.ban(&n, None, None)),
            Who::Id(_) => error(400, "player not online: ban by name"),
        },
        Err(e) => e,
    }
}

pub(super) fn unban(game: &mut Game, r: &Request) -> Reply {
    match field(r.body, "value") {
        Some(v) => match game.access.unban(&v) {
            Ok(true) => ok(),
            Ok(false) => error(400, format!("{v} is not banned (the bans: GET /api/status, access; a range is unbanned by its own entry)")),
            Err(e) => error(500, e),
        },
        None => error(400, "'value' is required"),
    }
}

pub(super) fn teleport(game: &mut Game, r: &Request) -> Reply {
    let t = match dashboard::body::<dashboard::Teleport>(r.body) {
        Ok(t) => t,
        Err(e) => return e,
    };
    if !t.position.iter().all(|v| v.is_finite()) {
        return error(400, "position: three finite numbers");
    }
    let named = match t.id {
        Some(id) => Who::Id(id),
        None => match t.name {
            Some(n) => Who::Name(n),
            None => return error(400, "'id' or 'name' is required"),
        },
    };
    match find_client(game, &named) {
        Ok(Some(c)) if crate::status::teleport(game, c, t.position) => {
            println!("client {c}: teleported to {:?} (admin)", t.position);
            ok()
        }
        Err(e) => e,
        _ => error(400, "player not online and ready"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::call;
    use crate::{access, game, protocol, wire};
    use serde_json::json;

    #[test]
    fn unbanning_what_is_not_banned_is_said() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        game.access.ban("Mallory", None, Some("10.9.9.9")).unwrap();
        let (status, answer) = call(&mut game, "POST", "/api/unban", json!({"value": "Mellory"}));
        assert!(status == 400 && answer["error"].as_str().unwrap().contains("Mellory is not banned"), "{status} {answer}");
        assert_eq!(game.access.lists.banned_names, ["Mallory"]);
        assert_eq!(call(&mut game, "POST", "/api/unban", json!({"value": " mallory "})).0, 200);
        assert_eq!(call(&mut game, "POST", "/api/unban", json!({"value": "10.9.9.9"})).0, 200);
        assert!(game.access.lists.banned_names.is_empty() && game.access.lists.banned_ips.is_empty());
    }

    #[test]
    fn players_list_teleport_and_chat_through_the_api() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let object = game::tests::join(&mut game, 1, "Ann");
        game.net.objects.get_mut(&object).unwrap().position = [560.0, 3.0, 500.0];
        game.connect(2, None, false, Some("10.0.0.2".into()));
        let (status, s) = call(&mut game, "GET", "/api/status", json!({}));
        assert_eq!(status, 200);
        let players = s["players"].as_array().unwrap();
        assert_eq!(players.iter().map(|p| (p["id"].as_u64().unwrap(), p["status"].as_str().unwrap())).collect::<Vec<_>>(), [(1, "ready"), (2, "joining")]);
        assert_eq!((players[0]["name"].as_str(), players[0]["position"].clone(), players[0]["gauges"].as_array().map(Vec::len)), (Some("Ann"), json!([560.0, 3.0, 500.0]), Some(4)));
        assert!(players[0]["gaugeMax"].as_array().is_some_and(|m| m.len() == 4 && m.iter().all(|v| v.as_f64().unwrap() > 0.0)), "{}", players[0]);
        assert_eq!((players[1]["ip"].as_str(), players[1]["position"].is_null()), (Some("10.0.0.2"), true));
        let stages = s["stages"]["list"].as_array().unwrap();
        assert!(stages.len() > 3 && stages.windows(2).all(|w| w[0]["start"].as_f64() <= w[1]["start"].as_f64()), "stages in order: {stages:?}");
        assert!(stages.iter().any(|st| st["key"] == s["stages"]["current"]), "the current stage is one of them");
        for key in ["serverVersion", "tables", "world", "objects", "units", "gameTime"] {
            assert!(!s[key].is_null(), "{key}");
        }
        // Teleport: the game's respawn move, to that player only.
        let before = game.net.out.len();
        assert_eq!(call(&mut game, "POST", "/api/teleport", json!({"name": "ann", "position": [10.0, 20.5, -30.0]})).0, 200);
        let sent: Vec<_> = game.net.out[before..].iter().filter(|(_, kind, _)| *kind == protocol::CLIENT_RPC).collect();
        assert_eq!(sent.len(), 1);
        let (to, _, body) = sent[0];
        assert!(to.reaches(1) && !to.reaches(2));
        let mut r = wire::Reader::new(body);
        assert_eq!(r.packed_u64(), Some(object));
        r.packed_u16();
        let method = r.packed_u32().unwrap();
        assert_eq!(game.net.spec.get(method).unwrap().name, "PlayerStatus.DieAndRespawnClientRpc");
        assert_eq!(game.net.spec.decode(method, r.rest()).unwrap(), vec![json!(false), json!(0), json!([10.0, 20.5, -30.0])]);
        assert_eq!(call(&mut game, "POST", "/api/teleport", json!({"id": 1, "position": [1, 2, 3]})).0, 200);
        for bad in [json!({"id": 2, "position": [0, 0, 0]}), json!({"name": "Bob", "position": [0, 0, 0]}), json!({"name": "Ann"}), json!({"name": "Ann", "position": [0, 0]}), json!({"name": "Ann", "position": [0, 0, 0], "x": 1})] {
            assert_eq!(call(&mut game, "POST", "/api/teleport", bad.clone()).0, 400, "{bad}");
        }
        // The map: this planet's buildings.
        let (status, map) = call(&mut game, "GET", "/api/map", json!({}));
        assert_eq!((status, map["planet"].as_str(), map["buildings"].is_array()), (200, Some("Prime"), true));
        assert!(map["buildings"].as_array().unwrap().iter().all(|b| b.as_array().is_some_and(|p| p.len() == 3)), "x, y, z");
    }

    /// An id names that client only, never a player whose name is the same digits; a name two
    /// players share is refused.
    #[test]
    fn players_are_named_by_id_or_by_a_name_only_one_has() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        game::tests::join(&mut game, 1, "4");
        game::tests::join(&mut game, 4, "Ann");
        assert_eq!(call(&mut game, "POST", "/api/kick", json!({"id": 4})).0, 200);
        assert_eq!(game.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [4], "client 4, not the player named 4");
        game.kicks.clear();
        assert_eq!(call(&mut game, "POST", "/api/kick", json!({"name": "4"})).0, 200);
        assert_eq!(game.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [1]);
        // The game refuses a second player of one name; two of them is a rename away.
        game::tests::join(&mut game, 5, "Bob");
        game.clients.get_mut(&5).unwrap().ready.as_mut().unwrap().name = "ann".into();
        let (status, answer) = call(&mut game, "POST", "/api/kick", json!({"name": "ANN"}));
        assert_eq!(status, 400, "{answer}");
        assert!(answer["error"].as_str().unwrap().contains("client id"));
        assert_eq!(call(&mut game, "POST", "/api/kick", json!({})).0, 400);
        assert_eq!(call(&mut game, "POST", "/api/ban", json!({"id": 99})).0, 400, "an id names an online client");
    }

    /// A ban that cannot be written answers 500 (it would be lost at the next start).
    #[test]
    fn a_ban_that_cannot_be_kept_answers_500() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        game.access = access::Access::load(Some("/nonexistent-dir-pcds/access.json".into()));
        let (status, answer) = call(&mut game, "POST", "/api/ban", json!({"name": "Mallory"}));
        assert_eq!(status, 500, "{answer}");
        assert!(game.access.lists.banned_names.is_empty());
    }
}
