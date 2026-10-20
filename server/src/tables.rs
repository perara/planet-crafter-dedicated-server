//! The tables folder's contract: which game build tables are for, and which folder of run/data
//! holds the tables for a build in the format this server reads. The command line, the bank's
//! status and the reader (`pcds read-game`) all ask these two.

use std::path::{Path, PathBuf};

/// The game build: a hash of the game's code, which changes with every game update. One way
/// for every installation (Steam, steamcmd or a copied folder).
pub fn build_id(game: &Path) -> Result<String, String> {
    let path = game.join("Planet Crafter_Data/Managed/Assembly-CSharp.dll");
    let code = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{:016x}", xxhash_rust::xxh64::xxh64(&code, 0)))
}

/// The tables in `data` (run/data) for a game build in a tables format. Folders whose name
/// starts with "." are extractions not finished yet.
pub fn find_tables(data: &Path, build: &str, format: u32) -> Result<PathBuf, String> {
    let (mut any, mut other_format) = (false, None);
    for entry in std::fs::read_dir(data).into_iter().flatten().flatten() {
        let dir = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') || !dir.is_dir() {
            continue;
        }
        any = true;
        if !std::fs::read_to_string(dir.join("buildid")).is_ok_and(|id| id.trim() == build) {
            continue;
        }
        let theirs = std::fs::read_to_string(dir.join("tables-format")).map_or_else(|_| "none".to_string(), |f| f.trim().to_string());
        if theirs == format.to_string() {
            return Ok(dir);
        }
        other_format = Some((dir, theirs));
    }
    if let Some((dir, theirs)) = other_format {
        return Err(format!("the tables in {} are of tables format {theirs}, the server reads format {format}: run `pcds update`, which reads them again", dir.display()));
    }
    if !any {
        return Err(format!("no game tables in {}: run `pcds update` (see README)", data.display()));
    }
    Err(format!("the game changed: run `pcds update` (no tables in {} for game build {build})", data.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_id_is_a_hash_of_the_game_code() {
        let dir = std::env::temp_dir().join(format!("pcds-build-id-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Planet Crafter_Data/Managed")).unwrap();
        std::fs::write(dir.join("Planet Crafter_Data/Managed/Assembly-CSharp.dll"), b"code").unwrap();
        let id = build_id(&dir).unwrap();
        assert_eq!(id, format!("{:016x}", xxhash_rust::xxh64::xxh64(b"code", 0)));
        assert_eq!(id.len(), 16);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(build_id(&dir).is_err());
    }
}
