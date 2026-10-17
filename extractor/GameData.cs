using System;
using System.Collections;
using System.Collections.Generic;
using System.Linq;
using System.Reflection;
using System.Threading;
using HarmonyLib;
using Newtonsoft.Json.Linq;
using SpaceCraft;
using Unity.Collections;
using Unity.Netcode;
using UnityEngine;

namespace PcdsExtractor
{
    // What the standalone server needs from the game, read at runtime once per game version:
    // every game asset (groups, planets, terraform stages, events...) with its serialized fields,
    // the game components on each group's prefab, the scene's singletons and world objects, and
    // the exact network layout of every world object the server can spawn. GET /api/gamedata.
    // Spawning the layouts creates and destroys objects in the loaded world: run it on a
    // throwaway server (pcds oracle), never on a world you play.
    internal static class GameData
    {
        const int MaxDepth = 6;
        static readonly MethodInfo GetSceneObject = AccessTools.Method(typeof(NetworkObject), "GetMessageSceneObject");

        // layouts = false: only what the loaded planet's scene holds (GET /api/scenedata), without
        // spawning anything; run after travelling the oracle to each planet.
        internal static JObject Build(bool layouts = true)
        {
            JObject result = Extractor.OnMain(() => new JObject
            {
                ["gameVersion"] = Application.version,
                ["planet"] = Managers.GetManager<PlanetLoader>().GetCurrentPlanetData()?.id,
                ["assets"] = Assets(),
                ["groupPrefabs"] = GroupPrefabs(),
                ["singletons"] = Singletons(),
                ["sceneWorldObjects"] = SceneWorldObjects(),
                ["waterVolumes"] = WaterVolumes(),
                ["oreVeins"] = OreVeins(),
            }, 120000);
            if (layouts)
            {
                result["layouts"] = Layouts();
            }
            return result;
        }

        // Every ScriptableObject of the game's own types, keyed "Type:name".
        static JObject Assets()
        {
            JObject assets = new JObject();
            foreach (ScriptableObject asset in Resources.FindObjectsOfTypeAll<ScriptableObject>().Where(IsGameObjectType).OrderBy(a => a.name))
            {
                assets[Ref(asset)] = Fields(asset, 0);
            }
            return assets;
        }

        static JObject GroupPrefabs()
        {
            JObject prefabs = new JObject();
            foreach (Group group in GroupsHandler.GetAllGroups())
            {
                GameObject prefab = group.GetAssociatedGameObject();
                if (prefab != null)
                {
                    // rigidbody: GroupNetworkContainer.CreateChild keeps server ownership of such items.
                    prefabs[group.GetId()] = new JObject { ["prefab"] = prefab.name, ["rigidbody"] = prefab.GetComponent<Rigidbody>() != null, ["components"] = Components(prefab) };
                }
            }
            return prefabs;
        }

        static JArray Components(GameObject root)
        {
            return new JArray(root.GetComponentsInChildren<MonoBehaviour>(true).Where(c => c != null && IsGameObjectType(c)).Select(c =>
            {
                JObject component = new JObject
                {
                    ["type"] = c.GetType().Name,
                    ["path"] = RelativePath(root.transform, c.transform),
                    ["fields"] = Fields(c, 0),
                };
                JObject poses = Poses(root.transform, c);
                if (poses.Count > 0)
                {
                    component["poses"] = poses;
                }
                return component;
            }));
        }

        // Where a component's serialized Transform fields point, relative to the root: spots that
        // come placed with their building (MachineDeparturePlatform._landingPosition).
        static JObject Poses(Transform root, MonoBehaviour c)
        {
            JObject poses = new JObject();
            for (Type t = c.GetType(); t != null && t != typeof(MonoBehaviour) && t != typeof(NetworkBehaviour); t = t.BaseType)
            {
                foreach (FieldInfo f in t.GetFields(BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.DeclaredOnly))
                {
                    bool serialized = (f.IsPublic && !f.IsNotSerialized) || f.GetCustomAttribute<SerializeField>() != null;
                    if (serialized && f.FieldType == typeof(Transform) && f.GetValue(c) is Transform target && target != null && !poses.ContainsKey(f.Name))
                    {
                        poses[f.Name] = new JObject
                        {
                            ["position"] = Vector(root.InverseTransformPoint(target.position)),
                            ["rotation"] = Vector(Quaternion.Inverse(root.rotation) * target.rotation),
                        };
                    }
                }
            }
            return poses;
        }

