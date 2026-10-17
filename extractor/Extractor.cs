using System;
using System.Collections;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading;
using BepInEx;
using BepInEx.Configuration;
using BepInEx.Logging;
using HarmonyLib;
using Newtonsoft.Json;
using SpaceCraft;
using Unity.Netcode;
using Unity.Netcode.Transports.UTP;
using UnityEngine;
using UnityEngine.SceneManagement;

namespace PcdsExtractor
{
    // Runs the game headless as a host on 127.0.0.1, so `pcds extract` can read the tables the
    // standalone server needs (GET /api/netdata, /api/gamedata, /api/scenedata) from the game
    // itself. It boots straight into a world of its own (created as a new Standard world when
    // missing), keeps the host avatar hidden and idle, and quits on POST /api/stop.
    [BepInPlugin("pcds.extractor", "Planet Crafter dedicated server table extractor", "1.0.0")]
    public class Extractor : BaseUnityPlugin
    {
        internal static Extractor Instance;
        internal static ManualLogSource Log;

        ConfigEntry<string> _saveName;
        ConfigEntry<int> _port;
        ConfigEntry<int> _apiPort;
        ConfigEntry<string> _apiToken;

        // Read by the API thread; rebuilt once per second on the main thread.
        internal static volatile string StatusJson = "{\"state\":\"starting\"}";

        static readonly ConcurrentQueue<Action> MainQueue = new ConcurrentQueue<Action>();
        volatile bool _stop;

        void Awake()
        {
            Instance = this;
            Log = Logger;
            _saveName = Config.Bind("Extractor", "SaveName", "Extract-1", "World the extractor hosts (save file name without .json); created as a new Standard world when missing");
            _port = Config.Bind("Extractor", "Port", 7791, "UDP port on 127.0.0.1 a scripted player joins during the extraction");
            _apiPort = Config.Bind("Extractor", "ApiPort", 7781, "HTTP port on 127.0.0.1 of the extraction API");
            _apiToken = Config.Bind("Extractor", "ApiToken", "", "Bearer token for the extraction API; generated when empty");
            if (string.IsNullOrEmpty(_apiToken.Value))
            {
                _apiToken.Value = Guid.NewGuid().ToString("N");
            }
            Harmony.CreateAndPatchAll(typeof(Patches), "pcds.extractor");
            Harmony.CreateAndPatchAll(typeof(GhostHost), "pcds.extractor.host");
            Api.Start("127.0.0.1", _apiPort.Value, _apiToken.Value);
            SceneManager.sceneLoaded += OnSceneLoaded;
        }

        void Update()
        {
            while (MainQueue.TryDequeue(out Action action))
            {
                action();
            }
        }

        // Runs f on the Unity main thread and waits for its result (API requests).
        internal static T OnMain<T>(Func<T> f, int timeoutMs = 10000)
        {
            T result = default;
            Exception error = null;
            using (ManualResetEventSlim done = new ManualResetEventSlim())
            {
                MainQueue.Enqueue(() =>
                {
                    try
                    {
                        result = f();
                    }
                    catch (Exception e)
                    {
                        error = e;
                    }
                    done.Set();
                });
                if (!done.Wait(timeoutMs))
                {
                    throw new TimeoutException("main thread did not answer");
                }
            }
            if (error != null)
            {
                // Keep the main-thread stack trace in the log.
                System.Runtime.ExceptionServices.ExceptionDispatchInfo.Capture(error).Throw();
            }
            return result;
        }

        // Runs a coroutine on the main thread and waits for the result it hands to its callback.
        internal static T OnMainCoroutine<T>(Func<Action<T>, IEnumerator> start, int timeoutMs)
        {
            T result = default;
            using (ManualResetEventSlim done = new ManualResetEventSlim())
            {
                MainQueue.Enqueue(() => Instance.StartCoroutine(start(r =>
                {
                    result = r;
                    done.Set();
                })));
                if (!done.Wait(timeoutMs))
                {
                    throw new TimeoutException("main thread coroutine did not finish");
                }
            }
            return result;
        }

        internal void RequestStop()
        {
            _stop = true;
        }

        void OnSceneLoaded(Scene scene, LoadSceneMode loadMode)
        {
            if (scene.name == GameConfig.introSceneName)
            {
                StartCoroutine(Boot());
            }
            else if (scene.name == GameConfig.mainSceneName)
            {
                StartCoroutine(WhenHostReady());
            }
        }

        static SaveFilesSelector FindSelector()
        {
            return UnityEngine.Object.FindObjectsByType<SaveFilesSelector>(FindObjectsInactive.Include, FindObjectsSortMode.None).FirstOrDefault();
        }

        // From the main menu: create the world if needed, then host it over direct IP.
        IEnumerator Boot()
        {
            while (FindSelector() == null || Managers.GetManager<SavedDataHandler>() == null)
            {
                yield return null;
            }
            // Let Intro.Start finish building the menu before driving it.
            yield return new WaitForSecondsRealtime(2f);
            string dataDir = Application.persistentDataPath;
            // The game reads Server.conf when hosting over UnityTransport (UnityTransportNetworkBackend.StartHostAsync).
            File.WriteAllText(Path.Combine(dataDir, "Server.conf"), JsonUtility.ToJson(new ServerConfiguration { hostPort = (ushort)_port.Value, hostPlayerName = "Extractor" }));
            string save = _saveName.Value;
            if (!File.Exists(Path.Combine(dataDir, save + ".json")))
            {
                Logger.LogInfo($"PCDS creating new world {save}");
                SaveFilesCreateNew creator = UnityEngine.Object.FindObjectsByType<SaveFilesCreateNew>(FindObjectsInactive.Include, FindObjectsSortMode.None).First();
                creator.Init();
                creator.newNameInput.text = save;
                Traverse.Create(creator).Field("_fileName").SetValue(save);
                creator.CreateNewFile();
            }
            SavedDataHandler savedData = Managers.GetManager<SavedDataHandler>();
            savedData.onlineGame = true;
            savedData.localPlayerName = "Extractor";
            Logger.LogInfo($"PCDS hosting {save} on 127.0.0.1:{_port.Value}");
            FindSelector().SelectedSaveFile(save);
        }

