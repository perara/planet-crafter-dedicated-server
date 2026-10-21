//! A save of an older layout: the import of whatever bytes never panics, what it converts is a
//! save the loader reads, and importing that again changes nothing.
use super::*;

pub fn run(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    if let Ok(out) = import(&text, 77, "2.103") {
        let save = SaveFile::parse(&out).unwrap_or_else(|e| panic!("the import wrote a save the loader refuses: {e}"));
        crate::save::check_settings(&save.settings).unwrap_or_else(|e| panic!("the import wrote settings the loader refuses: {e}"));
        match import(&out, 5, "other") {
            Ok(again) => assert_eq!(again, out, "importing a converted save changed it"),
            Err(e) => panic!("a converted save is refused by the import: {e}"),
        }
    }
    // The pieces on their own.
    let _ = patched(&text);
    let _ = convert(&text, 1, "x");
    let _ = string_field(&text, "planetId", None);
    let _ = string_field(&text, "unlockedGroups", Some(','));
}
