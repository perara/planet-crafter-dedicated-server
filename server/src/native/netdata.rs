//! The network tables that need no spawned object (NetData.Build, Manifest.Build): planets,
//! groups with the network prefab each spawns as, the NetworkManager's settings, and Netcode's
//! message table (each INetworkMessage's XXHash32 name hash and version).

use serde_json::{json, Map, Value};

use super::serial::Raw;
use super::tables::Ctx;
use super::dotnet::TypeName;
use super::unity::{self, Id};

/// A field of a read object by name (the last one: derived classes win).
pub fn field<'a>(fields: &'a [(String, Raw)], name: &str) -> Option<&'a Raw> {
    fields.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v)
}

impl Ctx {
    /// The first MonoBehaviour of a script class in the given files.
    pub fn find_behaviour(&mut self, files: &[&str], namespace: &str, class: &str) -> Option<Id> {
        for name in files {
            let Ok(f) = self.data.file(name) else { continue };
            let behaviours: Vec<i64> = self.data.files[f].objects.iter().filter(|o| o.class_id == unity::MONO_BEHAVIOUR).map(|o| o.path).collect();
            for path in behaviours {
                let id = Id(f, path);
                if self.graph.script(&mut self.data, id).is_some_and(|s| s.0 == namespace && s.1 == class) {
                    return Some(id);
                }
            }
        }
        None
    }

    /// NetworkObject.GlobalObjectIdHash of a prefab root, as 8 hex digits.
    pub fn prefab_hash(&mut self, root: Id) -> Option<String> {
        self.graph_file(root.0);
        let components = self.graph.nodes.get(&root)?.components.clone();
        for (c, class) in components {
            if class != unity::MONO_BEHAVIOUR || self.graph.script(&mut self.data, c).is_none_or(|s| s.1 != "NetworkObject") {
                continue;
            }
            let b = self.behaviour(c)?;
            if let Some(Raw::UInt(h)) = field(&b.fields, "GlobalObjectIdHash") {
                return Some(format!("{:08x}", h));
            }
        }
        None
    }

    /// Manifest planets: GetPlanetList(allPlanets: true), the ones shown in the build.
    pub fn planets(&mut self, assets: &Map<String, Value>) -> Vec<Value> {
        let list = assets.get("PlanetList:Planets").and_then(|p| p["_planets"].as_array()).cloned().unwrap_or_default();
        list.iter()
            .filter_map(|r| assets.get(r.as_str()?))
            .filter(|p| p["showInBuild"].as_bool() == Some(true))
            .map(|p| {
                let id = p["id"].as_str().unwrap_or("");
                json!({"id": id, "hash": crate::data::stable_hash(id), "steamAppId": p["steamAppId"], "startingPlanet": p["startingPlanet"]})
            })
            .collect()
    }

    /// Manifest groups (as extract-raw keeps them): id -> hash, prefab name and prefab hash.
    pub fn group_table(&mut self) -> Map<String, Value> {
        let mut out = Map::new();
        for (gid, prefab) in self.groups.clone() {
            let (name, hash) = match prefab {
                Some(root) => {
                    self.graph_file(root.0);
                    (self.graph.nodes.get(&root).map(|n| json!(n.name)).unwrap_or(Value::Null), self.prefab_hash(root).map(Value::from).unwrap_or(Value::Null))
                }
                None => (Value::Null, Value::Null),
            };
            out.insert(gid.clone(), json!({"hash": crate::data::stable_hash(&gid), "prefab": name, "prefabHash": hash}));
        }
        out
    }

    /// The NetworkManager's NetworkConfig fields.
    pub fn network_config(&mut self) -> Option<(usize, Vec<(String, Raw)>)> {
        let id = self.find_behaviour(&["level0", "level1", "level4"], "Unity.Netcode", "NetworkManager")?;
        let b = self.behaviour(id)?;
        match field(&b.fields, "NetworkConfig")? {
            Raw::Fields(_, f) => Some((id.0, f.clone())),
            _ => None,
        }
    }

