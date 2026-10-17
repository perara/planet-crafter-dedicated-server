using System;
using System.Collections;
using System.Collections.Generic;
using System.Linq;
using System.Reflection;
using HarmonyLib;
using Newtonsoft.Json.Linq;
using SpaceCraft;
using Tessera;
using UnityEngine;
using UnityEngine.SceneManagement;

namespace PcdsExtractor
{
    // What the server needs to generate procedural wrecks (GenerateOnTrigger) and world instances
    // (WorldInstanceScene) itself: GET /api/wreckdata, and "instanceScenes" in GET /api/scenedata.
    //
    // A wreck's tiles are laid out by Tessera (a wave-function-collapse solver) from the wreck's
    // seed. The server does not run Tessera: the game lays out a pool of seeds here and the server
    // gives its wrecks those seeds. What follows the layout (the RandomizePropData each tile holds,
    // picked with Unity.Mathematics.Random) is dumped as data the server walks as the game does.
    internal static class Generation
    {
        // Seeds 1..Seeds of every generator.
        internal const int Seeds = 64;

        internal static JObject Wrecks()
        {
            Props props = new Props();
            JArray generators = new JArray();
            foreach (object entry in (IEnumerable)AccessTools.Field(typeof(ProceduralInstancesHandler), "_generators").GetValue(ProceduralInstancesHandler.Instance))
            {
                GameObject prefab = Traverse.Create(entry).Field("generator").GetValue<GameObject>();
                Traverse wreck = Traverse.Create(prefab.GetComponent<GenerateOnTrigger>());
                GameObject tessera = wreck.Field("_generator").GetValue<TesseraGenerator>().gameObject;
                GameObject container = wreck.Field("_randomGeneratedPropsContainer").GetValue<GameObject>();
                // GenerateRandomPropsFromRoot: the generator's other children, each walked from where
                // it sits under the generator.
                JArray children = new JArray();
                foreach (Transform child in prefab.transform)
                {
                    string key = child.gameObject == tessera || child.gameObject == container ? null : props.Key(child.gameObject, prefab.name + "/" + child.name);
                    if (key != null)
                    {
                        children.Add(new JObject { ["props"] = key, ["ltw"] = Affine(Matrix4x4.TRS(child.localPosition, child.localRotation, child.localScale)) });
                    }
                }
                generators.Add(new JObject
                {
                    ["prefab"] = prefab.name,
                    ["scale"] = Vector(prefab.transform.localScale),
                    ["generator"] = Affine(prefab.transform.worldToLocalMatrix * tessera.transform.localToWorldMatrix),
                    ["container"] = Affine(prefab.transform.worldToLocalMatrix * container.transform.localToWorldMatrix),
                    ["children"] = children,
                    ["limitedGroups"] = new JArray(((IEnumerable)wreck.Field("_limitedGroups").GetValue()).Cast<object>().Select(l => new JArray(Traverse.Create(l).Field("groupData").GetValue<GroupData>()?.id, Traverse.Create(l).Field("maxQuantity").GetValue<int>()))),
                    ["sceneInstanceRadiusRatio"] = wreck.Field("_sceneInstanceRadiusRatio").GetValue<float>(),
                    ["triggers"] = new JArray(prefab.GetComponents<Collider>().Where(c => c.isTrigger).Select(Shape)),
                    ["layouts"] = Layouts(prefab, props),
                });
            }
            return new JObject { ["seeds"] = Seeds, ["generators"] = generators, ["props"] = props.Finish() };
        }