        // Game components that exist once in the loaded scenes: the handlers and managers.
        static JObject Singletons()
        {
            JObject singletons = new JObject();
            // A joined player's avatar would make every player component look like several
            // instances; only the extractor's own (hidden) host avatar counts.
            Func<MonoBehaviour, bool> onJoinedPlayer = c =>
            {
                PlayerMainController player = c.GetComponentInParent<PlayerMainController>(true);
                return player != null && !player.IsOwner;
            };
            foreach (IGrouping<Type, MonoBehaviour> type in UnityEngine.Object.FindObjectsByType<MonoBehaviour>(FindObjectsInactive.Include, FindObjectsSortMode.None).Where(IsGameObjectType).Where(c => !onJoinedPlayer(c)).GroupBy(c => c.GetType()))
            {
                if (type.Count() == 1)
                {
                    singletons[type.Key.Name] = Fields(type.First(), 0);
                }
            }
            return singletons;
        }

        // World objects that come with the planet's scene (ids below 200 000 000).
        static JArray SceneWorldObjects()
        {
            return new JArray(UnityEngine.Object.FindObjectsByType<WorldObjectAssociated>(FindObjectsInactive.Include, FindObjectsSortMode.None)
                .Select(a => (associated: a, wo: a.GetWorldObject()))
                .Where(x => x.wo != null && WorldObjectsIdHandler.IsWorldObjectFromScene(x.wo.GetId()))
                .OrderBy(x => x.wo.GetId())
                .Select(x => new JObject
                {
                    ["id"] = x.wo.GetId(),
                    ["group"] = x.wo.GetGroup()?.GetId(),
                    ["position"] = Vector(x.associated.transform.position),
                    ["rotation"] = Vector(x.associated.transform.rotation),
                    ["components"] = Components(x.associated.gameObject),
                }));
        }

        // Water boxes the game tests "underwater" against (WaterHandler), with the transform of
        // the object that rises with terraformation (WorldUnitPositioning) at dump time.
        static JArray WaterVolumes()
        {
            return new JArray(UnityEngine.Object.FindObjectsByType<WaterVolume>(FindObjectsInactive.Include, FindObjectsSortMode.None).Select(v =>
            {
                BoxCollider box = v.GetComponentInChildren<BoxCollider>(true);
                WorldUnitPositioning positioning = v.GetComponent<WorldUnitPositioning>();
                return new JObject
                {
                    ["ignoreForEnergyDetection"] = v.ignoreForEnergyDetection,
                    ["active"] = v.gameObject.activeInHierarchy,
                    ["root"] = new JObject { ["position"] = Vector(v.transform.position), ["rotation"] = Vector(v.transform.rotation), ["scale"] = Vector(v.transform.lossyScale) },
                    ["box"] = box == null ? null : new JObject
                    {
                        ["center"] = Vector(box.center),
                        ["size"] = Vector(box.size),
                        ["position"] = Vector(box.transform.position),
                        ["rotation"] = Vector(box.transform.rotation),
                        ["scale"] = Vector(box.transform.lossyScale),
                    },
                    ["positioning"] = positioning == null ? null : Fields(positioning, 0),
                    ["activators"] = Activators(v.transform),
                };
            }));
        }

        // Ore veins (MachineGenerationGroupVein) with the colliders on the vein's own object: an
        // extractor's mining ray (MachineGenerator.SetMiningRayGeneration) hits those.
        static JArray OreVeins()
        {
            return new JArray(UnityEngine.Object.FindObjectsByType<MachineGenerationGroupVein>(FindObjectsInactive.Include, FindObjectsSortMode.None).Select(v => new JObject
            {
                ["id"] = v.TryGetComponent<WorldUniqueId>(out WorldUniqueId id) ? id.GetWorldUniqueId() : 0,
                ["active"] = v.gameObject.activeInHierarchy,
                ["activators"] = Activators(v.transform),
                ["fields"] = Fields(v, 0),
                ["colliders"] = new JArray(v.GetComponents<Collider>().Select(c => new JObject
                {
                    ["enabled"] = c.enabled,
                    ["boundsCenter"] = Vector(c.bounds.center),
                    ["boundsSize"] = Vector(c.bounds.size),
                    ["box"] = c is BoxCollider b ? new JObject
                    {
                        ["center"] = Vector(b.center),
                        ["size"] = Vector(b.size),
                        ["position"] = Vector(b.transform.position),
                        ["rotation"] = Vector(b.transform.rotation),
                        ["scale"] = Vector(b.transform.lossyScale),
                    } : null,
                })),
            }));
        }

