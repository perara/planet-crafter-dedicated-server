using System.Linq;
using HarmonyLib;
using SpaceCraft;
using Unity.Netcode;
using UnityEngine;

namespace PcdsExtractor
{
    // The host has to own a player object, because the game runs its simulation through the
    // local player. This keeps that avatar out of everything other players can see.
    [HarmonyPatch]
    internal static class GhostHost
    {
        static NetworkObject _host;

        internal static void Hide(PlayerMainController host)
        {
            _host = host.NetworkObject;
            // Checked by Netcode when it spawns objects for a newly connected client.
            _host.CheckObjectVisibility = clientId => clientId == NetworkManager.ServerClientId;
            foreach (ulong clientId in NetworkManager.Singleton.ConnectedClientsIds.Where(id => id != NetworkManager.ServerClientId && _host.IsNetworkVisibleTo(id)).ToList())
            {
                _host.NetworkHide(clientId);
            }
        }

        // The player list (names, map markers, chat, player-count scaling) is a NetworkList the
        // server fills for every player it starts, the host included.
        internal static void Tick()
        {
            if (PlayersDataManager.Instance == null)
            {
                return;
            }
            NetworkList<PlayerData> list = Traverse.Create(PlayersDataManager.Instance).Field("_playersData").GetValue<NetworkList<PlayerData>>();
            if (list == null)
            {
                return;
            }
            for (int i = list.Count - 1; i >= 0; i--)
            {
                if (list[i].id == NetworkManager.ServerClientId)
                {
                    list.RemoveAt(i);
                }
            }
        }

        // PlayersManager.Update shows players to each other again when they come close.
        [HarmonyPrefix]
        [HarmonyPatch(typeof(NetworkObject), nameof(NetworkObject.NetworkShow), typeof(ulong))]
        static bool KeepHidden(NetworkObject __instance)
        {
            return _host == null || __instance != _host;
        }

        // Meteor events otherwise pick a random player, the hidden host included.
        [HarmonyPrefix]
        [HarmonyPatch(typeof(MeteoHandler), nameof(MeteoHandler.LaunchSpecificMeteoEvent), typeof(int), typeof(Vector3?))]
        static void AimAtRealPlayer(ref Vector3? refPosition)
        {
            if (refPosition.HasValue || _host == null)
            {
                return;
            }
            PlayerMainController[] players = Managers.GetManager<PlayersManager>().playersControllers
                .Where(p => p != null && p.NetworkObject != _host)
                .ToArray();
            if (players.Length > 0)
            {
                refPosition = players[Random.Range(0, players.Length)].transform.position;
            }
        }
    }
}