        // GenerateOnTrigger.Generate up to its layout, per seed on a fresh copy of the generator far
        // from everything (Tessera also takes the scene's tiles in its grid as constraints): the
        // generator's other children walked first (GenerateRandomPropsFromRoot moves the props it
        // picks, some of which pin Tessera tiles), then Tessera from the seed, the next seed while
        // it fails. The tiles in the order Tessera lists them, under the generator.
        static JObject Layouts(GameObject prefab, Props props)
        {
            MethodInfo walk = AccessTools.Method(typeof(GenerateOnTrigger), "GenerateRandomPropsFromRoot");
            JObject layouts = new JObject();
            for (uint seed = 1; seed <= Seeds; seed++)
            {
                GameObject copy = UnityEngine.Object.Instantiate(prefab, new Vector3(0f, -20000f, 0f), Quaternion.identity);
                try
                {
                    GenerateOnTrigger wreck = copy.GetComponent<GenerateOnTrigger>();
                    TesseraGenerator generator = Traverse.Create(wreck).Field("_generator").GetValue<TesseraGenerator>();
                    GameObject container = Traverse.Create(wreck).Field("_randomGeneratedPropsContainer").GetValue<GameObject>();
                    object[] args = { null, new Unity.Mathematics.Random(seed) };
                    foreach (GameObject child in copy.transform.Cast<Transform>().Select(t => t.gameObject).Where(g => g != generator.gameObject && g != container).ToList())
                    {
                        args[0] = child;
                        walk.Invoke(wreck, args);
                    }
                    IList<TesseraTileInstance> tiles = null;
                    for (uint current = seed; tiles == null && current < seed + 50; current++)
                    {
                        generator.Generate(new TesseraGenerateOptions { seed = (int)current, onComplete = c => tiles = c.success ? c.tileInstances : null });
                    }
                    layouts[seed.ToString()] = new JArray(tiles.Select(t => (key: props.Key(t.Tile.gameObject), t)).Where(x => x.key != null).Select(x => new JArray(x.key, Vector(x.t.LocalPosition), Vector(x.t.LocalRotation))));
                }
                finally
                {
                    // OnDestroy unregisters a never-spawned generator under owner -1.
                    ProceduralInstancesHandler.Instance.RegisterClientInstance(copy, -1);
                    UnityEngine.Object.DestroyImmediate(copy);
                }
            }
            return layouts;
        }

        // RandomizePropData lists, keyed by the object a generation walks from (a tile, a prop, a
        // generator's child): the components in GetComponentsInChildrenOrdered order, each with its
        // matrix as InstantiateRandomPropsInHierarchy takes it (the root at the origin, unrotated,
        // its scale kept), its random props and the components it makes ignore.
        sealed class Props
        {
            readonly Dictionary<GameObject, string> _keys = new Dictionary<GameObject, string>();
            readonly Dictionary<RandomizePropData, JArray> _ids = new Dictionary<RandomizePropData, JArray>();
            readonly List<(RandomizePropData component, JObject entry)> _entries = new List<(RandomizePropData, JObject)>();
            readonly JObject _lists = new JObject();

            // The key of root's list; null when there is nothing to walk.
            internal string Key(GameObject root, string name = null)
            {
                if (root == null)
                {
                    return null;
                }
                if (_keys.TryGetValue(root, out string known))
                {
                    return known;
                }
                List<RandomizePropData> list = new List<RandomizePropData>();
                Ordered(root.transform, list);
                string key = null;
                if (list.Count > 0)
                {
                    key = name ?? root.name;
                    for (int n = 2; _lists.ContainsKey(key); n++)
                    {
                        key = (name ?? root.name) + "#" + n;
                    }
                    _lists[key] = new JArray();
                }
                _keys[root] = key;
                Matrix4x4 origin = Matrix4x4.Scale(root.transform.localScale) * root.transform.worldToLocalMatrix;
                for (int i = 0; i < list.Count; i++)
                {
                    _ids[list[i]] = new JArray(key, i);
                }
                foreach (RandomizePropData p in list)
                {
                    JObject entry = new JObject
                    {
                        ["m"] = Affine(origin * p.transform.localToWorldMatrix),
                        ["network"] = p.networkObjects,
                        ["noCountLimited"] = p.noCountLimited,
                        // [chance, randomizeRotationY, group, key of the prop's own list (walked next)]
                        ["props"] = new JArray(p.props.Select(d => new JArray(d.chancePourcentage, d.randomizeRotationY,
                            d.prop == null ? null : GroupsHandler.GetGroupsViaGameObject(d.prop)?.GetId(), p.networkObjects ? null : Key(d.prop)))),
                    };
                    ((JArray)_lists[key]).Add(entry);
                    _entries.Add((p, entry));
                }
                return key;
            }

