//! The sky and the story, server side: SpaceCraft.MeteoHandler with MeteoSendInSpace (weather and
//! meteor events: which one, when, for how long, around whom), the server's AsteroidsHandler (its
//! random state, which MeteoHandler._currentSeed publishes), EnvironmentDayNightCycle, and
//! TerraformStagesHandler's current stage. Story events are in meteo/story.rs, explosives in
//! meteo/explode.rs.
//!
//! Meteors themselves are simulated by every client from the shared seed and reference position.
//! The impact points need the terrain (raycasts), which only clients have: the reference client
//! (`_asteroidReferenceClient`) is the one that creates the resources a meteor leaves, through
//! WorldObjectsHandler.CreateAndDropOnFloorServerRpc at the impact points it computed (the core
//! handles that RPC). The real host also creates them when it is the reference itself or when the
//! reference client left mid-event; this server has no terrain, so it hands the reference to a
//! connected player then (hand_over_reference).

use serde_json::Value;

use crate::clock::GameTime;
use crate::game::{Args, Game};
use crate::net::enc;
use crate::units::Kind;
use crate::wire::Writer;

pub mod explode;
mod story;

const HANDLER: &str = "MeteoHandler";
const DAY_NIGHT: &str = "EnvironmentDayNightCycle";

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Meteo {
    /// The planet the event lists were built for (InitMeteoHandler runs on every planet load).
    planet: Option<usize>,
    /// _meteoEvents: MeteoEventData asset keys ("" for a missing asset), and how many of them
    /// random events pick from (_meteoEventsOriginalCount).
    events: Vec<String>,
    original_count: usize,
    pub queue: Vec<(String, Option<[f32; 3]>)>,
    /// Server-side copies of _selectedDataMeteoEventIndex, _timeNewMeteoSet and _currentSeed.
    selected: i32,
    time_set: f64,
    seed: u32,
    /// _selectedAsteroidEventData: the asteroid event instance the running meteo event added.
    selected_asteroids: Option<u32>,
    /// _asteroidReferenceClient and the event's _asteroidReferencePosition (None: nobody was
    /// online at the launch, the position was not set).
    reference: (u64, Option<[f32; 3]>),
    /// TryToLaunchAnEvent and UpdateMeteo coroutines.
    next_try: GameTime,
    try_every: f32,
    next_update: GameTime,
    asteroids: Asteroids,
    day: DayNight,
    story: story::Story,
    explode: explode::Explode,
}

/// The f32 field of an asset (0 when absent).
fn num(v: &Value, key: &str) -> f32 {
    v[key].as_f64().unwrap_or(0.0) as f32
}

/// An asset reference list; missing entries stay as "" so indices match the game's lists.
fn refs(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().map(|r| r.as_str().unwrap_or("").to_string()).collect()
}

/// A nullable asset reference field.
fn reference<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v[key].as_str()
}

fn planet_data(game: &Game) -> Value {
    game.data.asset(&format!("PlanetData:PlanetData-{}", game.planet_id())).cloned().unwrap_or(Value::Null)
}

fn singleton<'a>(game: &'a Game, class: &str) -> &'a Value {
    &game.data.singletons[class]
}

fn uint(v: u32) -> Vec<u8> {
    Writer::new().packed_u32(v).0.clone()
}

fn vec3(v: [f32; 3]) -> Vec<u8> {
    Writer::new().vec3(v).0.clone()
}

/// Sets a scene variable's starting value (no delta: the scene goes out with ConnectionApproved).
fn init_var(game: &mut Game, class: &str, var: &str, bytes: Vec<u8>) {
    if let Some((object, _)) = game.net.find_class(class) {
        game.net.objects.get_mut(&object).unwrap().init(class, var, bytes);
    }
}

/// Before the world objects are placed: the scene variables' starting values.
pub fn init(game: &mut Game) {
    game.ext.meteo.selected = -1;
    init_var(game, HANDLER, "_selectedDataMeteoEventIndex", enc::int(-1));
    init_var(game, HANDLER, "_timeNewMeteoSet", enc::double(0.0));
    init_var(game, HANDLER, "_currentSeed", uint(0));
    init_var(game, HANDLER, "_asteroidReferencePosition", vec3([0.0; 3]));
    init_var(game, HANDLER, "_asteroidReferenceClient", enc::ulong(0));
    init_var(game, DAY_NIGHT, "_dayNightLerpValue", enc::float(0.0));
    story::init(game);
}

