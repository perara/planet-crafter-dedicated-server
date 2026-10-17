//! State of the ported subsystems that lives next to the core model.

use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct Ext {
    pub creative_mode: bool,
    pub gauges: crate::gauges::Gauges,
    pub status: crate::status::Status,
    /// InventoryDestroyWhenEmpty: inventory -> the world object to destroy when it empties.
    pub destroy_when_empty: HashMap<i32, i32>,
    /// Buildings being deconstructed (GroupNetworkBase._isDestroying).
    pub deconstructing: HashSet<i32>,
    pub machines: crate::machines::Machines,
    pub energy: crate::energy::Energy,
    pub water: crate::water::Water,
    pub meteo: crate::meteo::Meteo,
    pub logistics: crate::logistics::Logistics,
    pub rockets: crate::rockets::State,
    pub planets: crate::planets::State,
    pub animals: crate::animals::State,
    pub first_item_linked: crate::handlers::FirstItemLinked,
    pub default_colors: crate::handlers::DefaultColors,
}