        // What shows or hides an object with terraformation, on it and on its parents:
        // HideDependingOnTerraStage (once, at load) and WorldUnitPositioning (hideBefore/After).
        static JArray Activators(Transform t)
        {
            JArray list = new JArray();
            for (; t != null; t = t.parent)
            {
                foreach (HideDependingOnTerraStage h in t.GetComponents<HideDependingOnTerraStage>())
                {
                    list.Add(new JObject { ["type"] = "HideDependingOnTerraStage", ["fields"] = Fields(h, 0) });
                }
                foreach (WorldUnitPositioning p in t.GetComponents<WorldUnitPositioning>())
                {
                    list.Add(new JObject { ["type"] = "WorldUnitPositioning", ["fields"] = Fields(p, 0) });
                }
            }
            return list;
        }

        // Spawns one world object per group (and both container variants for items), lets them
        // initialise for a second, records what a joining client would receive, then removes them.
        static JObject Layouts()
        {
            List<(string key, WorldObject wo, GameObject go)> spawned = Extractor.OnMain(() =>
            {
                List<(string, WorldObject, GameObject)> list = new List<(string, WorldObject, GameObject)>();
                Vector3 far = new Vector3(0f, -5000f, 0f);
                foreach (Group group in GroupsHandler.GetAllGroups().Where(g => g.GetAssociatedGameObject() != null))
                {
                    bool ownPrefab = group.GetAssociatedGameObject().GetComponent<NetworkObject>() != null;
                    foreach (bool fromDb in ownPrefab || group.GetInventorySize() > 0 ? new[] { false } : new[] { false, true })
                    {
                        try
                        {
                            WorldObject wo = WorldObjectsHandler.Instance.CreateNewWorldObject(group, 0, null, save: false);
                            wo.SetPositionAndRotation(far, Quaternion.identity);
                            GameObject go = WorldObjectsHandler.Instance.InstantiateWorldObject(wo, fromDb, 0uL);
                            if (go != null)
                            {
                                list.Add((group.GetId() + (fromDb ? "#static" : ""), wo, go));
                            }
                        }
                        catch (Exception e)
                        {
                            Extractor.Log.LogWarning($"PCDS gamedata: {group.GetId()} did not spawn: {e.Message}");
                        }
                    }
                }
                return list;
            }, 300000);
            Thread.Sleep(1500);
            return Extractor.OnMain(() =>
            {
                JObject layouts = new JObject();
                foreach ((string key, WorldObject wo, GameObject go) in spawned)
                {
                    try
                    {
                        NetworkObject networkObject = go.GetComponent<NetworkObject>();
                        object sceneObject = GetSceneObject.Invoke(networkObject, new object[] { 0uL, false });
                        FastBufferWriter writer = new FastBufferWriter(1024, Allocator.Temp, 1 << 20);
                        try
                        {
                            sceneObject.GetType().GetMethod("Serialize").Invoke(sceneObject, new object[] { writer });
                            JObject entry = Manifest.Object(networkObject);
                            entry["prefab"] = go.name.Replace("(Clone)", "");
                            entry["sceneObject"] = BitConverter.ToString(writer.ToArray()).Replace("-", "").ToLowerInvariant();
                            entry["inventories"] = new JArray(wo.GetLinkedInventoryId() != 0 ? 1 : 0, wo.GetSecondaryInventoriesId().Count);
                            layouts[key] = entry;
                        }
                        finally
                        {
                            writer.Dispose();
                        }
                    }
                    catch (Exception e)
                    {
                        Extractor.Log.LogWarning($"PCDS gamedata: {key} layout failed: {e.Message}");
                    }
                    WorldObjectsHandler.Instance.DestroyWorldObject(wo, destroyGameObject: true);
                }
                return layouts;
            }, 300000);
        }