/// The planet is loaded and the world placed: the handlers' coroutines start.
pub fn init_late(game: &mut Game) {
    let s = singleton(game, DAY_NIGHT);
    let day = DayNight { full_day_stay: num(s, "fullDayStayTime"), speed: num(s, "nightFallSpeed"), every: num(s, "_updateCoroutineTime"), next: game.time, full_phase_reached: GameTime::at(-99.0), ..Default::default() };
    game.ext.meteo.day = day;
    planet_loaded(game);
    explode::init(game);
    // MessagesHandler.SetReceivedMessages: the messages every world starts with.
    for m in refs(&singleton(game, "MessagesHandler")["defaultReceivedMessages"]) {
        if let Some(hash) = game.data.asset(&m).and_then(|m| m["stringId"].as_str()).map(crate::data::stable_hash) {
            crate::handlers::add_message(game, hash, false);
        }
    }
}

pub fn tick(game: &mut Game) {
    if game.ext.meteo.planet != Some(game.planet) {
        planet_loaded(game);
    }
    day_night(game);
    hand_over_reference(game);
    asteroids_tick(game);
    update_meteo(game);
    if game.time >= game.ext.meteo.next_try {
        let m = &mut game.ext.meteo;
        m.next_try = game.time + m.try_every;
        try_to_launch(game);
    }
    story::tick(game);
    explode::tick(game);
}

pub fn rpc(game: &mut Game, _client: u64, _object: u64, _behaviour: u16, name: &str, a: &Args) -> bool {
    match name {
        // The game would index past its list and stop its meteo coroutine; we ignore bad indices.
        "MeteoHandler.LaunchSpecificMeteoEventServerRpc" => {
            let index = a.i(0);
            if index >= 0 && (index as usize) < game.ext.meteo.events.len() && !game.ext.meteo.events[index as usize].is_empty() {
                launch_specific(game, index, None);
            }
        }
        "StoryEventsHandler.LaunchSpecificStoryEventEventServerRpc" => story::launch_by_hash(game, a.i(0)),
        _ => return false,
    }
    true
}

/// SessionController's planetIsLoaded, for these handlers: AsteroidsHandler.InitAsteroidsHandler,
/// then MeteoHandler.InitMeteoHandler (its coroutines start with the first planet only).
fn planet_loaded(game: &mut Game) {
    let first = game.ext.meteo.planet.is_none();
    game.ext.meteo.planet = Some(game.planet);
    let now = game.time;
    game.ext.meteo.asteroids.events.clear();
    game.ext.meteo.asteroids.next_check = now;
    if game.ext.meteo.selected >= 0 {
        set_selected(game, -1);
    }
    let planet = planet_data(game);
    let with_asteroids = refs(&planet["meteoEventsWithAsteroids"]);
    let mut events = refs(&planet["meteoEvents"]);
    events.extend(with_asteroids.iter().cloned());
    events.extend(with_asteroids);
    let original_count = events.len();
    events.extend(refs(&singleton(game, "MeteoSendInSpace")["meteoEvents"]));
    let s = singleton(game, HANDLER);
    let try_every = num(s, "tryToLaunchEventEvery") / game.save.settings["modifierMeteoOccurence"].as_f64().unwrap_or(1.0) as f32;
    let m = &mut game.ext.meteo;
    m.events = events;
    m.original_count = original_count;
    m.queue.clear();
    if first {
        m.try_every = try_every;
        m.next_try = now + 60.0;
        m.next_update = now;
    }
}

/// MeteoHandler.QueueMeteoEvent.
pub fn queue_meteo_event(game: &mut Game, event: &str, position: Option<[f32; 3]>) {
    game.ext.meteo.queue.push((event.to_string(), position));
}

/// MeteoHandler.SendSomethingInSpace: a rocket sent from `position` brings the meteo event
/// MeteoSendInSpace pairs with its group (for the ConstructibleProxy.SendInSpaceServerRpc port).
pub fn send_something_in_space(game: &mut Game, group: usize, position: [f32; 3]) {
    let s = singleton(game, "MeteoSendInSpace");
    let groups = refs(&s["groupsData"]);
    let events = refs(&s["meteoEvents"]);
    let Some(i) = groups.iter().position(|g| game.data.group_of_ref(g) == Some(group)) else { return };
    if let Some(event) = events.get(i).cloned() {
        queue_meteo_event(game, &event, Some(position));
    }
}

/// TryToLaunchAnEventLogic.
fn try_to_launch(game: &mut Game) {
    let m = &game.ext.meteo;
    if !m.queue.is_empty() && m.selected == -1 {
        let (event, position) = m.queue[0].clone();
        if let Some(i) = m.events.iter().position(|e| *e == event) {
            launch_specific(game, i as i32, position);
        }
        game.ext.meteo.queue.remove(0);
    } else if m.queue.is_empty() && m.selected == -1 {
        let chance = num(singleton(game, HANDLER), "eventChanceOnOneHundred");
        if chance * game.rules.meteo.event_chance > game.rng.rangef(0.0, 100.0) {
            launch_random(game);
        }
    }
}

