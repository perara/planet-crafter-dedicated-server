//! Global admin stop and native signs bound to one nearby station. Only the cached indexes
//! are new runtime fields; home stations use optional JSON in the existing save world header.
use super::*;

const HOME_KEY: &str = "pcdsDroneStations";
const SIGN_DISTANCE_SQ: f32 = 4.0 * 4.0;
const ALL_TIERS: u8 = 7;

#[derive(Default)]
pub(super) struct StationControls {
    recalls: HashMap<i32, u8>,
    signs: HashSet<i32>,
    homes: HashMap<i32, i32>,
    homes_dirty: bool,
}

#[derive(serde::Serialize)]
pub struct StationRecall {
    pub station: i32,
    pub tiers: Vec<u8>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlStatus {
    pub paused: bool,
    pub flying: usize,
    pub docked: usize,
    pub returning: usize,
    pub station_recalls: Vec<StationRecall>,
}

pub fn control_status(game: &Game) -> ControlStatus {
    let l = &game.ext.logistics;
    let docked = l.stations.iter().filter_map(|(_, station)| {
        game.inventories.get(&station.inventory).map(|inv| inv.items.iter().filter(|id| {
            game.wos.get(id).is_some_and(|w| station.groups.contains(&w.group))
        }).count())
    }).sum();
    let returning = l.fleet.iter().filter(|id| l.drones.get(*id).is_some_and(|d| d.task.is_none())).count();
    let mut station_recalls: Vec<_> = l.control.recalls.iter().map(|(&station, &mask)| StationRecall {
        station, tiers: (1..=3).filter(|tier| mask & (1 << (tier - 1)) != 0).collect(),
    }).collect();
    station_recalls.sort_unstable_by_key(|r| r.station);
    ControlStatus { paused: l.paused, flying: l.fleet.len(), docked, returning, station_recalls }
}

/// This remains the manual, global stop. A sign never clears an admin's stop.
pub fn set_paused(game: &mut Game, paused: bool) {
    if game.ext.logistics.paused != paused || game.save.world["logisticsPaused"].as_bool() != Some(paused) {
        game.ext.logistics.paused = paused;
        game.save.world["logisticsPaused"] = json!(paused);
        game.save_requested = true;
    }
}

fn command(text: &str) -> Option<u8> {
    let mut words = text.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("recall") { return None; }
    let mask = match words.next()?.to_ascii_lowercase().as_str() {
        "1" => 1, "2" => 2, "3" => 4, "all" => ALL_TIERS, _ => return None,
    };
    words.next().is_none().then_some(mask)
}

fn refresh_signs(game: &mut Game) -> bool {
    let mut recalls = HashMap::new();
    let mut signs = HashSet::new();
    for id in &game.constructed {
        let Some(sign) = game.wos.get(id).filter(|w| game.data.groups[w.group].id == "Sign") else { continue };
        let Some(mask) = command(&sign.text) else { continue };
        let closest = game.ext.logistics.stations.iter().filter(|(_, s)| s.planet == sign.planet_hash())
            .filter_map(|(id, _)| {
                let distance = motion::distance_sq(sign.position, station_position(game, *id));
                (distance <= SIGN_DISTANCE_SQ).then_some((*id, distance))
            }).min_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        if let Some((station, _)) = closest {
            signs.insert(*id);
            *recalls.entry(station).or_insert(0) |= mask;
        }
    }
    let changed = game.ext.logistics.control.recalls != recalls || game.ext.logistics.control.signs != signs;
    game.ext.logistics.control.recalls = recalls;
    game.ext.logistics.control.signs = signs;
    changed
}

fn valid_home(game: &Game, drone: i32, station: i32) -> bool {
    game.ext.logistics.stations.get(&station).is_some_and(|s| {
        game.wos.get(&drone).is_some_and(|w| w.planet_hash() == s.planet && s.groups.contains(&w.group))
    })
}

/// Older saves have no launch history. Use the last return station if known, otherwise the
/// closest compatible station, once; normal launches and docking keep the association exact.
fn legacy_home(game: &Game, drone: i32) -> Option<i32> {
    if let Some(station) = game.ext.logistics.drones.get(&drone).and_then(|d| d.station).filter(|s| valid_home(game, drone, *s)) {
        return Some(station);
    }
    let position = game.ext.logistics.drones.get(&drone)?.position;
    game.ext.logistics.stations.iter().filter(|(id, _)| valid_home(game, drone, **id))
        .map(|(id, _)| (*id, motion::distance_sq(position, station_position(game, *id))))
        .min_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0))).map(|(id, _)| id)
}

