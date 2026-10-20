//! What happens when the game's code panics (the policy, in one place). A panic is never allowed to
//! end the server for everyone, and never to replace the last whole save with a half-changed world:
//!
//! - one client's share of the game (`guarded`): that client is kicked, the bank goes on;
//! - the admin's, a chat hook's or a plugin's work (`contained`): logged, the bank goes on;
//! - either way the world is `taint`ed: the next save first keeps the file on disk as
//!   <save>.before-panic (`take_taint`, asked by persist.rs), once.
//! - the tick itself is not contained (`ticked`): a panic there ends the bank after a last save
//!   (serve.rs).

use crate::game::Game;

/// One client's share of the game. A panic there (the hook printed it) kicks that client instead
/// of ending the server for everyone (a client must not be able to restart the world by sending
/// something that panics). What the work had changed by then stays changed: the world may hold
/// the half of it. So it is not saved at once, which would replace the last consistent save on
/// disk with it at the moment it is least to be trusted; the next save, whenever it comes,
/// first keeps that file as <save>.before-panic (taint).
pub(super) fn guarded(game: &mut Game, client: u64, save_path: &str, work: impl FnOnce(&mut Game)) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(game))).is_err() {
        taint();
        error!("client {client}: the server panicked on it; kicked. The world may hold half of what it did: the save on disk is left as it is, and the next save keeps it as {save_path}.before-panic first");
        game.kick(client, "Server error");
    }
}

thread_local! {
    /// Work panicked since the last save: the world may be half-changed, the file on disk is the
    /// last whole one.
    static TAINTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn taint() {
    TAINTED.set(true);
}

/// One tick of the game. A panic in it is not contained: the whole world may be half-updated, so the
/// bank keeps what there is (a last save) and ends, and the front starts it again from the save.
pub(super) fn ticked(game: &mut Game) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| game.tick())).is_ok()
}

/// Whether work panicked since the last save, and forgets it: the save about to be written keeps
/// the file on disk first when it did (persist).
pub(super) fn take_taint() -> bool {
    TAINTED.take()
}

/// The admin's or a chat hook's work: a panic there (the hook printed it) is logged and the bank
/// goes on (there is no client to kick, and the world is the one the clients have). Some(what
/// the work returned) when it finished.
pub(crate) fn contained<T>(what: &str, work: impl FnOnce() -> T) -> Option<T> {
    let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    if done.is_err() {
        taint();
        error!("{what}: the server panicked on it; going on (the next save keeps the one on disk as .before-panic first)");
    }
    done.ok()
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::game;
    use crate::save::SaveFile;

    /// A panic in one client's work kicks it and goes on; the world, which may hold half of what the
    /// work did, is not saved at once over the last whole save: that file is kept as .before-panic
    /// by the next save, once.
    #[test]
    fn a_panicking_client_is_kicked_and_the_save_on_disk_is_kept_not_replaced_by_its_half_done_work() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-guarded-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        let before_path = format!("{path}.before-panic");
        crate::bank::persist::persist(&mut game, &path).unwrap();
        let whole = std::fs::read(&path).unwrap();
        assert!(!std::path::Path::new(&before_path).exists(), "no panic yet, nothing kept");
        // The work changes the world and panics half way.
        let objects = game.wos.len();
        guarded(&mut game, 7, &path, |g| {
            let iron = g.data.group("Iron").unwrap();
            g.create_world_object(iron, 0, true);
            panic!("test panic")
        });
        assert_eq!(game.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [7]);
        assert_eq!(game.wos.len(), objects + 1, "the world holds what the work did before it panicked");
        assert_eq!(std::fs::read(&path).unwrap(), whole, "the half-done world was not written over the whole save");
        // The next save keeps the whole one first, and writes the world as it is.
        crate::bank::persist::persist(&mut game, &path).unwrap();
        assert_eq!(std::fs::read(&before_path).unwrap(), whole);
        assert_ne!(std::fs::read(&path).unwrap(), whole);
        assert_eq!(SaveFile::load(&path).unwrap().objects.len(), objects + 1);
        // Once per panic: a later save does not replace the kept one with a save made after it.
        crate::bank::persist::persist(&mut game, &path).unwrap();
        assert_eq!(std::fs::read(&before_path).unwrap(), whole);
        // An admin or hook panic taints the same way.
        contained("a test", || panic!("again"));
        crate::bank::persist::persist(&mut game, &path).unwrap();
        assert_ne!(std::fs::read(&before_path).unwrap(), whole, "the save before the second panic is kept now");
        std::fs::remove_dir_all(dir).unwrap();
    }

}