    /// The message table as ConnectionRequestMessage sends it: slot i is the message
    /// NetworkMessageTypes numbers i, as (XXHash32 of its full name, version); the slots up to
    /// the next power of two stay (0, 0).
    pub fn message_versions(&self) -> Vec<Value> {
        let asm = &self.layouts.asm;
        let Some(netcode) = asm.0.iter().find(|a| a.name == "Unity.Netcode.Runtime") else { return Vec::new() };
        let Some(numbers) = netcode.find("Unity.Netcode", "NetworkMessageTypes").map(|i| &netcode.types()[i]) else { return Vec::new() };
        let slots: Vec<(i64, &str)> = numbers.fields.iter().filter_map(|f| Some((f.constant?, f.name.as_str()))).collect();
        let count = slots.iter().map(|(n, _)| *n + 1).max().unwrap_or(0).max(0) as usize;
        let mut out = vec![json!([0, 0]); count.next_power_of_two()];
        for (n, member) in slots {
            let message = netcode.types().iter().find(|t| t.namespace == "Unity.Netcode" && (t.name == member || t.name == format!("{member}Message")));
            let Some(t) = message else { continue };
            let full = format!("{}.{}", t.namespace, t.name);
            let version = t.methods.iter().find(|m| m.name == "get_Version").and_then(|m| netcode.il(m)).and_then(constant).unwrap_or(0);
            out[n as usize] = json!([xxhash_rust::xxh32::xxh32(full.as_bytes(), 0), version]);
        }
        out
    }
}

/// The int32 an IL body returns as its first instruction (ldc.i4.*).
fn constant(il: &[u8]) -> Option<i64> {
    match *il.first()? {
        0x15 => Some(-1),
        op @ 0x16..=0x1e => Some(op as i64 - 0x16),
        0x1f => Some(*il.get(1)? as i8 as i64),
        0x20 => Some(i32::from_le_bytes(il.get(1..5)?.try_into().ok()?) as i64),
        _ => None,
    }
}

/// A network object as a spawn describes it, and where it fails to.
type Entry = Result<Map<String, Value>, String>;

fn yes(fields: &[(String, Raw)], name: &str) -> bool {
    matches!(field(fields, name), Some(Raw::Bool(true)))
}

impl Ctx {
    /// The NetworkObject component of a game object.
    fn network_object_of(&mut self, node: Id) -> Option<Id> {
        let components = self.graph.nodes.get(&node)?.components.clone();
        components.into_iter().find(|(c, class)| *class == unity::MONO_BEHAVIOUR && self.graph.script(&mut self.data, *c).is_some_and(|s| s.1 == "NetworkObject")).map(|(c, _)| c)
    }

    fn script_type(&mut self, behaviour: Id) -> Option<TypeName> {
        let (namespace, name, assembly) = self.graph.script(&mut self.data, behaviour)?.clone();
        Some(TypeName { scope: Some(assembly.trim_end_matches(".dll").to_string()), namespace, name })
    }

    /// NetworkObject.SceneObject's flag bit for a property (what its getter tests).
    fn scene_flag(&self, property: &str) -> Result<u16, String> {
        let asm = &self.layouts.asm;
        let t = TypeName { scope: Some("Unity.Netcode.Runtime".into()), namespace: "Unity.Netcode".into(), name: "NetworkObject/SceneObject".into() };
        let get = asm.def(&t).and_then(|d| d.methods.iter().find(|m| m.name == format!("get_{property}")));
        let bit = get.and_then(|m| asm.assembly_of(&t)?.il(m)).and_then(super::netvars::first_int);
        bit.map(|b| 1 << b).ok_or_else(|| format!("SceneObject.{property}: no flag bit"))
    }

    /// A NetworkTransformState constant (k_*).
    fn transform_bit(&self, name: &str) -> Result<u32, String> {
        let asm = &self.layouts.asm;
        let t = TypeName { scope: Some("Unity.Netcode.Runtime".into()), namespace: "Unity.Netcode.Components".into(), name: "NetworkTransform/NetworkTransformState".into() };
        asm.def(&t).and_then(|d| d.fields.iter().find(|f| f.name == name)?.constant).map(|c| c as u32).ok_or_else(|| format!("NetworkTransformState.{name}: no constant"))
    }

