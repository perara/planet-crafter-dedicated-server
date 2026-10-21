//! World names: whatever the API or the command line is given, a name that is accepted is one
//! file name beside the save, never a path, never a settings file, never hidden.
use super::*;

pub fn run(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let save = "/srv/worlds/main.json";
    for accepted in [name(&text).ok(), name_arg(&text).ok(), arg(&text).ok()].into_iter().flatten() {
        assert!(valid(&accepted));
        assert!(!accepted.contains(['/', '\\', '\0']) && !accepted.starts_with('.') && accepted.len() <= 64, "{accepted:?}");
        let file = file(save, &accepted);
        assert_eq!(file.parent(), Some(Path::new("/srv/worlds")), "{file:?} is not beside the save");
        assert_eq!(stem(&file.to_string_lossy()), accepted);
        let kept = kept_stem(&accepted, "removed", SystemTime::UNIX_EPOCH);
        assert!(!kept.contains('/'));
        assert_eq!(backups(save).join(format!("{kept}.json")).parent(), Some(Path::new("/srv/worlds/backups")));
    }
    if let Ok(n) = name(&text) {
        assert!(n != "access" && n != "rules");
    }
    // The three agree on what a plain name is.
    if valid(&text) && !NOT_WORLDS.contains(&&*text) {
        assert_eq!(name(&text).as_deref(), Ok(&*text));
        assert_eq!(name_arg(&text).as_deref(), Ok(&*text));
    }
    let _ = stem(&text);
    let _ = dir(&text);
    let _ = snapshot_path(&text);
}
