//! `pcds-server extract RAW.json OUT_DIR`: turns the plugin's GET /api/netdata export (taken
//! from the real game running as a server) into this version's netdata.json, rpcs.json and
//! connection-request.hex. With GET /api/gamedata and /api/scenedata (see `pcds standalone
//! update`), that is everything a new game version needs.

use serde_json::{json, Map, Value};

use crate::protocol::read_scene_object;
use crate::wire::{Reader, Writer};
use crate::world::hex;

fn hex_of(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A spawned object as netdata describes it: hash, flags, parent, behaviours with their
/// variables, and (for prefabs) the sync data after the variables and the spawn's full sync.
fn object(o: &Value, prefab: bool) -> Option<Value> {
    let bytes = hex(o["sceneObject"].as_str()?);
    let s = read_scene_object(&mut Reader::new(&bytes))?;
    let behaviours: Vec<Value> = o["behaviours"]
        .as_array()?
        .iter()
        .map(|b| {
            let vars: Vec<Value> = b["variables"].as_array().into_iter().flatten().map(|v| json!({"name": v["name"], "type": v["type"], "bytes": v["bytes"]})).collect();
            json!({"class": b["class"].as_str().unwrap_or("").rsplit('.').next().unwrap_or(""), "behaviourId": b["behaviourId"], "vars": vars})
        })
        .collect();
    let mut entry = json!({"name": o["name"], "hash": s.hash, "flags": s.flags, "parent": s.parent, "behaviours": behaviours});
    if prefab {
        let vars_len: usize = o["behaviours"].as_array()?.iter().flat_map(|b| b["variables"].as_array().into_iter().flatten()).map(|v| v["bytes"].as_str().unwrap_or("").len() / 2).sum();
        // The transform (when the object syncs one) ends with the scale, right before the
        // scene handle and the sync size.
        let scale: Option<Vec<f32>> = s.position.map(|_| {
            let at = bytes.len() - s.sync.len() - 8 - 12;
            [0, 4, 8].iter().map(|i| f32::from_le_bytes(bytes[at + i..at + i + 4].try_into().unwrap())).collect()
        });
        entry["syncTail"] = json!(hex_of(s.sync.get(vars_len..)?));
        entry["scale"] = json!(scale);
        entry["spawnSync"] = json!(hex_of(&s.sync));
    } else {
        entry["id"] = json!(s.id);
    }
    Some(entry)
}

pub fn run(args: &[String]) {
    let [raw, out] = args else {
        eprintln!("usage: pcds-server extract RAW.json OUT_DIR");
        std::process::exit(2);
    };
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(raw).expect("read raw export")).expect("parse raw export");
    let objects = raw["objects"].as_array().cloned().unwrap_or_default();
    // A joined (remote) player's layout is the one joining clients get; the host's own player
    // only stands in when nobody joined.
    let player = objects.iter().filter(|o| o["player"].as_bool() == Some(true)).max_by_key(|o| o["owner"].as_u64() != Some(0)).cloned();
    let mut scene = Vec::new();
    let mut prefabs = Map::new();
    let mut scene_handle = 0;
    for o in &objects {
        let bytes = hex(o["sceneObject"].as_str().unwrap_or(""));
        if let Some(s) = read_scene_object(&mut Reader::new(&bytes)) {
            // The scene handle sits right before the sync size.
            let at = bytes.len() - s.sync.len() - 8;
            scene_handle = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        }
        let name = o["name"].as_str().unwrap_or("").replace("(Clone)", "");
        if o["scene"].as_bool() == Some(true) {
            scene.extend(object(o, false));
        } else if o["player"].as_bool() == Some(true) {
            if player.as_ref() == Some(o) {
                prefabs.insert("Player".into(), object(o, true).expect("player object"));
            }
        } else if name.starts_with("WorldUnitsPlanet") && !prefabs.contains_key("WorldUnitsPlanet") {
            prefabs.insert("WorldUnitsPlanet".into(), object(o, true).expect("planet units object"));
        }
    }
    let groups: Map<String, Value> = raw["groups"].as_array().into_iter().flatten().filter_map(|g| Some((g["id"].as_str()?.to_string(), json!({"hash": g["hash"], "prefab": g["prefab"], "prefabHash": g["prefabHash"]})))).collect();
    let netdata = json!({
        "gameVersion": raw["gameVersion"],
        "tickRate": raw["tickRate"],
        "playerPrefab": raw["playerPrefab"],
        "sceneHandle": scene_handle,
        "messageVersions": raw["messageVersions"],
        "sceneObjects": scene,
        "prefabs": prefabs,
        "planets": raw["planets"],
        "groups": groups,
    });
    std::fs::create_dir_all(out).expect("create output directory");
    let write = |name: &str, text: String| std::fs::write(format!("{out}/{name}"), text).unwrap_or_else(|e| panic!("{out}/{name}: {e}"));
    write("netdata.json", serde_json::to_string_pretty(&netdata).unwrap());
    let mut rpcs: Vec<(String, Value)> = raw["rpcs"].as_object().cloned().unwrap_or_default().into_iter().collect();
    rpcs.sort_by(|a, b| a.1["name"].as_str().cmp(&b.1["name"].as_str()));
    let rpcs: Map<String, Value> = rpcs.into_iter().collect();
    write("rpcs.json", serde_json::to_string_pretty(&rpcs).unwrap());
    // ConnectionRequestMessage: message versions, config hash, connection data ("version|0").
    let mut w = Writer::new();
    let versions = raw["messageVersions"].as_array().cloned().unwrap_or_default();
    w.packed_i32(versions.len() as i32);
    for v in &versions {
        w.u32(v[0].as_u64().unwrap_or(0) as u32).packed_i32(v[1].as_i64().unwrap_or(0) as i32);
    }
    let config: u64 = raw["configHash"].as_str().and_then(|s| s.parse().ok()).expect("configHash");
    let data = raw["connectionData"].as_str().unwrap_or("").as_bytes().to_vec();
    w.u64(config).i32(data.len() as i32).bytes(&data);
    write("connection-request.hex", hex_of(&w.0));
    println!("{out}: {} scene objects, prefabs {:?}, {} RPCs, {} message types", netdata["sceneObjects"].as_array().map(Vec::len).unwrap_or(0), prefabs.keys().collect::<Vec<_>>(), rpcs.len(), versions.len());
}

#[cfg(test)]
mod tests {
    #[test]
    fn rebuilds_the_captured_connection_request() {
        // The 2.103 request a real client sent, rebuilt from its parts.
        let Some(dir) = crate::test_data() else { return };
        let captured = std::fs::read_to_string(format!("{dir}/connection-request.hex")).unwrap();
        let bytes = crate::world::hex(captured.trim());
        let tail = &bytes[bytes.len() - 19..];
        assert_eq!(&tail[8..12], &7i32.to_le_bytes());
        assert_eq!(&tail[12..], b"2.103|0");
    }
}
