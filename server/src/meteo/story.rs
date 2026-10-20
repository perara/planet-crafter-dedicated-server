//! SpaceCraft.StoryEventsHandler, server side: story events picked at random a few times an hour
//! (terraformation stage, chance, prerequisite events and buildings), the ones clients launch,
//! and what each brings: a message, a meteo event, an unlock. The save keeps the triggered list.

use serde_json::{json, Value};

use super::{between_stages, planet_data, refs, reference, singleton};
use crate::clock::GameTime;
use crate::data::stable_hash;
use crate::game::Game;
use crate::net::{enc, ListEvent, Target};

const HANDLER: &str = "StoryEventsHandler";

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Story {
    /// The planet InitStoryEvents last ran for.
    planet: Option<usize>,
    /// _availableEvents, _currentPlanetEvents, _manualEvents: StoryEventData keys.
    available: Vec<String>,
    pub(super) current_planet: Vec<String>,
    manual: Vec<String>,
    /// _triggeredStoryEvents.
    triggered: Vec<i32>,
    next_try: GameTime,
}

fn period(game: &Game) -> f32 {
    let per_hour = singleton(game, HANDLER)["eventsTryEveryHour"].as_f64().unwrap_or(0.0) as f32;
    3600.0 / if per_hour == 0.0 { 2.0 } else { per_hour }
}

/// InitStoryEvents: the planet's events join the known ones, and the coroutine starts over.
fn init_story_events(game: &mut Game) {
    let planet = planet_data(game);
    let shared = planet["sharedManualEvents"].as_str().and_then(|k| game.data.asset(k)).map(|l| refs(&l["storyEventDatas"])).unwrap_or_default();
    let next_try = game.time + period(game);
    let s = &mut game.ext.meteo.story;
    s.planet = Some(game.planet);
    s.current_planet = refs(&planet["availableStoryEvents"]);
    s.available.extend(s.current_planet.iter().cloned());
    s.manual.extend(refs(&planet["manualStoryEvents"]));
    s.manual.extend(shared);
    s.next_try = next_try;
}

/// InitStoryEvents for the starting planet, then SavedDataHandler.LoadStoryEventsData: the other
/// saved planets' events, and the saved triggered events the handler knows (SetTriggeredEvents).
pub fn init(game: &mut Game) {
    init_story_events(game);
    let current = game.planet_id().to_string();
    for planet in game.save.planets.clone() {
        let Some(id) = planet["planetId"].as_str().filter(|id| *id != current) else { continue };
        let Some(p) = game.data.asset(&format!("PlanetData:PlanetData-{id}")).cloned() else { continue };
        let s = &mut game.ext.meteo.story;
        s.available.extend(refs(&p["availableStoryEvents"]));
        s.manual.extend(refs(&p["manualStoryEvents"]));
    }
    let ids: Vec<String> = game.save.story_events.iter().filter_map(|e| e["stringId"].as_str()).filter(|id| by_id(game, id).is_some()).map(str::to_string).collect();
    game.ext.meteo.story.triggered = ids.iter().map(|id| stable_hash(id)).collect();
    // SetAndGetStoryEvents writes back exactly these.
    game.save.story_events = ids.iter().map(|id| json!({"stringId": id})).collect();
    let items = game.ext.meteo.story.triggered.iter().map(|h| enc::int(*h)).collect();
    if let Some((object, _)) = game.net.find_class(HANDLER) {
        game.net.objects.get_mut(&object).unwrap().init_list(HANDLER, "_triggeredStoryEvents", items);
    }
}

pub fn tick(game: &mut Game) {
    if game.ext.meteo.story.planet != Some(game.planet) {
        init_story_events(game);
    }
    if game.time >= game.ext.meteo.story.next_try {
        game.ext.meteo.story.next_try = game.time + period(game);
        try_to_launch(game);
    }
}

fn event<'a>(game: &'a Game, key: &str) -> Option<&'a Value> {
    game.data.asset(key)
}

fn find(game: &Game, matches: impl Fn(&str) -> bool) -> Option<String> {
    let s = &game.ext.meteo.story;
    s.available.iter().chain(&s.manual).find(|k| event(game, k).and_then(|e| e["id"].as_str()).is_some_and(&matches)).cloned()
}

/// GetStoryEventDataViaId.
fn by_id(game: &Game, id: &str) -> Option<String> {
    find(game, |i| i == id)
}

fn has_triggered(game: &Game, key: &str) -> bool {
    event(game, key).and_then(|e| e["id"].as_str()).is_some_and(|id| game.ext.meteo.story.triggered.contains(&stable_hash(id)))
}

/// TryToLaunchAnEventLogic: up to 200 random picks among the planet's events, each tried once.
fn try_to_launch(game: &mut Game) {
    if game.units.planet(game.planet_hash()).is_none() {
        return;
    }
    let mut list = game.ext.meteo.story.current_planet.clone();
    for _ in 0..200 {
        if list.is_empty() {
            break;
        }
        let index = game.rng.range(0, list.len() as i32) as usize;
        if let Some(e) = event(game, &list[index]).cloned() {
            let fits = between_stages(game, reference(&e, "startTerraformStage"), reference(&e, "stopTerraformStage"))
                && e["chanceToAppear"].as_i64().unwrap_or(0) >= game.rng.range(0, 100) as i64
                && (!e["canOnlyBeTriggerdOnce"].as_bool().unwrap_or(false) || !has_triggered(game, &list[index]))
                && refs(&e["onlyIfAllEventsHaveTriggered"]).iter().all(|k| has_triggered(game, k))
                && refs(&e["onlyIfGroupsConstructed"]).iter().all(|g| game.data.group_of_ref(g).is_some_and(|g| game.group_count(true, g) > 0));
            if fits {
                launch(game, &list[index]);
                break;
            }
        }
        list.remove(index);
    }
}

