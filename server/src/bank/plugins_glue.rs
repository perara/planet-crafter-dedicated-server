//! The bank's hooks into the plugin host (plugins/), in one place: where their folder is, reading
//! and compiling them before the HELLO, starting them after a restore, looking at the folder
//! between ticks, their state into the snapshot and into the save, and the world-saved event. The
//! host is never called from anywhere else in the bank; a panic in a plugin's turn is contained
//! (panic.rs) and the world goes on.

use super::panic::contained;
use super::start::rules_path;
use crate::game::Game;
use crate::{events, plugins};

/// The plugins' folder: beside the rules file (run/plugins for an installation, next to the world for a
/// server started by hand).
fn plugins_dir(save_path: &str) -> std::path::PathBuf {
    std::path::Path::new(&rules_path(save_path)).with_file_name("plugins")
}

/// The world's name: its save's file name without the extension.
fn world_name(save_path: &str) -> String {
    std::path::Path::new(save_path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "world".into())
}

/// Reads, validates and compiles the world's plugins, before the HELLO: after a restore only their
/// start is left, so a hot update stops the world for that, not for this.
pub(super) fn prepare(save_path: &str) -> plugins::Host {
    plugins::prepare(&plugins_dir(save_path), &world_name(save_path))
}

/// Starts the prepared plugins in the restored or loaded game.
pub(super) fn attach(game: &mut Game, prepared: plugins::Host, flipped: bool) {
    plugins::attach(game, prepared, flipped);
}

/// Plugins appear, change and go while the world runs: the folder is looked at between ticks.
pub(super) fn poll(game: &mut Game) {
    contained("the plugin folder's scan", || plugins::poll(game));
}

/// The plugins' states go into the snapshot of a flip (within their budget altogether).
pub(super) fn checkpoint_for_flip(game: &mut Game) {
    contained("the plugins' state", || plugins::checkpoint(game, Some(plugins::limits::FLIP_BUDGET)));
}

/// A save ended: the plugins hear of it and keep their state beside the world's.
pub(super) fn world_saved(game: &mut Game, ok: bool) {
    contained("the plugins' state", || {
        events::world_saved(game, ok);
        plugins::saved(game);
    });
}