/// LaunchRandomMeteoEvent: up to 100 picks among the planet's own events for one that fits the
/// terraformation stage.
fn launch_random(game: &mut Game) {
    if game.ext.meteo.selected != -1 || game.ext.meteo.original_count == 0 {
        return;
    }
    let inited = game.units.planet(game.planet_hash()).is_some();
    for _ in 0..100 {
        let i = game.rng.range(0, game.ext.meteo.original_count as i32);
        let Some(event) = game.data.asset(&game.ext.meteo.events[i as usize]) else { continue };
        if inited && between_stages(game, reference(event, "startTerraformStage"), reference(event, "stopTerraformStage")) {
            launch_specific(game, i, None);
            break;
        }
    }
}

/// LaunchSpecificMeteoEvent(int, Vector3?): a new seed, the reference position and client (a
/// random player unless given), the start time, then the event.
fn launch_specific(game: &mut Game, index: i32, position: Option<[f32; 3]>) {
    let seed = game.rng.range(1, 999_999) as u32;
    set_seed(game, seed);
    let (client, position) = pick_reference(game, position);
    set_reference(game, client, position);
    let now = game.time.secs();
    game.ext.meteo.time_set = now;
    game.set_on(HANDLER, "_timeNewMeteoSet", enc::double(now));
    set_selected(game, index);
}

/// LaunchSpecificMeteoEvent's reference client: the player nearest the given position, else a
/// random player and where they are. The game picks among its players, the host's included; with
/// nobody online the server stands in for the host (client 0) and the position stays.
fn pick_reference(game: &mut Game, position: Option<[f32; 3]>) -> (u64, Option<[f32; 3]>) {
    if let Some(p) = position {
        return (game.nearest_client(p), Some(p));
    }
    // The game's player list: players who finished joining.
    let players: Vec<(u64, u64)> = game.clients.values().filter(|c| c.ready.is_some()).filter_map(|c| Some((c.id, c.player?))).collect();
    if players.is_empty() {
        return (0, None);
    }
    let (client, player) = players[game.rng.range(0, players.len() as i32) as usize];
    (client, game.net.objects.get(&player).map(|o| o.position))
}

fn set_reference(game: &mut Game, client: u64, position: Option<[f32; 3]>) {
    if let Some(p) = position {
        game.set_on(HANDLER, "_asteroidReferencePosition", vec3(p));
    }
    game.set_on(HANDLER, "_asteroidReferenceClient", enc::ulong(client));
    game.ext.meteo.reference = (client, position);
}

/// AsteroidsHandler.SpawnAsteroid lets the reference client create each asteroid's resources,
/// or the host while that client is not connected (it left mid-event, or nobody else was online
/// and the launch picked the host). This server has no terrain to drop them, so while asteroids
/// fall without a connected reference, a connected player takes the host's part: the one
/// nearest the event's position, or (nobody was online at the launch) one picked as the launch
/// picks, around whom the event then falls as the game's launch would have it. Clients apply
/// the new reference from their next asteroid on.
fn hand_over_reference(game: &mut Game) {
    let (client, position) = game.ext.meteo.reference;
    if game.ext.meteo.asteroids.events.is_empty() || game.clients.get(&client).is_some_and(|c| c.player.is_some()) {
        return;
    }
    let (client, position) = pick_reference(game, position);
    if client != 0 {
        set_reference(game, client, position);
    }
}

/// _currentSeed.Value = seed, with the server's OnValueChanged (AsteroidsHandler.Seed, which only
/// takes while its random state is unset).
fn set_seed(game: &mut Game, seed: u32) {
    if game.ext.meteo.seed == seed {
        return;
    }
    game.ext.meteo.seed = seed;
    game.set_on(HANDLER, "_currentSeed", uint(seed));
    if game.ext.meteo.asteroids.state == 0 {
        game.ext.meteo.asteroids.state = seed;
    }
}

/// _selectedDataMeteoEventIndex.Value = index, with the server's OnValueChanged: the event's
/// asteroids start, or stop with the event.
fn set_selected(game: &mut Game, current: i32) {
    let previous = game.ext.meteo.selected;
    if previous == current {
        return;
    }
    game.ext.meteo.selected = current;
    game.set_on(HANDLER, "_selectedDataMeteoEventIndex", enc::int(current));
    let now = game.time;
    if current != -1 {
        let data = game.data.asset(&game.ext.meteo.events[current as usize]).and_then(|e| reference(e, "asteroidEventData")).and_then(|a| game.data.asset(a)).cloned();
        let m = &mut game.ext.meteo;
        m.selected_asteroids = data.map(|d| m.asteroids.add(&d, now));
    }
    if current == -1 && previous >= 0 {
        let m = &mut game.ext.meteo;
        if let Some(id) = m.selected_asteroids.take() {
            m.asteroids.remove(id);
        }
    }
}

