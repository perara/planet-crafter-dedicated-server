//! The network tables that need no spawned object (NetData.Build, Manifest.Build): planets,
//! groups with the network prefab each spawns as, the NetworkManager's settings, and Netcode's
//! message table (each INetworkMessage's XXHash32 name hash and version).

use serde_json::{json, Map, Value};

use super::serial::Raw;
use super::tables::Ctx;
use super::dotnet::{Member, TypeName, TypeSig};
use super::unity::{self, Id};

/// A field of a read object by name (the first one: a class's own field hides its base's).
pub fn field<'a>(fields: &'a [(String, Raw)], name: &str) -> Option<&'a Raw> {
    fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

impl Ctx {
    /// The first MonoBehaviour of a script class in the given files.
    pub fn find_behaviour(&mut self, files: &[&str], namespace: &str, class: &str) -> Result<Option<Id>, String> {
        for name in files {
            let f = self.data.file(name).map_err(|e| format!("{name}: {e}"))?;
            let behaviours: Vec<i64> = self.data.files[f].objects.iter().filter(|o| o.class_id == unity::MONO_BEHAVIOUR).map(|o| o.path).collect();
            for path in behaviours {
                let id = Id(f, path);
                if self.graph.script(&mut self.data, id)?.is_some_and(|s| s.0 == namespace && s.1 == class) {
                    return Ok(Some(id));
                }
            }
        }
        Ok(None)
    }

    /// NetworkObject.GlobalObjectIdHash of a prefab root, as 8 hex digits; None when the root
    /// has no NetworkObject.
    pub fn prefab_hash(&mut self, root: Id) -> Result<Option<String>, String> {
        self.graph_file(root.0)?;
        let Some(c) = self.network_object_of(root)? else { return Ok(None) };
        match field(&self.behaviour(c)?.fields, "GlobalObjectIdHash") {
            Some(Raw::UInt(h)) => Ok(Some(format!("{:08x}", h))),
            other => Err(format!("NetworkObject.GlobalObjectIdHash: {other:?}")),
        }
    }

    /// Manifest planets: GetPlanetList(allPlanets: true), the ones shown in the build.
    pub fn planets(&mut self, assets: &Map<String, Value>) -> Result<Vec<Value>, String> {
        let list = assets.get("PlanetList:Planets").and_then(|p| p["_planets"].as_array()).ok_or("no PlanetList:Planets._planets")?;
        let mut out = Vec::new();
        for r in list {
            let p = r.as_str().and_then(|r| assets.get(r)).ok_or_else(|| format!("PlanetList:Planets._planets: {r} is not an asset"))?;
            if p["showInBuild"].as_bool().ok_or_else(|| format!("{r}: showInBuild"))? {
                let id = p["id"].as_str().ok_or_else(|| format!("{r}: no id"))?;
                out.push(json!({"id": id, "hash": crate::data::stable_hash(id), "steamAppId": p["steamAppId"], "startingPlanet": p["startingPlanet"]}));
            }
        }
        Ok(out)
    }

    /// Manifest groups: id -> hash, prefab name and prefab hash.
    pub fn group_table(&mut self) -> Result<Map<String, Value>, String> {
        let mut out = Map::new();
        for (gid, prefab) in self.groups.clone() {
            let (name, hash) = match prefab {
                Some(root) => {
                    self.graph_file(root.0)?;
                    let name = self.graph.nodes.get(&root).map(|n| json!(n.name)).ok_or_else(|| format!("{gid}: its prefab is not a GameObject"))?;
                    (name, self.prefab_hash(root)?.map(Value::from).unwrap_or(Value::Null))
                }
                None => (Value::Null, Value::Null),
            };
            out.insert(gid.clone(), json!({"hash": crate::data::stable_hash(&gid), "prefab": name, "prefabHash": hash}));
        }
        Ok(out)
    }