            // ignoreIfGenerated as [key, index]; components never walked are left out.
            internal JObject Finish()
            {
                foreach ((RandomizePropData component, JObject entry) in _entries)
                {
                    JArray ignore = new JArray((component.ignoreIfGenerated ?? new RandomizePropData[0]).Where(c => c != null && _ids.ContainsKey(c)).Select(c => _ids[c]));
                    if (ignore.Count > 0)
                    {
                        entry["ignore"] = ignore;
                    }
                }
                return _lists;
            }
        }

        // GenerateOnTrigger.GetComponentsInChildrenOrdered.
        static void Ordered(Transform t, List<RandomizePropData> result)
        {
            if (!t.gameObject.activeSelf)
            {
                return;
            }
            RandomizePropData c = t.GetComponent<RandomizePropData>();
            if (c != null)
            {
                result.Add(c);
            }
            for (int i = 0; i < t.childCount; i++)
            {
                Ordered(t.GetChild(i), result);
            }
        }

        // The current planet's world instance scenes, each loaded and placed as
        // WorldInstanceHandler.HandleDataAfterLoad places it, before InitSceneVariables.
        internal static IEnumerator InstanceScenes(Action<JToken> done)
        {
            JObject scenes = new JObject();
            WorldInstanceGeneratorData data = Managers.GetManager<PlanetLoader>().GetCurrentPlanetData()?.instanceScenesData;
            foreach (string name in data == null ? new List<string>() : data.instanceSceneNames.Distinct().ToList())
            {
                JObject dump = null;
                AsyncOperation load = SceneManager.LoadSceneAsync(name, LoadSceneMode.Additive);
                load.completed += _ =>
                {
                    try
                    {
                        dump = InstanceScene(SceneManager.GetSceneByName(name));
                    }
                    catch (Exception e)
                    {
                        Extractor.Log.LogWarning($"PCDS instance scene {name}: {e}");
                        dump = new JObject { ["error"] = e.Message };
                    }
                };
                while (dump == null)
                {
                    yield return null;
                }
                yield return SceneManager.UnloadSceneAsync(name);
                scenes[name] = dump;
            }
            done(scenes);
        }

