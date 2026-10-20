//! The rules and the settings (config.rs): the same changes the admin page and `pcds config` make.

use serde::Serialize;
use serde_json::Value;

use super::{error, json, Reply, Request};
use crate::game::Game;
use crate::{config, rules};

/// The rules as the API shows them (the hooks' targets and secret as "(set)"); the defaults
/// come with them when they are read.
#[derive(Serialize)]
struct Rules {
    rules: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    defaults: Option<Value>,
}

/// GET /api/rules.
pub(super) fn rules(game: &mut Game, _: &Request) -> Reply {
    // Through a Value, as the answer always was: the rules' f32 numbers print as the f64 they widen to.
    let defaults = serde_json::to_value(rules::Rules::default()).ok();
    json(200, &Rules { rules: rules::shown(&game.rules), defaults })
}

/// POST /api/rules: a JSON merge patch of the rules.
pub(super) fn patch_rules(game: &mut Game, r: &Request) -> Reply {
    match game.rules.patched(r.body).and_then(|new| rules::set(game, new)) {
        Ok(()) => json(200, &Rules { rules: rules::shown(&game.rules), defaults: None }),
        Err(e) => error(400, e),
    }
}

/// GET /api/config.
pub(super) fn config(game: &mut Game, _: &Request) -> Reply {
    (200, config::get(game).to_string())
}