    /// The NetworkManager's NetworkConfig fields.
    pub fn network_config(&mut self) -> Result<(usize, Vec<(String, Raw)>), String> {
        let id = self.find_behaviour(&["level0", "level1", "level4"], "Unity.Netcode", "NetworkManager")?.ok_or("no NetworkManager in level0, level1 or level4")?;
        match field(&self.behaviour(id)?.fields, "NetworkConfig") {
            Some(Raw::Fields(_, f)) => Ok((id.0, f.clone())),
            other => Err(format!("NetworkManager.NetworkConfig: {other:?}")),
        }
    }

    /// The message table as ConnectionRequestMessage sends it: slot i is the message
    /// NetworkMessageTypes numbers i, as (XXHash32 of its full name, version); the slots up to
    /// the next power of two stay (0, 0).
    /// Every NetworkMessageTypes member must name a message type with a get_Version read.
    pub fn message_versions(&self) -> Result<Vec<Value>, String> {
        let asm = &self.layouts.asm;
        let netcode = asm.0.iter().find(|a| a.name == "Unity.Netcode.Runtime").ok_or("no Unity.Netcode.Runtime assembly")?;
        let numbers = netcode.find("Unity.Netcode", "NetworkMessageTypes").map(|i| &netcode.types()[i]).ok_or("no Unity.Netcode.NetworkMessageTypes")?;
        let mut slots: Vec<(usize, &str)> = Vec::new();
        for f in numbers.fields.iter().filter(|f| f.name != "value__") {
            let n = f.constant.ok_or_else(|| format!("NetworkMessageTypes.{}: no constant", f.name))?;
            slots.push((usize::try_from(n).ok().filter(|n| *n < 4096).ok_or_else(|| format!("NetworkMessageTypes.{} = {n}: not a message slot", f.name))?, f.name.as_str()));
        }
        let count = slots.iter().map(|(n, _)| *n + 1).max().ok_or("NetworkMessageTypes has no members")?;
        let mut out = vec![json!([0, 0]); count.next_power_of_two()];
        for (n, member) in slots {
            let message = netcode.types().iter().position(|t| t.namespace == "Unity.Netcode" && (t.name == member || t.name == format!("{member}Message")));
            let t = message.ok_or_else(|| format!("NetworkMessageTypes.{member}: no message type {member} or {member}Message"))?;
            let def = &netcode.types()[t];
            let full = format!("{}.{}", def.namespace, def.name);
            out[n] = json!([xxhash_rust::xxh32::xxh32(full.as_bytes(), 0), message_version(netcode, t, 0)?]);
        }
        Ok(out)
    }
}

/// INetworkMessage.Version of a message type: get_Version returns an int32 constant, or another
/// message's get_Version (ProxyMessage returns RpcMessage's).
fn message_version(netcode: &super::dotnet::Assembly, t: usize, depth: usize) -> Result<i64, String> {
    let def = &netcode.types()[t];
    let get = def.methods.iter().find(|m| m.name == "get_Version").ok_or_else(|| format!("{}: no get_Version", def.name))?;
    let il = netcode.il(get).ok_or_else(|| format!("{}.get_Version: no IL body", def.name))?;
    let ops = super::netvars::instructions(il)?;
    let unread = || format!("{}.get_Version: neither a constant nor another message's version ({ops:x?})", def.name);
    match ops[..] {
        [(code @ 0x15..=0x1E, _), (0x2A, _)] => Ok(code as i64 - 0x16),
        [(0x1F | 0x20, v), (0x2A, _)] => Ok(v),
        // ldloca; initobj; ldloca; call Other.get_Version; ret
        [.., (0x28, token), (0x2A, _)] if depth < 4 => match netcode.member(token as u32) {
            Some(Member::Method { owner: TypeSig::ValueType(o) | TypeSig::Class(o), name, .. }) if name == "get_Version" && o.scope.as_ref().is_none_or(|s| *s == netcode.name) => {
                let other = netcode.find(&o.namespace, &o.name).ok_or_else(unread)?;
                message_version(netcode, other, depth + 1)
            }
            _ => Err(unread()),
        },
        _ => Err(unread()),
    }
}

/// A network object as a spawn describes it, and where it fails to.
type Entry = Result<Map<String, Value>, String>;

fn yes(fields: &[(String, Raw)], name: &str) -> bool {
    matches!(field(fields, name), Some(Raw::Bool(true)))
}

