using System.Collections;
using System.Collections.Generic;
using System.Linq;
using System.Reflection;
using HarmonyLib;
using Newtonsoft.Json.Linq;
using SpaceCraft;
using Unity.Collections;
using Unity.Netcode;
using UnityEngine;

namespace PcdsExtractor
{
    // Debug aid for the standalone server: what the real server has spawned, which behaviours each
    // object carries, and every network variable's type, value and exact serialized bytes, plus
    // the prefab and group tables. GET /api/manifest; extracted once per game version.
    internal static class Manifest
    {
        static readonly MethodInfo WriteSync = AccessTools.Method(typeof(NetworkVariableBase), "WriteFieldSynchronization");

        internal static JObject Build()
        {
            NetworkManager manager = NetworkManager.Singleton;
            NetworkConfig config = manager.NetworkConfig;
            return new JObject
            {
                ["gameVersion"] = Application.version,
                ["config"] = new JObject
                {
                    ["tickRate"] = config.TickRate,
                    ["lengthSafety"] = config.EnsureNetworkVariableLengthSafety,
                    ["sceneManagement"] = config.EnableSceneManagement,
                    ["forceSamePrefabs"] = config.ForceSamePrefabs,
                    ["protocolVersion"] = config.ProtocolVersion,
                    ["playerPrefab"] = HashOf(config.PlayerPrefab),
                },
                ["objects"] = new JArray(manager.SpawnManager.SpawnedObjectsList.OrderBy(o => o.NetworkObjectId).Select(Object)),
                ["prefabs"] = new JArray(config.Prefabs.Prefabs.Where(p => p.Prefab != null).Select(p => new JObject
                {
                    ["hash"] = HashOf(p.Prefab),
                    ["name"] = p.Prefab.name,
                    ["behaviours"] = new JArray(p.Prefab.GetComponentsInChildren<NetworkBehaviour>(true).Select(b => b.GetType().Name)),
                })),
                ["planets"] = new JArray(Managers.GetManager<PlanetLoader>().planetList.GetPlanetList(allPlanets: true).Select(p => new JObject
                {
                    ["id"] = p.id,
                    ["hash"] = p.GetPlanetHash(),
                    ["steamAppId"] = p.steamAppId,
                    ["startingPlanet"] = p.startingPlanet,
                })),
                ["groups"] = new JArray(GroupsHandler.GetAllGroups().Select(g => new JObject
                {
                    ["id"] = g.GetId(),
                    ["hash"] = g.stableHashCode,
                    ["prefab"] = g.GetAssociatedGameObject() == null ? null : g.GetAssociatedGameObject().name,
                    ["prefabHash"] = HashOf(g.GetAssociatedGameObject()),
                })),
            };
        }

        static JToken HashOf(GameObject prefab)
        {
            NetworkObject networkObject = prefab == null ? null : prefab.GetComponent<NetworkObject>();
            return networkObject == null ? null : JToken.FromObject(((uint)Traverse.Create(networkObject).Field("GlobalObjectIdHash").GetValue()).ToString("x8"));
        }

        internal static JObject Object(NetworkObject o)
        {
            List<NetworkBehaviour> behaviours = Traverse.Create(o).Property("ChildNetworkBehaviours").GetValue<List<NetworkBehaviour>>();
            return new JObject
            {
                ["id"] = o.NetworkObjectId,
                ["hash"] = ((uint)Traverse.Create(o).Field("GlobalObjectIdHash").GetValue()).ToString("x8"),
                ["name"] = o.name,
                ["path"] = PathOf(o.transform),
                ["owner"] = o.OwnerClientId,
                ["scene"] = o.IsSceneObject ?? true,
                ["player"] = o.IsPlayerObject,
                ["position"] = new JArray(o.transform.position.x, o.transform.position.y, o.transform.position.z),
                ["behaviours"] = new JArray(behaviours.Select(Behaviour)),
            };
        }

        static JObject Behaviour(NetworkBehaviour b)
        {
            List<NetworkVariableBase> variables = Traverse.Create(b).Field("NetworkVariableFields").GetValue<List<NetworkVariableBase>>();
            return new JObject
            {
                ["class"] = b.GetType().FullName,
                ["behaviourId"] = b.NetworkBehaviourId,
                ["variables"] = new JArray(variables.Select(Variable)),
            };
        }

        static JObject Variable(NetworkVariableBase v)
        {
            JObject result = new JObject
            {
                ["name"] = v.Name,
                ["type"] = TypeName(v.GetType()),
                ["bytes"] = Bytes(v),
            };
            PropertyInfo value = v.GetType().GetProperty("Value");
            if (value != null)
            {
                result["value"] = value.GetValue(v)?.ToString();
            }
            else if (v is IEnumerable items)
            {
                result["items"] = new JArray(items.Cast<object>().Select(i => i?.ToString()));
            }
            return result;
        }

        static string Bytes(NetworkVariableBase v)
        {
            FastBufferWriter writer = new FastBufferWriter(256, Allocator.Temp, 1 << 20);
            try
            {
                WriteSync.Invoke(v, new object[] { writer });
                return System.BitConverter.ToString(writer.ToArray()).Replace("-", "").ToLowerInvariant();
            }
            finally
            {
                writer.Dispose();
            }
        }

        static string TypeName(System.Type t)
        {
            return t.IsGenericType ? $"{t.Name.Split('`')[0]}<{string.Join(",", t.GetGenericArguments().Select(TypeName))}>" : t.Name;
        }

        static string PathOf(Transform t)
        {
            return t.parent == null ? t.name : PathOf(t.parent) + "/" + t.name;
        }
    }
}
