using System;
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
    // The network tables the standalone server needs, read from the running game: every
    // spawned object as Netcode serializes it, the message version table, the connection
    // request's config hash and data, and every RPC with its argument encoding.
    // GET /api/netdata; `pcds-server extract` turns it into data/<version>/.
    internal static class NetData
    {
        static readonly MethodInfo GetSceneObject = AccessTools.Method(typeof(NetworkObject), "GetMessageSceneObject");

        internal static JObject Build()
        {
            NetworkManager manager = NetworkManager.Singleton;
            JObject manifest = Manifest.Build();
            object messages = Traverse.Create(manager).Field("MessageManager").GetValue();
            Type[] types = Traverse.Create(messages).Property("MessageTypes").GetValue<Type[]>();
            MethodInfo localVersion = AccessTools.Method(messages.GetType(), "GetLocalVersion");
            MethodInfo hash32 = AccessTools.Method(AccessTools.TypeByName("Unity.Netcode.XXHash"), "Hash32", new[] { typeof(string) });
            // As NetworkConnectionManager sends them: one per handler slot, empty slots as (0, 0).
            int slots = ((Array)Traverse.Create(messages).Property("MessageHandlers").GetValue()).Length;
            JArray versions = new JArray();
            for (int i = 0; i < slots; i++)
            {
                Type t = types[i];
                versions.Add(t == null ? new JArray(0u, 0) : new JArray((uint)hash32.Invoke(null, new object[] { t.FullName }), (int)localVersion.Invoke(messages, new object[] { t })));
            }
            return new JObject
            {
                ["gameVersion"] = Application.version,
                ["tickRate"] = manager.NetworkConfig.TickRate,
                ["playerPrefab"] = ((uint)Traverse.Create(manager.NetworkConfig.PlayerPrefab.GetComponent<NetworkObject>()).Field("GlobalObjectIdHash").GetValue()).ToString("x8"),
                ["configHash"] = manager.NetworkConfig.GetConfig().ToString(),
                ["connectionData"] = Application.version + "|0",
                ["messageVersions"] = versions,
                ["objects"] = new JArray(manager.SpawnManager.SpawnedObjectsList.OrderBy(o => o.NetworkObjectId).Select(Object)),
                ["planets"] = manifest["planets"],
                ["groups"] = manifest["groups"],
                ["rpcs"] = Rpcs(),
            };
        }

        static JObject Object(NetworkObject o)
        {
            JObject entry = Manifest.Object(o);
            object sceneObject = GetSceneObject.Invoke(o, new object[] { 0uL, false });
            FastBufferWriter writer = new FastBufferWriter(1024, Allocator.Temp, 1 << 22);
            try
            {
                sceneObject.GetType().GetMethod("Serialize").Invoke(sceneObject, new object[] { writer });
                entry["sceneObject"] = BitConverter.ToString(writer.ToArray()).Replace("-", "").ToLowerInvariant();
            }
            finally
            {
                writer.Dispose();
            }
            return entry;
        }

        // Every RPC of the game's NetworkBehaviours. Netcode's code generator gives each RPC a
        // static __rpc_handler_<hash> next to it; the RPC method's IL loads that hash (ldc.i4).
        // Static, so RPCs of types the loaded world has no instance of are included too.
        static JObject Rpcs()
        {
            JObject rpcs = new JObject();
            Assembly game = typeof(WorldObjectsHandler).Assembly;
            foreach (Type type in game.GetTypes().Where(t => typeof(NetworkBehaviour).IsAssignableFrom(t)))
            {
                HashSet<uint> hashes = new HashSet<uint>();
                foreach (MethodInfo h in type.GetMethods(BindingFlags.Static | BindingFlags.NonPublic | BindingFlags.Public | BindingFlags.DeclaredOnly))
                {
                    if (h.Name.StartsWith("__rpc_handler_") && uint.TryParse(h.Name.Substring("__rpc_handler_".Length), out uint value))
                    {
                        hashes.Add(value);
                    }
                }
                foreach (MethodInfo m in type.GetMethods(BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.DeclaredOnly))
                {
                    if (!m.Name.EndsWith("Rpc") || m.Name.StartsWith("__"))
                    {
                        continue;
                    }
                    uint? hash = HashIn(m, hashes);
                    if (hash == null || rpcs.ContainsKey(hash.ToString()))
                    {
                        continue;
                    }
                    rpcs[hash.ToString()] = new JObject
                    {
                        ["name"] = type.Name + "." + m.Name,
                        ["to"] = Target(m),
                        ["args"] = new JArray(m.GetParameters().Where(p => !p.ParameterType.Name.EndsWith("RpcParams")).Select(Arg)),
                    };
                }
            }
            return rpcs;
        }

        // The first ldc.i4 (0x20) operand in the method's IL that is a registered hash.
        static uint? HashIn(MethodInfo m, HashSet<uint> hashes)
        {
            byte[] il = m.GetMethodBody()?.GetILAsByteArray();
            if (il == null)
            {
                return null;
            }
            for (int i = 0; i + 4 < il.Length; i++)
            {
                if (il[i] == 0x20)
                {
                    uint v = BitConverter.ToUInt32(il, i + 1);
                    if (hashes.Contains(v))
                    {
                        return v;
                    }
                }
            }
            return null;
        }

        static string Target(MethodInfo m)
        {
            foreach (Attribute a in m.GetCustomAttributes(true).OfType<Attribute>())
            {
                switch (a.GetType().Name)
                {
                    case "ServerRpcAttribute":
                        return "server";
                    case "ClientRpcAttribute":
                        return "client";
                    case "RpcAttribute":
                        object target = a.GetType().GetField("SendTo")?.GetValue(a) ?? a.GetType().GetProperty("SendTo")?.GetValue(a);
                        return target?.ToString().ToLowerInvariant() ?? "server";
                }
            }
            return "?";
        }

        static readonly HashSet<Type> Packed = new HashSet<Type> { typeof(int), typeof(uint), typeof(short), typeof(ushort), typeof(long), typeof(ulong) };

        // How Netcode's code generator writes a parameter of this type.
        static JObject Arg(ParameterInfo p)
        {
            Type t = p.ParameterType;
            JObject arg = new JObject { ["name"] = p.Name, ["type"] = TypeName(t), ["optional"] = !t.IsValueType };
            if (t.IsArray)
            {
                arg["enc"] = "array";
                arg["item"] = Encoding(t.GetElementType(), inArray: true);
            }
            else
            {
                arg["enc"] = Encoding(t, inArray: false);
            }
            if (Encoding(t.IsArray ? t.GetElementType() : t, inArray: false) == "struct")
            {
                arg["fields"] = new JArray(StructFields(t.IsArray ? t.GetElementType() : t));
            }
            return arg;
        }

        static string Encoding(Type t, bool inArray)
        {
            if (t == typeof(string))
            {
                return "string";
            }
            if (t.IsEnum)
            {
                return "enum";
            }
            if (!inArray && Packed.Contains(t))
            {
                return "packed";
            }
            if (t.IsPrimitive || t.Namespace == "UnityEngine")
            {
                return "raw";
            }
            if (t.GetInterfaces().Any(i => i.Name == "INetworkSerializeByMemcpy"))
            {
                return "struct";
            }
            return "serializable";
        }

        // Field types of a memcpy struct in layout order, with padding the way the Rust codec names it.
        static IEnumerable<string> StructFields(Type t)
        {
            int offset = 0;
            foreach (FieldInfo f in t.GetFields(BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic).OrderBy(f => System.Runtime.InteropServices.Marshal.OffsetOf(t, f.Name).ToInt32()))
            {
                int at = System.Runtime.InteropServices.Marshal.OffsetOf(t, f.Name).ToInt32();
                for (; offset < at; offset++)
                {
                    yield return "pad1";
                }
                string name = f.FieldType.IsEnum ? "enum" : TypeName(f.FieldType);
                offset = at + (f.FieldType == typeof(bool) ? 1 : System.Runtime.InteropServices.Marshal.SizeOf(f.FieldType.IsEnum ? Enum.GetUnderlyingType(f.FieldType) : f.FieldType));
                yield return name;
            }
        }

        static string TypeName(Type t)
        {
            if (t.IsArray)
            {
                return TypeName(t.GetElementType()) + "[]";
            }
            string alias = t == typeof(int) ? "int" : t == typeof(uint) ? "uint" : t == typeof(short) ? "short" : t == typeof(ushort) ? "ushort"
                : t == typeof(long) ? "long" : t == typeof(ulong) ? "ulong" : t == typeof(bool) ? "bool" : t == typeof(float) ? "float"
                : t == typeof(double) ? "double" : t == typeof(byte) ? "byte" : t == typeof(string) ? "string" : null;
            if (alias != null)
            {
                return alias;
            }
            return t.IsNested ? TypeName(t.DeclaringType) + "." + t.Name : t.Name;
        }
    }
}