    /// NetworkTransform.OnSynchronize's state for a spawn: the flags from the component's
    /// settings, and the transform's own values for each synchronized axis.
    fn transform_state(&mut self, behaviour: Id, node: Id, root: Id) -> Result<Value, String> {
        let b = self.behaviour(behaviour).ok_or("unreadable NetworkTransform")?;
        let f = &b.fields;
        for unsupported in ["UseHalfFloatPrecision", "UseQuaternionSynchronization", "SyncScaleX", "SyncScaleY", "SyncScaleZ", "SlerpPosition", "SwitchTransformSpaceWhenParented"] {
            if yes(f, unsupported) {
                return Err(format!("{}: NetworkTransform.{unsupported} is not supported", b.class));
            }
        }
        if node != root && !yes(f, "InLocalSpace") {
            return Err(format!("{} below the root syncs in world space: not supported", b.class));
        }
        let mut flags = self.transform_bit("k_TeleportingBit")? | self.transform_bit("k_Synchronization")? | self.transform_bit("k_ReliableSequenced")?;
        if yes(f, "InLocalSpace") {
            flags |= self.transform_bit("k_InLocalSpaceBit")?;
        }
        if yes(f, "Interpolate") {
            flags |= self.transform_bit("k_Interpolate")?;
        }
        if yes(f, "UseUnreliableDeltas") {
            flags |= self.transform_bit("k_UseUnreliableDeltas")?;
        }
        let x = self.graph.nodes.get(&node).and_then(|n| n.transform).and_then(|t| self.graph.xforms.get(&t)).ok_or("no transform")?;
        let euler = crate::transform::euler_angles(x.rotation);
        let mut values = Vec::new();
        for (i, axis) in ["X", "Y", "Z"].iter().enumerate() {
            if yes(f, &format!("SyncPosition{axis}")) {
                flags |= self.transform_bit(&format!("k_Position{axis}Bit"))?;
                values.push(super::tables::single(x.position[i]));
            }
        }
        for (i, axis) in ["X", "Y", "Z"].iter().enumerate() {
            if yes(f, &format!("SyncRotAngle{axis}")) {
                flags |= self.transform_bit(&format!("k_RotAngle{axis}Bit"))?;
                values.push(super::tables::single(euler[i]));
            }
        }
        Ok(json!({"flags": flags, "root": node == root, "values": values}))
    }

    /// A NetworkObject's spawn description: hash, flags, parent, and its NetworkBehaviours
    /// (GetComponentsInChildren, the ones this object owns) with their variables and
    /// NetworkTransform states.
    fn network_object(&mut self, object: Id, scene: bool) -> Entry {
        let b = self.behaviour(object).ok_or("unreadable NetworkObject")?;
        let root = b.game_object.ok_or("NetworkObject without a game object")?;
        let f = b.fields.clone();
        let uint = |n: &str| match field(&f, n) {
            Some(Raw::UInt(v)) => Ok(*v as u32),
            other => Err(format!("NetworkObject.{n}: {other:?}")),
        };
        let source = uint("InScenePlacedSourceGlobalObjectIdHash")?;
        // CheckForGlobalObjectIdHashOverride: an in-scene instance of a prefab goes as the prefab.
        let hash = if scene && source != 0 { source } else { uint("GlobalObjectIdHash")? };
        let mut flags = self.scene_flag("WorldPositionStays")?;
        let parent = self.graph.parent(root);
        if scene {
            flags |= self.scene_flag("IsSceneObject")? | self.scene_flag("DestroyWithScene")?;
            if parent.is_some() && !yes(&f, "AlwaysReplicateAsRoot") {
                flags |= self.scene_flag("HasParent")?;
            }
        }
        if yes(&f, "SynchronizeTransform") {
            flags |= self.scene_flag("HasTransform")?;
        }
        if yes(&f, "DontDestroyWithOwner") {
            flags |= self.scene_flag("DontDestroyWithOwner")?;
        }
        let name = self.graph.nodes.get(&root).map(|n| n.name.clone()).unwrap_or_default();
        let mut behaviours = Vec::new();
        let mut transforms = Vec::new();
        let nodes = self.graph.descendants(root);
        let mut owned: std::collections::HashSet<Id> = Default::default();
        for n in nodes {
            // A NetworkObject below this one owns its own subtree.
            let parent_owned = n == root || self.graph.parent(n).is_some_and(|p| owned.contains(&p));
            let components = self.graph.nodes.get(&n).map(|x| x.components.clone()).unwrap_or_default();
            if !parent_owned || (n != root && self.network_object_of(n).is_some()) {
                continue;
            }
            owned.insert(n);
            for (c, class) in components {
                if class != unity::MONO_BEHAVIOUR {
                    continue;
                }
                let Some(ty) = self.script_type(c) else { continue };
                if !self.layouts.asm.derives_from(&ty, "Unity.Netcode.NetworkBehaviour") {
                    continue;
                }
                let index = behaviours.len();
                let fields = self.behaviour(c).map(|b| b.fields).unwrap_or_default();
                let vars = super::netvars::variables(&self.layouts.asm, &ty, &fields)?;
                behaviours.push(json!({"class": ty.name, "behaviourId": index, "vars": vars}));
                if self.layouts.asm.derives_from(&ty, "Unity.Netcode.Components.NetworkTransform") {
                    let mut state = self.transform_state(c, n, root)?;
                    state["behaviour"] = json!(index);
                    transforms.push(state);
                }
            }
        }
        let scale = self.graph.nodes.get(&root).and_then(|n| n.transform).and_then(|t| self.graph.xforms.get(&t)).map(|x| x.scale).unwrap_or([1.0; 3]);
        let mut out = Map::new();
        out.insert("name".into(), json!(name));
        out.insert("hash".into(), json!(hash));
        out.insert("flags".into(), json!(flags));
        out.insert("parent".into(), if flags & self.scene_flag("HasParent")? != 0 { json!(0) } else { Value::Null });
        out.insert("behaviours".into(), Value::Array(behaviours));
        out.insert("transforms".into(), Value::Array(transforms));
        out.insert("scale".into(), json!(scale.map(super::tables::single)));
        Ok(out)
    }