/// UpdateMeteo, server side: ends the event once its duration is over (GetMeteoLerpValue) and
/// publishes the asteroids' random state while they fall. Every updateTimer seconds between
/// events, every frame during one.
fn update_meteo(game: &mut Game) {
    if game.time < game.ext.meteo.next_update {
        return;
    }
    let m = &game.ext.meteo;
    if m.selected != -1 {
        let duration = game.data.asset(&m.events[m.selected as usize]).map(|e| num(e, "duration")).unwrap_or(0.0);
        if (game.time.secs() - m.time_set) as f32 >= duration {
            set_selected(game, -1);
        }
    }
    if !game.ext.meteo.asteroids.events.is_empty() {
        set_seed(game, game.ext.meteo.asteroids.state);
    }
    let timer = num(singleton(game, HANDLER), "updateTimer");
    let m = &mut game.ext.meteo;
    m.next_update = if m.selected == -1 { game.time + if timer == 0.0 { 0.1 } else { timer } } else { game.time };
}

/// WorldUnitsHandler.IsWorldValuesAreBetweenStages on the current planet (TerraformStage keys).
pub fn between_stages(game: &Game, start: Option<&str>, stop: Option<&str>) -> bool {
    let (start, stop) = (start.and_then(|s| game.data.asset(s)), stop.and_then(|s| game.data.asset(s)));
    let Some(stage) = start.or(stop) else { return true };
    let unit = stage["unitType"].as_str().unwrap_or("");
    let value = if unit == "SystemTerraformation" {
        system_terraformation(game)
    } else {
        let Some(kind) = Kind::from_name(unit) else { return false };
        if game.units.planet(game.planet_hash()).is_none() {
            return false;
        }
        crate::units::value(game, kind, 0)
    };
    if value < 0.0 {
        return true;
    }
    let at = |s: &Value| s["startValue"].as_f64().unwrap_or(0.0);
    start.is_none_or(|s| value >= at(s)) && stop.is_none_or(|s| value < at(s))
}

/// WorldUnitSystemTerraformation's value: the planets' terraformation multiplied together.
fn system_terraformation(game: &Game) -> f64 {
    let mut total = 0.0f64;
    for p in &game.units.planets {
        let v = p.get(Kind::Terraformation).total;
        total = if total < f64::EPSILON { v } else { total * v.max(1.0) };
    }
    total
}

/// TerraformStagesHandler.GetCurrentGlobalStage: the TerraformStage key of the highest stage the
/// current planet's terraformation has reached.
pub fn current_global_stage(game: &Game) -> Option<String> {
    let value = crate::units::value(game, Kind::Terraformation, 0);
    let mut best: Option<(f64, String)> = None;
    for key in refs(&planet_data(game)["allTerraStages"]) {
        let start = game.data.asset(&key).and_then(|s| s["startValue"].as_f64()).unwrap_or(0.0);
        if value >= start && start >= best.as_ref().map_or(0.0, |b| b.0) {
            best = Some((start, key));
        }
    }
    best.map(|b| b.1)
}