fn associate_older_drones(game: &mut Game) {
    // Cold-loaded drones are instantiated before deferred CheckBusy adds them to fleet.
    let missing: Vec<_> = game.ext.logistics.drones.iter().filter(|(d, dr)| dr.net.is_some() && !game.ext.logistics.control.homes.contains_key(*d)).map(|(d, _)| *d).collect();
    for drone in missing {
        if let Some(station) = legacy_home(game, drone) {
            game.ext.logistics.control.homes.insert(drone, station);
            game.ext.logistics.control.homes_dirty = true;
        }
    }
}

/// Rebuild skipped indexes after cold load or hot restore. Do not mutate serialized state:
/// handoff verification must still compare the exact snapshot received from an older release.
pub fn restore_controls(game: &mut Game) {
    let homes: HashMap<_, _> = game.save.world[HOME_KEY].as_object().into_iter().flatten().filter_map(|(d, s)| {
        let (drone, station) = (d.parse::<i32>().ok()?, i32::try_from(s.as_i64()?).ok()?);
        valid_home(game, drone, station).then_some((drone, station))
    }).collect();
    game.ext.logistics.control = StationControls { homes, ..Default::default() };
    associate_older_drones(game);
    refresh_signs(game);
}

pub(super) fn persist_older_homes(game: &mut Game) {
    if std::mem::take(&mut game.ext.logistics.control.homes_dirty) {
        game.save.world[HOME_KEY] = json!(game.ext.logistics.control.homes);
    }
}

pub(super) fn remember_station(game: &mut Game, drone: i32, station: i32) {
    game.ext.logistics.control.homes.insert(drone, station);
    if !game.save.world[HOME_KEY].is_object() { game.save.world[HOME_KEY] = json!({}); }
    game.save.world[HOME_KEY][drone.to_string()] = json!(station);
}

pub(super) fn forget_drone(game: &mut Game, drone: i32) {
    game.ext.logistics.control.homes.remove(&drone);
    if let Some(homes) = game.save.world[HOME_KEY].as_object_mut() { homes.remove(&drone.to_string()); }
}

pub(super) fn station_removed(game: &mut Game, station: i32) {
    let drones: Vec<_> = game.ext.logistics.control.homes.iter().filter(|(_, s)| **s == station).map(|(d, _)| *d).collect();
    for drone in drones { forget_drone(game, drone); }
}

pub fn text_changed(game: &mut Game, wo: i32) {
    if game.wos.get(&wo).is_some_and(|w| game.data.groups[w.group].id == "Sign") {
        let was_control = game.ext.logistics.control.signs.contains(&wo);
        refresh_signs(game);
        if was_control || game.ext.logistics.control.signs.contains(&wo) {
            associate_older_drones(game);
            persist_older_homes(game);
            // An idle or held world still saves control edits and station associations.
            game.save_requested = true;
        }
    }
}

pub(super) fn object_changed(game: &mut Game, is_sign_or_station: bool) {
    if is_sign_or_station && game.ext.logistics.started {
        associate_older_drones(game);
        if refresh_signs(game) { game.save_requested = true; }
    }
}

pub(super) fn station_recall_matches(game: &Game, station: i32, group: usize) -> bool {
    let mask = game.ext.logistics.control.recalls.get(&station).copied().unwrap_or(0);
    let tier = match game.data.groups[group].id.as_str() { "Drone1" => 1, "Drone2" => 2, "Drone3" => 4, _ => 0 };
    mask == ALL_TIERS || mask & tier != 0
}

pub(super) fn recalled_station(game: &Game, drone: i32) -> Option<i32> {
    let station = game.ext.logistics.control.homes.get(&drone).copied().filter(|s| valid_home(game, drone, *s)).or_else(|| legacy_home(game, drone))?;
    station_recall_matches(game, station, game.wos.get(&drone)?.group).then_some(station)
}