        static bool IsGameObjectType(UnityEngine.Object o)
        {
            return o != null && o.GetType().Namespace == "SpaceCraft";
        }

        static string Ref(UnityEngine.Object o)
        {
            return o.GetType().Name + ":" + o.name;
        }

        static string RelativePath(Transform root, Transform t)
        {
            return t == root ? "" : (t.parent == root ? t.name : RelativePath(root, t.parent) + "/" + t.name);
        }

        // Unity's serialized fields: public or [SerializeField], not static, not [NonSerialized].
        static JObject Fields(object target, int depth)
        {
            JObject fields = new JObject();
            for (Type t = target.GetType(); t != null && t != typeof(MonoBehaviour) && t != typeof(ScriptableObject) && t != typeof(NetworkBehaviour) && t != typeof(object); t = t.BaseType)
            {
                foreach (FieldInfo f in t.GetFields(BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.DeclaredOnly))
                {
                    bool serialized = (f.IsPublic && !f.IsNotSerialized) || f.GetCustomAttribute<SerializeField>() != null;
                    if (!serialized || fields.ContainsKey(f.Name))
                    {
                        continue;
                    }
                    JToken value = Value(f.GetValue(target), depth + 1);
                    if (value != null)
                    {
                        fields[f.Name] = value;
                    }
                }
            }
            return fields;
        }

        // A field value as JSON; null when the type carries nothing the server can use.
        static JToken Value(object v, int depth)
        {
            switch (v)
            {
                case null:
                    return JValue.CreateNull();
                case UnityEngine.Object destroyed when destroyed == null:
                    return JValue.CreateNull();
                case string s:
                    return s;
                case bool b:
                    return b;
                case Enum e:
                    return e.ToString();
                case float f:
                    return f;
                case double d:
                    return d;
                case int _:
                case uint _:
                case long _:
                case ulong _:
                case short _:
                case ushort _:
                case byte _:
                case sbyte _:
                    return JToken.FromObject(v);
                case Vector2 v2:
                    return new JArray(v2.x, v2.y);
                case Vector3 v3:
                    return Vector(v3);
                case Vector4 v4:
                    return new JArray(v4.x, v4.y, v4.z, v4.w);
                case Quaternion q:
                    return Vector(q);
                case Vector2Int i2:
                    return new JArray(i2.x, i2.y);
                case Vector3Int i3:
                    return new JArray(i3.x, i3.y, i3.z);
                case Color c:
                    return new JArray(c.r, c.g, c.b, c.a);
                case LayerMask m:
                    return m.value;
                case AnimationCurve curve:
                    return new JArray(curve.keys.Select(k => new JArray(k.time, k.value, k.inTangent, k.outTangent)));
                case ScriptableObject so:
                    return IsGameObjectType(so) ? Ref(so) : null;
                case GameObject go:
                    return "GameObject:" + go.name;
                case Component component:
                    return component.GetType().Name + "@" + component.gameObject.name;
                case UnityEngine.Object _:
                    return null;
                case Delegate _:
                    return null;
                case IDictionary dictionary:
                    if (depth > MaxDepth)
                    {
                        return null;
                    }
                    // Serialized dictionaries (e.g. SerializedDictionary<GroupData, int>): [key, value] pairs.
                    return new JArray(dictionary.Keys.Cast<object>().Select(k => new JArray(Value(k, depth + 1) ?? JValue.CreateNull(), Value(dictionary[k], depth + 1) ?? JValue.CreateNull())));
                case IList list:
                    if (depth > MaxDepth)
                    {
                        return null;
                    }
                    return new JArray(list.Cast<object>().Select(i => Value(i, depth + 1) ?? JValue.CreateNull()));
            }
            Type type = v.GetType();
            if (depth > MaxDepth || !type.IsSerializable || type.Namespace != "SpaceCraft")
            {
                return null;
            }
            return Fields(v, depth);
        }

        static JArray Vector(Vector3 v)
        {
            return new JArray(v.x, v.y, v.z);
        }

        static JArray Vector(Quaternion q)
        {
            return new JArray(q.x, q.y, q.z, q.w);
        }
    }
}
