using System;
using System.Net;
using System.Text;
using System.Threading;
using Newtonsoft.Json;

namespace PcdsExtractor
{
    // The extraction API on 127.0.0.1, served from a background thread. Every call that
    // touches the game runs on the main thread through Extractor.OnMain.
    //   GET  /api/status     state, game version, current planet, world units
    //   GET  /api/netdata    network tables (spawned objects, message versions, RPCs)
    //   GET  /api/gamedata   game data (assets, prefabs, singletons, layouts of every group)
    //   GET  /api/scenedata  the loaded planet's scene (scene world objects, water, ore veins)
    //   GET  /api/manifest   what is spawned, with every network variable (for debugging)
    //   POST /api/stop       quit the game
    internal static class Api
    {
        static HttpListener _listener;
        static string _token;

        internal static void Start(string address, int port, string token)
        {
            _token = token;
            _listener = new HttpListener();
            _listener.Prefixes.Add($"http://{address}:{port}/");
            _listener.Start();
            new Thread(Loop) { IsBackground = true, Name = "pcds-extractor-api" }.Start();
            Extractor.Log.LogInfo($"PCDS api on http://{address}:{port}/");
        }

        static void Loop()
        {
            while (_listener.IsListening)
            {
                HttpListenerContext context = _listener.GetContext();
                ThreadPool.QueueUserWorkItem(_ => Handle(context));
            }
        }

        static void Handle(HttpListenerContext context)
        {
            string path = context.Request.Url.AbsolutePath;
            try
            {
                if (context.Request.Headers["Authorization"] != "Bearer " + _token)
                {
                    SendJson(context, 401, new { error = "missing or wrong token" });
                    return;
                }
                switch (context.Request.HttpMethod + " " + path)
                {
                    case "GET /api/status":
                        Send(context, 200, Extractor.StatusJson);
                        return;
                    case "GET /api/manifest":
                        Send(context, 200, Extractor.OnMain(() => Manifest.Build().ToString()));
                        return;
                    case "GET /api/gamedata":
                        Send(context, 200, GameData.Build().ToString());
                        return;
                    case "GET /api/netdata":
                        Send(context, 200, Extractor.OnMain(() => NetData.Build().ToString(), 60000));
                        return;
                    case "GET /api/scenedata":
                        Send(context, 200, GameData.Build(layouts: false).ToString());
                        return;
                    case "POST /api/stop":
                        Extractor.Instance.RequestStop();
                        SendJson(context, 200, new { ok = true });
                        return;
                    default:
                        SendJson(context, 404, new { error = "unknown endpoint" });
                        return;
                }
            }
            catch (Exception e)
            {
                Extractor.Log.LogError($"PCDS api {path}: {e}");
                SendJson(context, 500, new { error = e.Message });
            }
        }

        static void SendJson(HttpListenerContext context, int status, object value)
        {
            Send(context, status, JsonConvert.SerializeObject(value));
        }

        static void Send(HttpListenerContext context, int status, string json)
        {
            byte[] bytes = Encoding.UTF8.GetBytes(json);
            context.Response.StatusCode = status;
            context.Response.ContentType = "application/json";
            context.Response.ContentLength64 = bytes.Length;
            context.Response.OutputStream.Write(bytes, 0, bytes.Length);
            context.Response.OutputStream.Close();
        }
    }
}