/// The current planet's terraformation stages for the admin page: each stage's key, unit and
/// start value in order, and the stage reached (current_global_stage).
pub fn stages(game: &Game) -> Value {
    let mut list: Vec<Value> = refs(&planet_data(game)["allTerraStages"])
        .into_iter()
        .filter_map(|key| {
            let s = game.data.asset(&key)?;
            Some(serde_json::json!({"key": key, "unit": s["unitType"], "start": s["startValue"]}))
        })
        .collect();
    list.sort_by(|a, b| a["start"].as_f64().partial_cmp(&b["start"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    serde_json::json!({"current": current_global_stage(game), "list": list})
}

// ---- AsteroidsHandler (server) ---------------------------------------------------------------

/// The server's AsteroidsHandler: its Unity.Mathematics.Random state (MeteoHandler publishes it
/// as _currentSeed while asteroid events run) and the asteroid events it spawns for.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Asteroids {
    state: u32,
    events: Vec<AsteroidEvent>,
    next_check: GameTime,
    next_id: u32,
}

/// An instance of AsteroidEventData with its counters.
#[derive(serde::Serialize, serde::Deserialize)]
struct AsteroidEvent {
    id: u32,
    every: f32,
    max_simultaneous: i32,
    max_total: i32,
    existing: i32,
    total: i32,
    last_sent: GameTime,
    /// When the asteroids in flight land (Asteroid.OnImpact lowers the existing count).
    landings: Vec<GameTime>,
}

/// Seconds from an asteroid's spawn to its impact. Approximation: Asteroid's speed ranges and the
/// spawn boxes are not in the game data dump, so flights are taken to last this long (the real
/// server spawned Osmium asteroids, one in flight at a time, every 14 to 22 s: 18 s on average).
const FLIGHT_SECONDS: f32 = 18.0;

impl Asteroids {
    /// AddAsteroidEvent(Instantiate(asteroidEventData)).
    fn add(&mut self, data: &Value, now: GameTime) -> u32 {
        self.next_id += 1;
        let max_total = data["maxAsteroidsTotal"].as_i64().unwrap_or(0) as i32;
        self.events.push(AsteroidEvent {
            id: self.next_id,
            every: num(data, "spawnOneEvery"),
            max_simultaneous: data["maxAsteroidsSimultaneous"].as_i64().unwrap_or(0) as i32,
            max_total: if max_total != 0 { max_total } else { 999_999_999 },
            existing: 0,
            total: 0,
            last_sent: now,
            landings: Vec::new(),
        });
        self.next_id
    }

    /// RemoveAsteroidEvent: the random state resets once no event is left.
    fn remove(&mut self, id: u32) {
        self.events.retain(|e| e.id != id);
        if self.events.is_empty() {
            self.state = 0;
        }
    }

    /// Unity.Mathematics.Random.NextState (xorshift32).
    fn next_state(&mut self) -> u32 {
        let t = self.state;
        self.state ^= self.state << 13;
        self.state ^= self.state >> 17;
        self.state ^= self.state << 5;
        t
    }

    /// SpawnAsteroid. Approximation: the target point's NextFloat2Direction is drawn once (the
    /// game draws again while the point is outside every AsteroidZone, whose bounds we lack).
    fn spawn(&mut self, i: usize, now: GameTime) {
        let e = &self.events[i];
        if e.existing >= e.max_simultaneous || e.total >= e.max_total {
            return;
        }
        self.next_state();
        let e = &mut self.events[i];
        e.existing += 1;
        e.total += 1;
        e.landings.push(now + FLIGHT_SECONDS);
    }
}

/// AsteroidsHandler.SpawningRoutine: every 0.5 s while idle, every frame while events run.
fn asteroids_tick(game: &mut Game) {
    let now = game.time;
    let a = &mut game.ext.meteo.asteroids;
    if now < a.next_check {
        return;
    }
    for i in 0..a.events.len() {
        let e = &mut a.events[i];
        let landed = e.landings.iter().filter(|t| **t <= now).count();
        e.landings.retain(|t| *t > now);
        e.existing -= landed as i32;
        if e.last_sent < now - e.every {
            e.last_sent = now;
            a.spawn(i, now);
        }
    }
    a.next_check = if a.events.is_empty() { now + 0.5 } else { now };
}

// ---- EnvironmentDayNightCycle ----------------------------------------------------------------

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct DayNight {
    full_day_stay: f32,
    speed: f32,
    every: f32,
    next: GameTime,
    full_phase_reached: GameTime,
    pong: f32,
    value: f32,
}

/// Mathf.PingPong.
fn ping_pong(t: f32, length: f32) -> f32 {
    let l2 = length * 2.0;
    let r = (t - (t / l2).floor() * l2).clamp(0.0, l2);
    length - (r - length).abs()
}

impl DayNight {
    /// One SetDayNightLerpValue step: after fullDayStayTime minutes of day the value ping-pongs
    /// to 100 (night) and back; back under 1 it snaps to 0 and the day starts over.
    fn step(&mut self, now: GameTime) {
        if now > self.full_phase_reached + self.full_day_stay * 60.0 {
            self.pong += self.speed;
            let previous = self.value;
            self.value = ping_pong(self.pong, 100.0);
            if self.value < 1.0 && previous > self.value {
                self.full_phase_reached = now;
                self.pong = 0.0;
                self.value = 0.0;
            }
        }
    }
}

fn day_night(game: &mut Game) {
    let d = &mut game.ext.meteo.day;
    if game.time < d.next {
        return;
    }
    // WaitForSeconds resumes on the first frame after the wait: at the real server's frame rate
    // that is 0.35 ms late, our 5 ms ticks would make it 3 ms. Keep to the schedule instead.
    d.next = (d.next + d.every).max(game.time);
    d.step(game.time);
    let value = d.value;
    game.set_on(DAY_NIGHT, "_dayNightLerpValue", enc::float(value));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::save::SaveFile;

    /// A game on an empty Prime save.
    pub(crate) fn game() -> Option<Game> {
        let mut save = SaveFile::empty();
        save.settings = serde_json::json!({"planetId": "Prime", "modifierMeteoOccurence": 1.0});
        save.planets = vec![serde_json::json!({"planetId": "Prime"})];
        crate::game::test_game(save)
    }

    pub(crate) fn var(game: &Game, class: &str, name: &str) -> Vec<u8> {
        let (object, _) = game.net.find_class(class).unwrap();
        match game.net.get(object, class, name) {
            Some(crate::net::VarValue::Plain(b)) => b.clone(),
            _ => panic!("{class}.{name}"),
        }
    }

    /// Clients launch a meteo event by its index in the planet's list (an index outside it is
    /// ignored, where the game would stop its meteo) and a story event by its id's hash.
    #[test]
    fn clients_launch_meteo_and_story_events() {
        use crate::testkit::*;
        let mut save = crate::game::tests::save();
        save.settings["modifierMeteoOccurence"] = serde_json::json!(1.0);
        let Some(mut g) = crate::game::test_game(save) else { return };
        join(&mut g, 1, "Ann");
        let selected = |g: &Game| tests::var(g, HANDLER, "_selectedDataMeteoEventIndex");
        let events = g.ext.meteo.events.len() as i32;
        let index = (0..events).find(|i| !g.ext.meteo.events[*i as usize].is_empty()).expect("a meteo event");
        for bad in [-1, events, i32::MAX] {
            send_scene(&mut g, 1, "MeteoHandler.LaunchSpecificMeteoEventServerRpc", &[serde_json::json!(bad)]);
            assert_eq!(g.ext.meteo.selected, -1, "{bad}");
        }
        send_scene(&mut g, 1, "MeteoHandler.LaunchSpecificMeteoEventServerRpc", &[serde_json::json!(index)]);
        assert_eq!((g.ext.meteo.selected, selected(&g)), (index, enc::int(index)));

        let hash = crate::data::stable_hash("StoryEvent-Mont-Open");
        g.net.out.clear();
        send_scene(&mut g, 1, "StoryEventsHandler.LaunchSpecificStoryEventEventServerRpc", &[serde_json::json!(hash)]);
        send_scene(&mut g, 1, "StoryEventsHandler.LaunchSpecificStoryEventEventServerRpc", &[serde_json::json!(12345)]);
        assert_eq!(sent(&g, "StoryEventsHandler.AnEventHasBeenTriggeredClientRpc").into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(crate::net::Target::All, vec![serde_json::json!(hash)])]);
        assert!(g.snapshot().story_events.contains(&serde_json::json!({"stringId": "StoryEvent-Mont-Open"})));
    }

    /// The meteo coroutine: its first try a minute in launches the queued event (a rocket's,
    /// a story's) before any random one; the event ends after its duration; later tries launch
    /// a random event of the planet's own list as often as the rules' chance says (never at 0).
    #[test]
    fn the_meteo_loop_launches_queued_events_first_then_random_ones() {
        let Some(mut g) = game() else { return };
        let run = |g: &mut Game, until: f32| {
            while g.time.f32() < until {
                g.time += 0.1;
                tick(g);
            }
        };
        let queued = "MeteoEventData:MeteoEvent-BasicOresFullRed";
        queue_meteo_event(&mut g, queued, Some([5.0, 6.0, 7.0]));
        g.rules.meteo.event_chance = 0.0;
        run(&mut g, 59.0);
        assert_eq!(g.ext.meteo.selected, -1, "nothing before the first try");
        run(&mut g, 61.0);
        assert_eq!(g.ext.meteo.events[g.ext.meteo.selected as usize], queued);
        assert!(g.ext.meteo.queue.is_empty());
        assert_eq!(var(&g, HANDLER, "_asteroidReferencePosition"), vec3([5.0, 6.0, 7.0]));
        let duration = num(g.data.asset(queued).unwrap(), "duration");
        run(&mut g, 61.0 + duration + 1.0);
        assert_eq!(g.ext.meteo.selected, -1, "over after its duration");
        assert!(g.ext.meteo.asteroids.events.is_empty(), "its asteroids stop with it");
        // Chance 0: no random event, however long.
        let next = g.ext.meteo.next_try.f32();
        let every = g.ext.meteo.try_every;
        run(&mut g, next + 3.0 * every);
        assert_eq!(g.ext.meteo.selected, -1);
        // A chance that cannot miss: the next try launches one of the planet's own events.
        g.rules.meteo.event_chance = 1.0e9;
        let next = g.ext.meteo.next_try.f32();
        run(&mut g, next + 0.5);
        assert!((0..g.ext.meteo.original_count as i32).contains(&g.ext.meteo.selected), "{}", g.ext.meteo.selected);
    }

    #[test]
    fn day_night_matches_the_real_server() {
        // _dayNightLerpValue as the real server sent it, night step 1 and the 0.35 steps after.
        let mut d = DayNight { full_day_stay: 16.0, speed: 0.35, full_phase_reached: GameTime::at(-99.0), ..Default::default() };
        d.step(GameTime::at(10.0));
        assert_eq!(d.value, 0.0, "full day for the first 861 s");
        let mut seen = Vec::new();
        for n in 0..700 {
            d.step(GameTime::at(f64::from(900.0 + n as f32 * 0.5)));
            seen.push(d.value.to_bits());
        }
        assert_eq!(&seen[..3], &[0x3eb33300, 0x3f333300, 0x3f866680]);
        assert!(seen.contains(&u32::from_le_bytes([0xbc, 0xcc, 0x87, 0x42])), "67.89987 at step 194");
        // Up to 100, back down, then the day starts over at 0.
        let top = seen.iter().map(|b| f32::from_bits(*b)).fold(0.0, f32::max);
        assert!(top > 99.0 && top <= 100.0);
        let reset = seen.iter().rposition(|b| *b == 0).unwrap();
        assert!(reset > 560 && d.full_phase_reached.f32() > 1100.0, "{reset} {}", d.full_phase_reached);
    }

    #[test]
    fn day_night_keeps_its_half_second_schedule() {
        let Some(mut g) = game() else { return };
        g.ext.meteo.day.full_day_stay = 0.0;
        for t in 0..2000 {
            g.time = GameTime::at(f64::from(t as f32 * 0.0051));
            day_night(&mut g);
        }
        // 10.2 s of 5.1 ms ticks: steps at 0, 0.5, ..., 10.0, none lost to tick rounding.
        assert_eq!((g.ext.meteo.day.pong / 0.35).round(), 21.0);
        assert_eq!(var(&g, DAY_NIGHT, "_dayNightLerpValue"), enc::float(g.ext.meteo.day.value));
    }

    /// The cycle's half-second steps do not slow down with the server's age: 21 in 10.2 s at the
    /// first day and at the thousandth (the float's 1 s resolution would have made them whole seconds).
    #[test]
    fn day_night_keeps_its_schedule_at_any_age_of_the_server() {
        for days in [0.0, 1.0, 7.0, 25.0, 100.0, 400.0, 1000.0] {
            let Some(mut g) = game() else { return };
            let base = days * 86_400.0;
            g.ext.meteo.day.full_day_stay = 0.0;
            g.time = GameTime::at(base);
            g.ext.meteo.day.next = g.time;
            g.ext.meteo.day.full_phase_reached = GameTime::at(base - 99.0);
            for t in 0..2000 {
                g.time = GameTime::at(base + f64::from(t) * 0.0051);
                day_night(&mut g);
            }
            assert_eq!((g.ext.meteo.day.pong / 0.35).round(), 21.0, "{days} days");
        }
    }

    #[test]
    fn asteroid_random_is_xorshift32() {
        // Unity.Mathematics.Random(1).NextUInt() sequence.
        let mut a = Asteroids { state: 1, ..Default::default() };
        let got: Vec<u32> = (0..3).map(|_| a.next_state()).collect();
        assert_eq!(got, [1, 270369, 67634689]);
    }

    #[test]
    fn meteo_events_follow_the_planet_lists() {
        let Some(mut g) = game() else { return };
        assert_eq!(g.ext.meteo.events.len(), 16 + 4 * 2 + 4);
        assert_eq!(g.ext.meteo.original_count, 24);
        assert_eq!(g.ext.meteo.events[16], "MeteoEventData:MeteoEvent-BasicOresFullRed");
        assert_eq!(var(&g, HANDLER, "_selectedDataMeteoEventIndex"), enc::int(-1));
        assert!(g.save.messages.iter().any(|m| m["stringId"] == "Message_YouAreAConvict"), "default message");
        // A launched event sets the seed, time and index, starts its asteroids and ends on time.
        g.time = crate::clock::GameTime::at(100.0);
        launch_specific(&mut g, 16, Some([1.0, 2.0, 3.0]));
        assert_eq!(g.ext.meteo.selected, 16);
        assert_eq!(var(&g, HANDLER, "_asteroidReferencePosition"), vec3([1.0, 2.0, 3.0]));
        assert_eq!(var(&g, HANDLER, "_timeNewMeteoSet"), enc::double(100.0));
        let seed = g.ext.meteo.seed;
        assert!((1..999_999).contains(&seed));
        assert_eq!(g.ext.meteo.asteroids.state, seed);
        // AsteroidEventSmalls: one every 3 s, the state published as _currentSeed.
        for t in 0..40 {
            g.time = GameTime::at(f64::from(100.0 + t as f32 * 0.1));
            asteroids_tick(&mut g);
            update_meteo(&mut g);
        }
        assert_ne!(g.ext.meteo.seed, seed, "a spawn moved the random state on");
        assert_eq!(var(&g, HANDLER, "_currentSeed"), uint(g.ext.meteo.asteroids.state));
        g.time = GameTime::at(180.0);
        update_meteo(&mut g);
        assert_eq!(g.ext.meteo.selected, -1);
        assert_eq!(g.ext.meteo.asteroids.state, 0, "the random state resets with the last asteroid event");
    }

    #[test]
    fn a_connected_player_takes_over_the_reference() {
        // A world players can join (an escape pod to spawn at).
        let mut save = crate::game::tests::save();
        save.settings["modifierMeteoOccurence"] = serde_json::json!(1.0);
        let Some(mut g) = crate::game::test_game(save) else { return };
        let join = |g: &mut Game, client: u64, at: [f32; 3]| {
            let player = crate::game::tests::join(g, client, &format!("P{client}"));
            g.net.objects.get_mut(&player).unwrap().position = at;
        };
        let reference = |g: &Game| (var(g, HANDLER, "_asteroidReferenceClient"), var(g, HANDLER, "_asteroidReferencePosition"));
        launch_specific(&mut g, 16, None);
        hand_over_reference(&mut g);
        assert_eq!(reference(&g), (enc::ulong(0), vec3([0.0; 3])), "nobody online");
        join(&mut g, 7, [10.0, 20.0, 30.0]);
        hand_over_reference(&mut g);
        assert_eq!(reference(&g), (enc::ulong(7), vec3([10.0, 20.0, 30.0])), "the joiner, and the event falls around them");
        // The reference leaves: the player nearest the event's position takes over.
        join(&mut g, 8, [500.0, 20.0, 30.0]);
        join(&mut g, 9, [15.0, 20.0, 30.0]);
        g.disconnect(7);
        hand_over_reference(&mut g);
        assert_eq!(reference(&g), (enc::ulong(9), vec3([10.0, 20.0, 30.0])));
        // Once the asteroids stop, the reference stays.
        set_selected(&mut g, -1);
        g.disconnect(9);
        hand_over_reference(&mut g);
        assert_eq!(reference(&g).0, enc::ulong(9));
    }

    #[test]
    fn rockets_queue_their_space_event() {
        let Some(mut g) = game() else { return };
        let rocket = g.data.group("RocketOxygen1").unwrap();
        send_something_in_space(&mut g, rocket, [5.0, 6.0, 7.0]);
        assert_eq!(g.ext.meteo.queue, vec![("MeteoEventData:SpaceEvent-MossSpread".to_string(), Some([5.0, 6.0, 7.0]))]);
        try_to_launch(&mut g);
        assert_eq!(g.ext.meteo.selected, 26, "the first matching entry of _meteoEvents");
        assert!(g.ext.meteo.queue.is_empty());
    }

    #[test]
    fn travel_reloads_the_planet_lists() {
        let Some(mut g) = game() else { return };
        launch_specific(&mut g, 16, None);
        g.planet = g.net_data.planet_index("Aqualis").unwrap();
        tick(&mut g);
        assert_eq!(g.ext.meteo.selected, -1, "InitMeteoHandler stops the running event");
        assert_eq!(g.ext.meteo.original_count, 8 + 2);
        assert_eq!(g.ext.meteo.asteroids.state, 0);
        assert_eq!(g.ext.meteo.story.current_planet.len(), 7);
    }

    #[test]
    fn stages_bound_events() {
        let Some(g) = game() else { return };
        // A fresh planet is barren: Volcanic (Barren..Lakes) fits, RainHeavy (Rain..) does not.
        assert!(between_stages(&g, Some("TerraformStage:TerraformStage0-Barren"), Some("TerraformStage:TerraformStage5-Lakes")));
        assert!(!between_stages(&g, Some("TerraformStage:TerraformStage3-Rain"), None));
        assert!(between_stages(&g, None, None));
        assert_eq!(current_global_stage(&g).as_deref(), Some("TerraformStage:TerraformStage0-Barren"));
    }
}
