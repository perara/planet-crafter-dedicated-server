//! Where the subsystems plug into the core. Each subsystem module (machines, meteo,
//! logistics, rockets, planets, animals) has the same entry points:
//!
//! - init(game): once, after the save is loaded and every placed object spawned
//! - tick(game): every server tick (about 5 ms); keep your own timers against game.time
//! - rpc(game, client, object, behaviour, name, args) -> bool: a client RPC; true when handled
//! - on_instantiated(game, wo): a world object got its network object (load, build, drop)
//! - on_destroyed(game, wo): a world object was destroyed (after removal from game.wos)
//! - inventory_changed(game, inventory, wo, added): Inventory.PropagateModification

use crate::game::{Args, Game};
use crate::model::WorldObject;

pub fn init(game: &mut Game) {
    crate::machines::init(game);
    crate::meteo::init_late(game);
    crate::logistics::init_late(game);
    crate::rockets::init(game);
    crate::planets::init(game);
    crate::animals::init(game);
}

pub fn tick(game: &mut Game) {
    section!("colors", crate::handlers::default_colors_tick(game));
    section!("chathooks", crate::chathooks::tick(game));
    section!("machines", crate::machines::tick(game));
    section!("meteo", crate::meteo::tick(game));
    section!("logistics", crate::logistics::tick(game));
    section!("rockets", crate::rockets::tick(game));
    section!("planets", crate::planets::tick(game));
    section!("animals", crate::animals::tick(game));
    section!("events", crate::events::tick(game));
}

pub fn rpc(game: &mut Game, client: u64, object: u64, behaviour: u16, name: &str, a: &Args) -> bool {
    crate::machines::rpc(game, client, object, behaviour, name, a)
        || crate::meteo::rpc(game, client, object, behaviour, name, a)
        || crate::logistics::rpc(game, client, object, behaviour, name, a)
        || crate::rockets::rpc(game, client, object, behaviour, name, a)
        || crate::planets::rpc(game, client, object, behaviour, name, a)
        || crate::animals::rpc(game, client, object, behaviour, name, a)
}

pub fn on_instantiated(game: &mut Game, wo: i32) {
    crate::handlers::first_item_linked_instantiated(game, wo);
    crate::handlers::default_color_instantiated(game, wo);
    crate::machines::on_instantiated(game, wo);
    crate::logistics::on_instantiated(game, wo);
    crate::rockets::on_instantiated(game, wo);
    crate::planets::on_instantiated(game, wo);
    crate::animals::on_instantiated(game, wo);
    crate::events::object_placed(game, wo);
}

pub fn on_destroyed(game: &mut Game, wo: &WorldObject) {
    crate::handlers::first_item_linked_destroyed(game, wo);
    crate::machines::on_destroyed(game, wo);
    crate::logistics::on_destroyed(game, wo);
    crate::rockets::on_destroyed(game, wo);
    crate::planets::on_destroyed(game, wo);
    crate::animals::on_destroyed(game, wo);
    crate::events::object_destroyed(game, wo);
}

pub fn inventory_changed(game: &mut Game, inventory: i32, wo: i32, added: bool) {
    crate::handlers::first_item_linked_changed(game, inventory);
    crate::machines::inventory_changed(game, inventory, wo, added);
    crate::logistics::inventory_changed(game, inventory, wo, added);
    crate::rockets::inventory_changed(game, inventory, wo, added);
    crate::planets::inventory_changed(game, inventory, wo, added);
    crate::animals::inventory_changed(game, inventory, wo, added);
}
