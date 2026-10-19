//! The rules file: JSON in, Rules (and the merge patch of the admin API) out, then validated
//! against the game's tables.
use super::*;
use std::sync::OnceLock;

static DATA: OnceLock<crate::gamedata::GameData> = OnceLock::new();

pub fn run(data: &[u8]) {
    let game = DATA.get_or_init(|| crate::gamedata::GameData::load(&format!("{}/gamedata.json", crate::fuzz_entry::data_dir())).expect("gamedata.json"));
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Ok(rules) = serde_json::from_str::<Rules>(text) {
        let _ = rules.validate(game);
        let _ = shown(&rules);
        let _ = changes(&rules);
        let _ = rules.max_players();
        // A rules file that loads and validates must save and load back the same.
        if rules.validate(game).is_ok() {
            let again: Rules = serde_json::from_str(&serde_json::to_string_pretty(&rules).unwrap()).expect("rules written by the server read back");
            assert!(again == rules);
        }
    } else {
        let _ = unreadable(text, &serde_json::from_str::<Rules>(text).err().unwrap());
    }
    // As a patch onto the defaults.
    if let Ok(patch) = serde_json::from_str::<Value>(text) {
        if let Ok(rules) = Rules::default().patched(&patch) {
            // Whatever the patch gave must read back from its own serialization (a number past
            // f32 once became null here).
            let again = serde_json::to_string(&rules).unwrap();
            assert!(serde_json::from_str::<Rules>(&again).is_ok(), "a patched rules does not read back: {again}");
        }
    }
}