/// LaunchSpecificStoryEventEventServerRpc.
pub fn launch_by_hash(game: &mut Game, hash: i32) {
    if let Some(key) = find(game, |id| stable_hash(id) == hash) {
        launch(game, &key);
    }
}

/// LaunchSpecificStoryEventEvent on the server.
pub fn launch(game: &mut Game, key: &str) {
    let Some(e) = event(game, key).cloned() else { return };
    let Some(id) = e["id"].as_str() else { return };
    let hash = stable_hash(id);
    if e["canOnlyBeTriggerdOnce"].as_bool().unwrap_or(false) && game.ext.meteo.story.triggered.contains(&hash) {
        return;
    }
    game.ext.meteo.story.triggered.push(hash);
    game.list_events_on(HANDLER, "_triggeredStoryEvents", vec![ListEvent::Add(enc::int(hash))]);
    game.save.story_events.push(json!({"stringId": id}));
    if let Some(message) = reference(&e, "messageDataStart").and_then(|m| game.data.asset(m)).and_then(|m| m["stringId"].as_str()) {
        crate::handlers::add_message(game, stable_hash(message), true);
    }
    if let Some(meteo) = reference(&e, "meteoDataEvent") {
        super::queue_meteo_event(game, meteo, None);
    }
    game.net.rpc_scene(Target::All, "StoryEventsHandler.AnEventHasBeenTriggeredClientRpc", &[json!(hash)]);
    if let Some(group) = reference(&e, "unlockGroupOnReceive").and_then(|g| game.data.group_of_ref(g)) {
        let group_hash = game.data.groups[group].hash;
        crate::unlocks::unlock_globally(game, group_hash, false, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meteo::tests::game;
    use crate::net::VarValue;

    fn triggered_var(g: &Game) -> Vec<Vec<u8>> {
        let (object, _) = g.net.find_class(HANDLER).unwrap();
        match g.net.get(object, HANDLER, "_triggeredStoryEvents") {
            Some(VarValue::List(items)) => items.clone(),
            _ => panic!(),
        }
    }

    #[test]
    fn knows_the_planets_events() {
        let Some(g) = game() else { return };
        let s = &g.ext.meteo.story;
        assert_eq!(s.current_planet.len(), 13);
        assert!(s.manual.contains(&"StoryEventData:UiEvent-FirstTimeVegetubes".to_string()), "shared manual events");
        assert_eq!(by_id(&g, "StoryEvent-Toxicity-ending"), None, "Toxicity is not a saved planet");
        assert_eq!(period(&g), 600.0);
    }

    #[test]
    fn a_story_event_brings_its_message_meteo_and_unlock() {
        let Some(mut g) = game() else { return };
        launch(&mut g, "StoryEventData:StoryEvent-Message-TradePlatform");
        let hash = stable_hash("StoryEvent-Message-TradePlatform");
        assert_eq!(triggered_var(&g), vec![enc::int(hash)]);
        assert_eq!(g.save.story_events, vec![json!({"stringId": "StoryEvent-Message-TradePlatform"})]);
        assert!(g.save.messages.iter().any(|m| m["stringId"] == "Message_TradePlatform"));
        assert!(crate::unlocks::is_globally_unlocked(&g, g.data.group("TradePlatform1").unwrap()));
        // Only once.
        launch(&mut g, "StoryEventData:StoryEvent-Message-TradePlatform");
        assert_eq!(triggered_var(&g).len(), 1);
        // A meteo story event queues its weather; a client launches a manual one by hash.
        launch_by_hash(&mut g, stable_hash("StoryEvent-FirstAsteroids"));
        assert_eq!(g.ext.meteo.queue[0].0, "MeteoEventData:MeteoEvent-BasicOresFullRed");
        launch_by_hash(&mut g, stable_hash("StoryEvent-Mont-Open"));
        launch_by_hash(&mut g, stable_hash("StoryEvent-Mont-Open"));
        assert_eq!(triggered_var(&g).len(), 4, "Mont-Open can trigger again");
    }

    #[test]
    fn saved_events_load_when_known() {
        let Some(mut g) = game() else { return };
        g.save.story_events = vec![json!({"stringId": "StoryEvent-FirstRain"}), json!({"stringId": "NoSuchEvent"})];
        init(&mut g);
        assert_eq!(g.ext.meteo.story.triggered, vec![stable_hash("StoryEvent-FirstRain")]);
        assert_eq!(g.save.story_events.len(), 1);
    }

    #[test]
    fn random_events_respect_their_conditions() {
        let Some(mut g) = game() else { return };
        // On a barren Prime only stage-free events without prerequisites can come: Help1 (50 %),
        // PlanetIdentification (75 %) or FirstAsteroids.
        for _ in 0..50 {
            try_to_launch(&mut g);
        }
        let allowed = ["StoryEvent-Message-Help1", "StoryEvent-Message-PlanetIdentification", "StoryEvent-FirstAsteroids", "StoryEvent-Message-Help2"];
        assert!(g.save.story_events.iter().all(|e| allowed.contains(&e["stringId"].as_str().unwrap())), "{:?}", g.save.story_events);
        assert!(!g.save.story_events.is_empty());
    }
}