        static JObject InstanceScene(Scene scene)
        {
            GameObject target = GameObject.Find("WorldInstancePositionToInstantiate") ?? UnityEngine.Object.FindFirstObjectByType<WorldInstanceHandler>().positionToInstantiate;
            GameObject[] roots = scene.GetRootGameObjects();
            foreach (GameObject root in roots)
            {
                root.transform.position = target.transform.position;
            }
            WorldInstanceScene instance = roots.Select(r => r.GetComponentInChildren<WorldInstanceScene>()).First(c => c != null);
            // The objects whose activity matters, with their parents: [parent, activeSelf, name].
            Dictionary<GameObject, int> ids = new Dictionary<GameObject, int>();
            JArray nodes = new JArray();
            int Node(GameObject go)
            {
                if (go == null)
                {
                    return -1;
                }
                if (!ids.TryGetValue(go, out int id))
                {
                    int parent = go.transform.parent == null ? -1 : Node(go.transform.parent.gameObject);
                    id = ids[go] = nodes.Count;
                    nodes.Add(new JArray(parent, go.activeSelf, go.name));
                }
                return id;
            }
            Dictionary<WorldVariationObject, int> objectIds = new Dictionary<WorldVariationObject, int>();
            JArray objects = new JArray();
            int Toggle(WorldVariationObject o)
            {
                if (!objectIds.TryGetValue(o, out int id))
                {
                    id = objectIds[o] = objects.Count;
                    objects.Add(new JObject { ["node"] = Node(o.gameObject), ["whole"] = o.wholeGameObject, ["whenEnabled"] = Node(o.whenEnabled), ["whenDisabled"] = Node(o.whenDisabled) });
                }
                return id;
            }
            JArray variations = new JArray(instance.worldVariations.Select(v => new JObject
            {
                ["everyTime"] = new JArray(v.objectsToEnableEveryTime.Select(Toggle)),
                // [min, max (exclusive), objects] as RandomizeObjectApparition rounds them
                ["random"] = new JArray(new[]
                {
                    (v.numberOfThoseObjects1, v.randomObjects1), (v.numberOfThoseObjects2, v.randomObjects2), (v.numberOfThoseObjects3, v.randomObjects3),
                    (v.numberOfThoseObjects4, v.randomObjects4), (v.numberOfThoseObjects5, v.randomObjects5), (v.numberOfThoseObjects6, v.randomObjects6),
                }.Select(g => new JArray(Mathf.RoundToInt(g.Item1.x), Mathf.RoundToInt(g.Item1.y) + 1, new JArray(g.Item2.Select(Toggle))))),
            }));
            // Every RandomizePropData under the scene component, active or not, in walk order.
            List<RandomizePropData> all = new List<RandomizePropData>();
            void Walk(Transform t)
            {
                RandomizePropData c = t.GetComponent<RandomizePropData>();
                if (c != null)
                {
                    all.Add(c);
                }
                foreach (Transform child in t)
                {
                    Walk(child);
                }
            }
            Walk(instance.transform);
            Transform preventBuilding = GameObjects.FindRecursively(instance.transform.root, "PreventBuilding");
            Collider collider = preventBuilding == null ? null : preventBuilding.GetComponent<Collider>();
            return new JObject
            {
                ["root"] = Node(instance.gameObject),
                ["variations"] = variations,
                ["objects"] = objects,
                ["wreckEntries"] = new JArray(instance.wreckEntries.Select(e => new JObject { ["node"] = Node(e), ["position"] = Vector(e.transform.position), ["rotation"] = Vector(e.transform.rotation) })),
                // [node, position, rotation, [[chance, group]...], [ignored indices]]
                ["props"] = new JArray(all.Select(p => new JArray(Node(p.gameObject), Vector(p.transform.position), Vector(p.transform.rotation),
                    new JArray(p.props.Select(d => new JArray(d.chancePourcentage, d.prop == null ? null : GroupsHandler.GetGroupsViaGameObject(d.prop)?.GetId()))),
                    new JArray((p.ignoreIfGenerated ?? new RandomizePropData[0]).Select(x => all.IndexOf(x)).Where(i => i >= 0))))),
                ["preventBuilding"] = collider == null ? null : Shape(collider),
                ["nodes"] = nodes,
            };
        }

        // A collider in world space (a prefab's: relative to its root).
        static JObject Shape(Collider c)
        {
            JObject shape = new JObject
            {
                ["type"] = c.GetType().Name,
                ["active"] = c.enabled && c.gameObject.activeInHierarchy,
                ["position"] = Vector(c.transform.position),
                ["rotation"] = Vector(c.transform.rotation),
                ["scale"] = Vector(c.transform.lossyScale),
            };
            switch (c)
            {
                case BoxCollider b:
                    shape["center"] = Vector(b.center);
                    shape["size"] = Vector(b.size);
                    break;
                case SphereCollider s:
                    shape["center"] = Vector(s.center);
                    shape["radius"] = s.radius;
                    break;
                case CapsuleCollider k:
                    shape["center"] = Vector(k.center);
                    shape["radius"] = k.radius;
                    shape["height"] = k.height;
                    shape["direction"] = k.direction;
                    break;
            }
            return shape;
        }

        // Rows 0-2 of a Matrix4x4 (the last is 0 0 0 1 for these).
        static JArray Affine(Matrix4x4 m)
        {
            return new JArray(m.m00, m.m01, m.m02, m.m03, m.m10, m.m11, m.m12, m.m13, m.m20, m.m21, m.m22, m.m23);
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