impl Ctx {
    /// The NetworkObject component of a game object.
    pub fn network_object_of(&mut self, node: Id) -> Result<Option<Id>, String> {
        let components = self.graph.nodes.get(&node).ok_or_else(|| format!("{node:?}: not a game object of a file read"))?.components.clone();
        for (c, class) in components {
            if class == unity::MONO_BEHAVIOUR && self.graph.script(&mut self.data, c)?.is_some_and(|s| s.0 == "Unity.Netcode" && s.1 == "NetworkObject") {
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    /// A MonoBehaviour's script class, which must have one and be in the assemblies.
    fn script_type(&mut self, behaviour: Id) -> Result<TypeName, String> {
        let (namespace, name, assembly) = self.graph.script(&mut self.data, behaviour)?.ok_or_else(|| format!("{behaviour:?}: a MonoBehaviour without a script"))?.clone();
        let t = TypeName { scope: Some(assembly.trim_end_matches(".dll").to_string()), namespace, name };
        self.layouts.asm.resolve(&t).ok_or_else(|| format!("{}.{} ({assembly}): not in the game's assemblies", t.namespace, t.name))?;
        Ok(t)
    }

    /// NetworkObject.SceneObject's flag bit for a property (what its getter tests).
    fn scene_flag(&self, property: &str) -> Result<u16, String> {
        let asm = &self.layouts.asm;
        let t = TypeName { scope: Some("Unity.Netcode.Runtime".into()), namespace: "Unity.Netcode".into(), name: "NetworkObject/SceneObject".into() };
        let get = asm.def(&t).and_then(|d| d.methods.iter().find(|m| m.name == format!("get_{property}")));
        let bit = get.and_then(|m| asm.assembly_of(&t)?.il(m)).and_then(super::netvars::first_int);
        bit.and_then(|b| u32::try_from(b).ok()).and_then(|b| 1u16.checked_shl(b)).ok_or_else(|| format!("SceneObject.{property}: no flag bit of 16 ({bit:?})"))
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
        let b = self.behaviour(behaviour)?;
        let f = &b.fields;
        for unsupported in ["UseHalfFloatPrecision", "UseQuaternionSynchronization", "SyncScaleX", "SyncScaleY", "SyncScaleZ", "SlerpPosition", "SwitchTransformSpaceWhenParented"] {
            if yes(f, unsupported) {
                return Err(format!("{}: NetworkTransform.{unsupported} is not supported", b.class));
            }
        }
        if (node != root || self.graph.parent(node).is_some()) && !yes(f, "InLocalSpace") {
            return Err(format!("{} below the root or on a parented root syncs in world space: not supported (the local values are read)", b.class));
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
        let x = self.graph.xform(node).ok_or("no transform")?;
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
        let b = self.behaviour(object)?;
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
            if !parent_owned || (n != root && self.network_object_of(n)?.is_some()) {
                continue;
            }
            owned.insert(n);
            for (c, class) in components {
                if class != unity::MONO_BEHAVIOUR {
                    continue;
                }
                let ty = self.script_type(c)?;
                if !self.layouts.asm.derives_from(&ty, "Unity.Netcode.NetworkBehaviour") {
                    continue;
                }
                let index = behaviours.len();
                let fields = self.behaviour(c)?.fields;
                let vars = super::netvars::variables(&self.layouts.asm, &ty, &fields)?;
                behaviours.push(json!({"class": ty.name, "behaviourId": index, "vars": vars}));
                if self.layouts.asm.derives_from(&ty, "Unity.Netcode.Components.NetworkTransform") {
                    let mut state = self.transform_state(c, n, root)?;
                    state["behaviour"] = json!(index);
                    transforms.push(state);
                }
            }
        }
        let scale = self.graph.xform(root).ok_or("no transform")?.scale;
        let mut out = Map::new();
        out.insert("name".into(), json!(name));
        out.insert("hash".into(), json!(hash));
        out.insert("flags".into(), json!(flags));
        out.insert("parent".into(), if flags & self.scene_flag("HasParent")? != 0 { json!(0) } else { Value::Null });
        out.insert("behaviours".into(), Value::Array(behaviours));
        out.insert("transforms".into(), Value::Array(transforms));
        out.insert("scale".into(), json!(scale.map(super::tables::single)));
        // WorldObjectsHandler.InstantiateWorldObject gives the group to the root's own
        // GroupNetworkBase, and GroupNetworkBase.OnNetworkSpawn keeps it only on a root with
        // children: whether a world object spawned from this prefab shows its group hash.
        let root_base = self.graph.nodes.get(&root).map(|x| x.components.clone()).unwrap_or_default().into_iter().filter(|(_, class)| *class == unity::MONO_BEHAVIOUR).any(|(c, _)| self.script_type(c).is_ok_and(|ty| self.layouts.asm.derives_from(&ty, "SpaceCraft.GroupNetworkBase")));
        out.insert("groupHashOnSpawn".into(), json!(root_base && !self.graph.children(root).is_empty()));
        Ok(out)
    }

    /// The in-scene network objects of the game's main scene, in the order the server spawns
    /// them on start (file order), numbered from 2 as the game's own server numbers them.
    pub fn scene_objects(&mut self) -> Result<Vec<Value>, String> {
        let main = self.data.build_scenes()?.iter().position(|s| s.ends_with("/GameMainScene.unity")).ok_or("no GameMainScene in the build")?;
        let file = self.data.file(&format!("level{main}")).map_err(|e| e.to_string())?;
        self.graph_file(file)?;
        let behaviours: Vec<i64> = self.data.files[file].objects.iter().filter(|o| o.class_id == unity::MONO_BEHAVIOUR).map(|o| o.path).collect();
        let mut out = Vec::new();
        for path in behaviours {
            let id = Id(file, path);
            if self.graph.script(&mut self.data, id)?.is_none_or(|s| s.0 != "Unity.Netcode" || s.1 != "NetworkObject") {
                continue;
            }
            let mut entry = self.network_object(id, true)?;
            let parent = self.behaviour(id)?.game_object.and_then(|g| self.graph.parent(g));
            if let Some(p) = parent && self.network_object_of(p)?.is_some() {
                return Err(format!("{}: in-scene network objects below another are not supported", entry["name"]));
            }
            entry.insert("id".into(), json!(out.len() + 2));
            out.push(Value::Object(entry));
        }
        Ok(out)
    }

    /// The prefab of a network object (its root's NetworkObject) as a spawn describes it.
    pub fn prefab(&mut self, root: Id, player: bool) -> Entry {
        self.graph_file(root.0)?;
        let object = self.network_object_of(root)?.ok_or("prefab without a NetworkObject")?;
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
        let handler = self.find_behaviour(&["level4"], "SpaceCraft", "WorldObjectsHandler")?.ok_or("no WorldObjectsHandler")?;
        let fields = self.behaviour(handler)?.fields;
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
        let container = |ctx: &mut Ctx, name: &str| -> Result<Value, String> {
            let root = match field(&fields, name) {
                Some(Raw::Ref(p, _)) => ctx.data.resolve(handler.0, *p)?,
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
            self.graph_file(root.0)?;
            if self.network_object_of(root)?.is_some() {
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

impl Ctx {
    /// The first MonoBehaviour of a script class in the named scenes.
    pub fn find_in_scenes(&mut self, scenes: &[&str], namespace: &str, class: &str) -> Result<Option<Id>, String> {
        let mut names = Vec::new();
        for s in scenes {
            let f = self.scene_file(s)?;
            names.push(self.data.files[f].name.clone());
        }
        self.find_behaviour(&names.iter().map(String::as_str).collect::<Vec<_>>(), namespace, class)
    }

    /// The build file of a scene by name (scene N of the build is levelN).
    pub fn scene_file(&mut self, name: &str) -> Result<usize, String> {
        let scenes = self.data.build_scenes()?;
        let index = scenes.iter().position(|s| s.rsplit('/').next() == Some(&format!("{name}.unity"))).ok_or_else(|| format!("no scene {name} in the build (it starts {:?})", &scenes[..scenes.len().min(8)]))?;
        self.data.file(&format!("level{index}")).map_err(|e| format!("level{index}: {e}"))
    }

    /// GameData.Singletons: the game's MonoBehaviour classes with exactly one instance (inactive
    /// ones included) in the scenes a server on the planet has loaded (BootScene,
    /// GameMainScene, the planet's scene) and on the player, with that instance's fields.
    pub fn singletons(&mut self, planet_scene: &str, player: Id) -> Result<Map<String, Value>, String> {
        let mut found: Vec<Id> = Vec::new();
        for scene in ["BootScene", "GameMainScene", planet_scene] {
            let f = self.scene_file(scene)?;
            self.graph_file(f)?;
            found.extend(self.data.files[f].objects.iter().filter(|o| o.class_id == unity::MONO_BEHAVIOUR).map(|o| Id(f, o.path)));
        }
        self.graph_file(player.0)?;
        found.extend(self.graph.behaviours_in_children(&mut self.data, player)?.into_iter().map(|b| b.0));
        let mut by_class: indexmap::IndexMap<String, Vec<Id>> = Default::default();
        for id in found {
            let Some((namespace, class, _)) = self.graph.script(&mut self.data, id)?.cloned() else { continue };
            let attached = !self.data.reader(id).and_then(|mut r| unity::behaviour(&mut r)).ok_or_else(|| format!("{id:?}: unreadable MonoBehaviour"))?.game_object.is_null();
            if namespace == "SpaceCraft" && attached {
                by_class.entry(class).or_default().push(id);
            }
        }
        let mut out = Map::new();
        for (class, ids) in by_class {
            let [id] = ids[..] else { continue };
            let b = self.behaviour(id)?;
            out.insert(class.clone(), Value::Object(self.fields(id.0, &b.fields, 0).map_err(|e| format!("{class}: {e}"))?));
        }
        Ok(out)
    }
}

impl Ctx {
    fn hash_of(&mut self, from: usize, r: Option<&Raw>) -> Result<u32, String> {
        let root = match r {
            Some(Raw::Ref(p, _)) => self.data.resolve(from, *p)?.ok_or("a null network prefab")?,
            other => return Err(format!("a network prefab is {other:?}")),
        };
        let hash = self.prefab_hash(root)?.ok_or("a network prefab without a NetworkObject")?;
        u32::from_str_radix(&hash, 16).map_err(|e| e.to_string())
    }

    /// NetworkPrefab.SourcePrefabGlobalObjectIdHash of every prefab the NetworkPrefabsLists
    /// register, and of the player prefab (NetworkManager adds it).
    fn registered_prefabs(&mut self, file: usize, config: &[(String, Raw)]) -> Result<Vec<u32>, String> {
        let lists = match field(config, "Prefabs") {
            Some(Raw::Fields(_, f)) => match field(f, "NetworkPrefabsLists") {
                Some(Raw::List(l)) => l.clone(),
                other => return Err(format!("NetworkPrefabs.NetworkPrefabsLists: {other:?}")),
            },
            other => return Err(format!("NetworkConfig.Prefabs: {other:?}")),
        };
        let mut out = Vec::new();
        for list in lists {
            let Raw::Ref(p, _) = list else { return Err("a NetworkPrefabsList that is not a reference".into()) };
            let id = self.data.resolve(file, p)?.ok_or("a null NetworkPrefabsList")?;
            let b = self.behaviour(id)?;
            let Some(Raw::List(entries)) = field(&b.fields, "List").cloned() else { return Err("NetworkPrefabsList.List".into()) };
            for e in entries {
                let Raw::Fields(_, f) = e else { return Err("a NetworkPrefab that is not a struct".into()) };
                let hash = match field(&f, "Override") {
                    Some(Raw::Enum(0, ..)) => self.hash_of(id.0, field(&f, "Prefab"))?,
                    Some(Raw::Enum(1, ..)) => self.hash_of(id.0, field(&f, "SourcePrefabToOverride"))?,
                    Some(Raw::Enum(2, ..)) => match field(&f, "SourceHashToOverride") {
                        Some(Raw::UInt(h)) => *h as u32,
                        other => return Err(format!("NetworkPrefab.SourceHashToOverride: {other:?}")),
                    },
                    other => return Err(format!("NetworkPrefab.Override: {other:?}")),
                };
                out.push(hash);
            }
        }
        out.push(self.hash_of(file, field(config, "PlayerPrefab"))?);
        Ok(out)
    }

    /// NetworkConfig.GetConfig: XXHash64 of the settings both ends must share (ProtocolVersion,
    /// Netcode's version string, the prefab hashes when ForceSamePrefabs, TickRate,
    /// ConnectionApproval, ForceSamePrefabs, EnableSceneManagement,
    /// EnsureNetworkVariableLengthSafety, RpcHashSize), as FastBufferWriter writes them.
    pub fn config_hash(&mut self, file: usize, config: &[(String, Raw)]) -> Result<u64, String> {
        let asm = self.layouts.asm.clone();
        let t = TypeName { scope: Some("Unity.Netcode.Runtime".into()), namespace: "Unity.Netcode".into(), name: "NetworkConfig".into() };
        let get = asm.def(&t).and_then(|d| d.methods.iter().find(|m| m.name == "GetConfig")).ok_or("no NetworkConfig.GetConfig")?;
        let a = asm.assembly_of(&t).ok_or("no Unity.Netcode.Runtime assembly")?;
        let writes = config_writes(a, a.il(get).ok_or("GetConfig: no IL body")?)?;
        // What this hash writes, in order, with each WriteValueSafe<T>'s T (None: the string).
        let hash_size_type = TypeSig::ValueType(TypeName { scope: None, namespace: "Unity.Netcode".into(), name: "HashSize".into() });
        let expected: [(&str, Option<TypeSig>); 9] = [
            ("ProtocolVersion", Some(TypeSig::U2)),
            ("string", None),
            ("key", Some(TypeSig::U4)),
            ("TickRate", Some(TypeSig::U4)),
            ("ConnectionApproval", Some(TypeSig::Bool)),
            ("ForceSamePrefabs", Some(TypeSig::Bool)),
            ("EnableSceneManagement", Some(TypeSig::Bool)),
            ("EnsureNetworkVariableLengthSafety", Some(TypeSig::Bool)),
            ("RpcHashSize", Some(hash_size_type)),
        ];
        let same = writes.len() == expected.len() && writes.iter().zip(&expected).all(|((what, ty), (e, et))| what.starts_with('"') == (*e == "string") && (what.starts_with('"') || what == e) && unscoped(ty) == *et);
        if !same {
            return Err(format!("GetConfig writes {writes:?}, not the {expected:?} this hash is computed from (update config_hash)"));
        }
        let version = writes[1].0.trim_matches('"').to_string();
        let int = |n: &str| match field(config, n) {
            Some(Raw::UInt(v)) => Ok(*v),
            Some(Raw::Int(v)) => Ok(*v as u64),
            Some(Raw::Enum(v, ..)) => Ok(*v as u64),
            other => Err(format!("NetworkConfig.{n}: {other:?}")),
        };
        let flag = |n: &str| match field(config, n) {
            Some(Raw::Bool(b)) => Ok(*b as u8),
            other => Err(format!("NetworkConfig.{n}: {other:?}")),
        };
        let mut w = Vec::new();
        w.extend(u16::try_from(int("ProtocolVersion")?).map_err(|e| format!("NetworkConfig.ProtocolVersion: {e}"))?.to_le_bytes());
        // WriteValueSafe(string, oneByteChars: false): the length, then UTF-16 units.
        let units: Vec<u16> = version.encode_utf16().collect();
        w.extend((units.len() as u32).to_le_bytes());
        units.iter().for_each(|u| w.extend(u.to_le_bytes()));
        if flag("ForceSamePrefabs")? != 0 {
            // NetworkPrefabOverrideLinks' keys in order: each prefab's source hash.
            let mut hashes = self.registered_prefabs(file, config)?;
            hashes.sort();
            hashes.dedup();
            hashes.iter().for_each(|h| w.extend(h.to_le_bytes()));
        }
        w.extend(u32::try_from(int("TickRate")?).map_err(|e| format!("NetworkConfig.TickRate: {e}"))?.to_le_bytes());
        for n in ["ConnectionApproval", "ForceSamePrefabs", "EnableSceneManagement", "EnsureNetworkVariableLengthSafety"] {
            w.push(flag(n)?);
        }
        let hash_size = TypeName { scope: Some("Unity.Netcode.Runtime".into()), namespace: "Unity.Netcode".into(), name: "HashSize".into() };
        let size = match asm.enum_base(&hash_size) {
            Some(TypeSig::U1 | TypeSig::I1) => 1,
            Some(TypeSig::U2 | TypeSig::I2) => 2,
            Some(TypeSig::U4 | TypeSig::I4) => 4,
            Some(TypeSig::U8 | TypeSig::I8) => 8,
            other => return Err(format!("Unity.Netcode.HashSize: base type {other:?}")),
        };
        let rpc_hash_size = int("RpcHashSize")?;
        if size < 8 && rpc_hash_size >> (8 * size) != 0 {
            return Err(format!("NetworkConfig.RpcHashSize {rpc_hash_size} does not fit {size} bytes"));
        }
        w.extend(&rpc_hash_size.to_le_bytes()[..size]);
        Ok(xxhash_rust::xxh64::xxh64(&w, 0))
    }
}

/// A type without its assembly scope (to compare a signature's type with a name).
fn unscoped(t: &Option<TypeSig>) -> Option<TypeSig> {
    match t {
        Some(TypeSig::ValueType(n)) => Some(TypeSig::ValueType(TypeName { scope: None, ..n.clone() })),
        other => other.clone(),
    }
}

/// What NetworkConfig.GetConfig writes, in order: per WriteValueSafe call, what it writes (the
/// NetworkConfig field last loaded, a string literal in quotes, or "key" for a prefab override
/// link's key) and its generic argument.
fn config_writes(a: &super::dotnet::Assembly, il: &[u8]) -> Result<Vec<(String, Option<TypeSig>)>, String> {
    let mut last: Option<String> = None;
    let mut out = Vec::new();
    for (code, arg) in super::netvars::instructions(il)? {
        if code == 0x72 {
            last = Some(format!("\"{}\"", a.user_string(arg as u32).ok_or("GetConfig: an unreadable string literal")?));
            continue;
        }
        match (code, a.member(arg as u32)) {
            (0x7B | 0x7C, Some(Member::Field { owner: TypeSig::Class(o), name })) if o.namespace == "Unity.Netcode" && o.name == "NetworkConfig" => last = Some(name),
            (0x28 | 0x6F, Some(Member::Method { name, .. })) if name == "get_Key" => last = Some("key".into()),
            (0x28 | 0x6F, Some(Member::Method { name, generic, .. })) if name == "WriteValueSafe" => {
                let what = last.take().ok_or("GetConfig: a WriteValueSafe of something other than a field, string or key")?;
                out.push((what, generic.first().cloned()));
            }
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::tables::Ctx;

    /// ProxyMessage's version is RpcMessage's (its get_Version calls that one), and GetConfig
    /// writes what config_hash hashes.
    #[test]
    fn message_versions_follow_calls_and_the_config_writes_are_checked() {
        let Some(dir) = super::super::unity::tests::game_data() else { return };
        let mut ctx = Ctx::open(dir.parent().unwrap()).unwrap();
        let versions = ctx.message_versions().unwrap();
        let netcode = ctx.layouts.asm.0.iter().find(|a| a.name == "Unity.Netcode.Runtime").unwrap();
        let slot = |name: &str| netcode.types()[netcode.find("Unity.Netcode", "NetworkMessageTypes").unwrap()].fields.iter().find(|f| f.name == name).unwrap().constant.unwrap() as usize;
        assert_eq!(versions[slot("Proxy")][1], versions[slot("Rpc")][1]);
        assert!(versions.iter().filter(|v| v[0] != 0).count() == 25, "{versions:?}");
        let (file, config) = ctx.network_config().unwrap();
        ctx.config_hash(file, &config).unwrap();
    }
}