    /// The in-scene network objects of the game's main scene, in the order the server spawns
    /// them on start (file order), numbered from 2 as the game's own server numbers them.
    pub fn scene_objects(&mut self) -> Result<Vec<Value>, String> {
        let main = self.data.build_scenes().iter().position(|s| s.ends_with("/GameMainScene.unity")).ok_or("no GameMainScene in the build")?;
        let file = self.data.file(&format!("level{main}")).map_err(|e| e.to_string())?;
        self.graph_file(file);
        let behaviours: Vec<i64> = self.data.files[file].objects.iter().filter(|o| o.class_id == unity::MONO_BEHAVIOUR).map(|o| o.path).collect();
        let mut out = Vec::new();
        for path in behaviours {
            let id = Id(file, path);
            if self.graph.script(&mut self.data, id).is_none_or(|s| s.1 != "NetworkObject") {
                continue;
            }
            let mut entry = self.network_object(id, true)?;
            let parent = self.behaviour(id).and_then(|b| b.game_object).and_then(|g| self.graph.parent(g));
            if parent.is_some_and(|p| self.network_object_of(p).is_some()) {
                return Err(format!("{}: in-scene network objects below another are not supported", entry["name"]));
            }
            entry.insert("id".into(), json!(out.len() + 2));
            out.push(Value::Object(entry));
        }
        Ok(out)
    }

    /// The prefab of a network object (its root's NetworkObject) as a spawn describes it.
    pub fn prefab(&mut self, root: Id, player: bool) -> Entry {
        self.graph_file(root.0);
        let object = self.network_object_of(root).ok_or("prefab without a NetworkObject")?;
        let mut entry = self.network_object(object, false)?;
        if player {
            let flags = entry["flags"].as_u64().unwrap_or(0) as u16 | self.scene_flag("IsPlayerObject")?;
            entry.insert("flags".into(), json!(flags));
        }
        Ok(entry)
    }
}