        IEnumerator WhenHostReady()
        {
            while (Managers.GetManager<PlayersManager>() == null)
            {
                yield return null;
            }
            Managers.GetManager<PlayersManager>().RegisterToLocalPlayerStarted(delegate
            {
                PlayerMainController host = Managers.GetManager<PlayersManager>().GetActivePlayerController();
                // The host avatar never moves; paused gauges keep it from dying.
                host.GetPlayerGaugesHandler().SetPauseConsumption(pause: true);
                GhostHost.Hide(host);
                Logger.LogInfo($"PCDS READY planet={Managers.GetManager<PlanetLoader>().GetCurrentPlanetData()?.GetPlanetId()}");
                StartCoroutine(Loop());
            });
        }

        IEnumerator Loop()
        {
            WaitForSecondsRealtime second = new WaitForSecondsRealtime(1f);
            while (!_stop)
            {
                QualitySettings.vSyncCount = 0;
                Application.targetFrameRate = 30;
                GhostHost.Tick();
                StatusJson = JsonConvert.SerializeObject(new
                {
                    state = "running",
                    gameVersion = Application.version,
                    planet = Managers.GetManager<PlanetLoader>()?.GetCurrentPlanetData()?.id,
                    units = Units(),
                });
                yield return second;
            }
            Logger.LogInfo("PCDS STOPPED");
            // The disk log is buffered and Wine exits before BepInEx flushes it.
            foreach (DiskLogListener disk in BepInEx.Logging.Logger.Listeners.OfType<DiskLogListener>())
            {
                disk.LogWriter.Flush();
            }
            Application.Quit();
        }

        // The current planet's world units: value and change per second, to compare with the
        // standalone server's on the same world.
        static Dictionary<string, object> Units()
        {
            WorldUnitsHandler handler = Managers.GetManager<WorldUnitsHandler>();
            if (handler == null || !handler.AreUnitsInited())
            {
                return null;
            }
            Dictionary<string, object> units = new Dictionary<string, object>();
            foreach (WorldUnit u in handler.GetAllPlanetUnits())
            {
                units[u.GetUnitType().ToString()] = new { value = u.GetValue(), perSecond = u.GetCurrentValuePersSec() };
            }
            return units;
        }

        [HarmonyPatch]
        static class Patches
        {
            // The game hosts through Steam networking whenever Steam is initialized; always use
            // Unity Transport (direct IP).
            [HarmonyPrefix]
            [HarmonyPatch(typeof(NetworkBackendProvider), nameof(NetworkBackendProvider.GetPlatformBackend))]
            static bool DirectIpOnly(ref INetworkBackend __result)
            {
                System.Reflection.FieldInfo field = AccessTools.Field(typeof(NetworkBackendProvider), "_platformBackend");
                if (!(field.GetValue(null) is UnityTransportNetworkBackend))
                {
                    field.SetValue(null, new UnityTransportNetworkBackend());
                }
                __result = (INetworkBackend)field.GetValue(null);
                return false;
            }

            // Listen on loopback only: the extraction is local.
            [HarmonyPostfix]
            [HarmonyPatch(typeof(UnityTransportNetworkBackend), nameof(UnityTransportNetworkBackend.ConfigureNetworkManager))]
            static void ListenOnLoopback(NetworkManager networkManager)
            {
                UnityTransport transport = networkManager.GetComponent<UnityTransport>();
                if (transport != null)
                {
                    transport.ConnectionData.ServerListenAddress = "127.0.0.1";
                }
            }

            // The game approves "version|session" against its random session number; the
            // scripted player joins like the game's IP menu, with session 0.
            [HarmonyPrefix]
            [HarmonyPatch(typeof(SessionController), "ApprovalCheck")]
            static bool Approve(NetworkManager.ConnectionApprovalRequest request, NetworkManager.ConnectionApprovalResponse response)
            {
                string[] parts = System.Text.Encoding.ASCII.GetString(request.Payload).Split('|');
                int expected = Managers.GetManager<SavedDataHandler>().sessionId;
                response.Approved = parts.Length == 2 && parts[0] == Application.version && (parts[1] == "0" || parts[1] == expected.ToString());
                response.Reason = response.Approved ? null : "MENU_Network_Error_version_mismatch";
                response.CreatePlayerObject = response.Approved;
                response.Pending = false;
                return false;
            }

            // Never unlock achievements on the account the game runs under.
            [HarmonyPrefix]
            [HarmonyPatch(typeof(AchievementsHandler), nameof(AchievementsHandler.UnlockAchievemement))]
            static bool NoAchievements()
            {
                return false;
            }

            // The intro video sets Time.timeScale = 0 until it finishes playing; with the Null
            // graphics device it may never become prepared, which would freeze the world.
            [HarmonyPrefix]
            [HarmonyPatch(typeof(SavedDataHandler), nameof(SavedDataHandler.GetSavedDataPlayedIntro), new Type[0])]
            static bool SkipIntroVideo(ref bool __result)
            {
                __result = true;
                return false;
            }
        }
    }
}