/// POST /api/config: a change of any setting; refused as a whole with the setting named.
pub(super) fn set_config(game: &mut Game, r: &Request) -> Reply {
    match config::set(game, r.body) {
        Ok(()) => (200, config::get(game).to_string()),
        Err(e) => (400, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::call;
    use crate::game::{self, Game};
    use crate::rules;
    use serde_json::{json, Value};

    /// The rules endpoints print the bytes the json! macro made (the rules' f32 numbers widened).
    #[test]
    fn the_rules_answers_print_what_the_json_macro_did() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        for growth in [1.0, 0.1, 2.5] {
            game.rules.speed.growth = growth;
            let old = json!({"rules": rules::shown(&game.rules), "defaults": rules::Rules::default()}).to_string();
            assert_eq!(call_raw(&mut game, "GET").1, old);
            let old = json!({"rules": rules::shown(&game.rules)}).to_string();
            let (status, new) = call_raw(&mut game, "POST");
            assert_eq!((status, new), (200, old));
        }
    }

    fn call_raw(game: &mut Game, method: &str) -> (u16, String) {
        let ctx = crate::bank::status::Ctx { ticks: &Default::default(), save_path: "", build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        // A POST with an empty patch changes nothing and answers with the rules.
        super::super::request(game, method, "/api/rules", &json!({}), &ctx)
    }

    #[test]
    fn rules_change_through_the_api_and_bad_ones_are_refused() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let post = |game: &mut Game, body: Value| call(game, "POST", "/api/rules", body).0;
        assert_eq!(post(&mut game, json!({"speed": {"growth": 2}})), 200);
        assert_eq!(game.rules.speed.growth, 2.0);
        assert_eq!(post(&mut game, json!({"speed": {"growth": -1}})), 400);
        assert_eq!(post(&mut game, json!({"sped": {}})), 400);
        assert_eq!(game.rules.speed.growth, 2.0, "a refused change keeps the rules");
    }

    /// The hooks' targets and secret never come back from the rules endpoints.
    #[test]
    fn the_rules_endpoints_never_show_hook_secrets() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let set = json!({"chat": {"webhooks": ["https://example.com/hook/t0ken"], "secret": "s3cret", "exec": ["on-chat", "--key=k3y"]}});
        let (status, answer) = call(&mut game, "POST", "/api/rules", set);
        let (_, read) = call(&mut game, "GET", "/api/rules", json!({}));
        assert_eq!(status, 200);
        for text in [answer.to_string(), read.to_string()] {
            assert!(!text.contains("t0ken") && !text.contains("s3cret") && !text.contains("k3y"), "{text}");
        }
        assert_eq!(read["rules"]["chat"]["secret"], "(set)");
    }

    #[test]
    fn autosave_minutes_stay_within_their_range() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let (status, _) = call(&mut game, "POST", "/api/config", json!({"save.autosaveMinutes": 7}));
        assert_eq!((status, game.rules.save.autosave_minutes), (200, 7));
        // Out of range or not a whole number: refused, and the interval stays.
        for bad in [json!(160_000_000_000_000_000u64), json!(2.5), json!(-1), json!("5")] {
            let (status, answer) = call(&mut game, "POST", "/api/config", json!({"save.autosaveMinutes": bad}));
            assert_eq!(status, 400, "{bad}: {answer}");
        }
        assert_eq!(game.rules.save.autosave_minutes, 7);
    }

    #[test]
    fn the_config_endpoint_round_trips() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let (status, config) = call(&mut game, "GET", "/api/config", json!({}));
        assert_eq!(status, 200);
        let knobs = config["knobs"].as_array().unwrap();
        let knob = |key: &str| knobs.iter().find(|k| k["key"] == key).unwrap_or_else(|| panic!("{key}")).clone();
        assert_eq!((knob("speed.growth")["type"].as_str(), knob("speed.growth")["default"].as_f64(), knob("speed.growth")["live"].as_bool()), (Some("float"), Some(1.0), Some(true)));
        assert_eq!(knob("PCDS_ADMIN_TOKEN")["live"], false);
        assert!(knob("PCDS_LISTEN")["readOnly"].is_string());
        assert_eq!((knob("update.auto")["default"].as_bool(), knob("update.checkHours")["default"].as_i64()), (Some(false), Some(24)));
        // Every value the page can change, posted back as read: accepted, nothing changes.
        let writable: serde_json::Map<String, Value> = knobs.iter().filter(|k| k["readOnly"].is_null()).map(|k| (k["key"].as_str().unwrap().to_string(), k["value"].clone())).collect();
        assert!(writable.contains_key("access.bannedNames") && writable.contains_key("save.autosaveMinutes"));
        assert_eq!(call(&mut game, "POST", "/api/config", json!(writable)).1, config);
        // A change of each kind of setting, read back.
        let change = json!({"speed.growth": 2.0, "death.consequences": "DropAllItems", "limits.buildings": {"Drill1": 3}, "access.whitelist": ["Ann"], "save.autosaveMinutes": 10});
        let (status, after) = call(&mut game, "POST", "/api/config", change.clone());
        assert_eq!(status, 200, "{after}");
        for (key, value) in change.as_object().unwrap() {
            let got = after["knobs"].as_array().unwrap().iter().find(|k| k["key"] == key.as_str()).unwrap()["value"].clone();
            assert_eq!(&got, value, "{key}");
        }
        assert_eq!((game.rules.speed.growth, game.access.lists.whitelist.clone(), game.rules.save.autosave_minutes), (2.0, vec!["Ann".to_string()], 10));
        assert_eq!(call(&mut game, "GET", "/api/config", json!({})).1, after);
        // A map is replaced, not merged; null puts back the default.
        call(&mut game, "POST", "/api/config", json!({"limits.buildings": {"Vegetube1": 1}}));
        assert_eq!(game.rules.limits.buildings.keys().collect::<Vec<_>>(), ["Vegetube1"]);
        let resets: serde_json::Map<String, Value> = writable.keys().map(|k| (k.clone(), Value::Null)).collect();
        assert_eq!(call(&mut game, "POST", "/api/config", json!(resets)).0, 200);
        assert_eq!((game.rules.clone(), game.access.lists.whitelist.len()), (rules::Rules::default(), 0));
        // Refused, with the setting named, and nothing of the change applied.
        for (bad, key) in [
            (json!({"speed.growth": 3, "gauges.foodDrain": -1}), "gauges.foodDrain"),
            (json!({"speed.growth": 3, "limits.buildings": {"Iron": 1}}), "limits.buildings"),
            (json!({"speed.growth": 3, "PCDS_LISTEN": "0.0.0.0:1"}), "PCDS_LISTEN"),
            (json!({"speed.growth": 3, "speed.grwoth": 1}), "speed.grwoth"),
            (json!({"speed.growth": 3, "access.whitelist": "Ann"}), "access.whitelist"),
            (json!({"speed.growth": 3, "save.autosaveMinutes": 100000}), "save.autosaveMinutes"),
            (json!({"access.whitelist": ["Bob"], "limits.buildings": {"Iron": 1}}), "limits.buildings"),
        ] {
            let (status, answer) = call(&mut game, "POST", "/api/config", bad.clone());
            assert_eq!((status, answer["key"].as_str()), (400, Some(key)), "{bad}: {answer}");
            assert_eq!(game.rules.speed.growth, 1.0, "{bad}");
            assert!(game.access.lists.whitelist.is_empty(), "{bad}: the access lists are kept as they were");
        }
    }
}