impl Ctx {
    /// GameData layouts: the network object each group's world objects spawn as
    /// (WorldObjectsHandler.InstantiateWorldObject): the group's own prefab when it has a
    /// NetworkObject, else the handler's container prefab (with an inventory when the group has
    /// one; a "#static" one for the others loaded from the save).
    pub fn world_object_layouts(&mut self, assets: &Map<String, Value>, group_prefabs: &Map<String, Value>) -> Result<Map<String, Value>, String> {
        let handler = self.find_behaviour(&["level4"], "SpaceCraft", "WorldObjectsHandler").ok_or("no WorldObjectsHandler")?;
        let fields = self.behaviour(handler).ok_or("unreadable WorldObjectsHandler")?.fields;
        // InstantiateWorldObject: networkObject.Spawn(destroyWithScene).
        let asm = &self.layouts.asm;
        let t = TypeName { scope: Some("Assembly-CSharp".into()), namespace: "SpaceCraft".into(), name: "WorldObjectsHandler".into() };
        let instantiate = asm.def(&t).and_then(|d| d.methods.iter().find(|m| m.name == "InstantiateWorldObject"));
        let destroy_with_scene = instantiate.and_then(|m| { let a = asm.assembly_of(&t)?; super::netvars::bool_argument(a, a.il(m)?, "Spawn") }).ok_or("InstantiateWorldObject: no Spawn(bool)")?;
        let with_scene = if destroy_with_scene { self.scene_flag("DestroyWithScene")? } else { 0 };
        let spawned = |mut v: Value| {
            v["flags"] = json!(v["flags"].as_u64().unwrap_or(0) | with_scene as u64);
            v
        };
        let mut container = |ctx: &mut Ctx, name: &str| -> Result<Value, String> {
            let root = match field(&fields, name) {
                Some(Raw::Ref(p, _)) => ctx.data.resolve(handler.0, *p),
                _ => None,
            };
            let mut entry = ctx.prefab(root.ok_or(format!("WorldObjectsHandler.{name}: no prefab"))?, false)?;
            entry.insert("prefab".into(), entry["name"].clone());
            Ok(Value::Object(entry))
        };
        let plain = spawned(container(self, "_groupNetworkContainer")?);
        let fixed = spawned(container(self, "_groupNetworkContainerStatic")?);
        let with_inventory = spawned(container(self, "_groupNetworkContainerWithInventory")?);
        let (rot_x, rot_z) = (self.transform_bit("k_RotAngleXBit")? as u64, self.transform_bit("k_RotAngleZBit")? as u64);
        let rot_y = self.transform_bit("k_RotAngleYBit")? as u64;
        // GroupNetworkContainer.CreateChild: an item with its own Rigidbody keeps only the
        // container's Y angle in sync.
        let rigid = |mut v: Value, gid: &str| {
            if group_prefabs.get(gid).is_some_and(|g| g["rigidbody"] == true) {
                for t in v["transforms"].as_array_mut().into_iter().flatten() {
                    let flags = t["flags"].as_u64().unwrap_or(0);
                    let positions = t["values"].as_array().map(|a| a.len()).unwrap_or(0) - [rot_x, rot_y, rot_z].iter().filter(|b| flags & **b != 0).count();
                    let mut values = t["values"].as_array().cloned().unwrap_or_default();
                    // Values: the positions, then the X, Y, Z angles that are synced.
                    let mut at = positions;
                    for bit in [rot_x, rot_y, rot_z] {
                        if flags & bit != 0 {
                            if bit != rot_y {
                                values.remove(at);
                            } else {
                                at += 1;
                            }
                        }
                    }
                    t["flags"] = json!(flags & !(rot_x | rot_z));
                    t["values"] = Value::Array(values);
                }
            }
            v
        };
        let inventory: std::collections::HashMap<&str, i64> = assets.iter().filter(|(k, _)| k.starts_with("GroupData")).filter_map(|(_, g)| Some((g["id"].as_str()?, g["inventorySize"].as_i64().unwrap_or(0)))).collect();
        let mut out = Map::new();
        for (gid, prefab) in self.groups.clone() {
            let Some(root) = prefab else { continue };
            self.graph_file(root.0);
            if self.network_object_of(root).is_some() {
                let mut entry = self.prefab(root, false).map_err(|e| format!("{gid}: {e}"))?;
                entry.insert("prefab".into(), entry["name"].clone());
                out.insert(gid, spawned(Value::Object(entry)));
            } else if inventory.get(gid.as_str()).copied().unwrap_or(0) > 0 {
                out.insert(gid.clone(), rigid(with_inventory.clone(), &gid));
            } else {
                out.insert(format!("{gid}#static"), rigid(fixed.clone(), &gid));
                out.insert(gid.clone(), rigid(plain.clone(), &gid));
            }
        }
        Ok(out)
    }
}
