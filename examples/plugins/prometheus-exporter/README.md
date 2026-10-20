# prometheus-exporter

An out-of-process plugin: a Python 3 program (standard library only) that serves a server's numbers to
Prometheus on `http://127.0.0.1:9188/metrics`. It reads the plugin event stream (`POST /api/plugins/events`,
a long poll) for counts of what happens, and `GET /api/status` and `GET /api/plugins` for the state.
The server speaks no metrics format; see `docs/plugins.md`, "Out-of-process integrations".

```bash
./exporter.py --self-test                                  # no server needed
./exporter.py --replay bundle.json                         # renders a saved capture, no network (below)
PCDS_TOKEN=... ./exporter.py                               # --listen 127.0.0.1:9188 --api http://127.0.0.1:7782
PCDS_TOKEN=pcdst_... ./exporter.py --no-admin              # a scoped token: the stream's counters only
curl -s 127.0.0.1:9188/metrics
```

Run it as a service of its own (systemd with `EnvironmentFile=` mode 0600, Docker). `prometheus.yml` is a
scrape config, `pcds.rules.yml` has four alerts (down, plugin faults, a plugin off, a failing save). They are
valid YAML; there was no `promtool` here to check the PromQL.

## The token, honestly

| What | Request | Token it takes today |
|---|---|---|
| counts of chat, joins, leaves, objects, saves, rule changes | `POST /api/plugins/events` | scope `events.read` |
| players, world, tick times, saves, backups, process, traffic | `GET /api/status` | **the admin token only** |
| per-plugin counters and states | `GET /api/plugins` | **the admin token only** |

There is no scope for the last two (the scopes are `events.read`, `chat.read`, `chat.send`, `player.kick`,
`player.teleport`). That is a gap: to get the full set the exporter has to hold the admin token, which
can also stop the world, change rules and install plugins. If you run it that way, keep it on the server's
own machine, listening on loopback, with the token in a root-owned file. With a scoped token
(`pcds plugin token create metrics --scope events.read`) use `--no-admin`: it exports only the stream's
counters and the exporter's own metrics. A token passed with the wrong scope is not fatal: `/api/status`
answers 403, `pcds_exporter_errors_total{source="status"}` counts it and the rest is still served.
The token is read from the environment only, so it is not in `ps`.

## What it reads

The stream: `{"consumer": "prometheus", "after": N, "types": [chat, player_join, player_leave, object_placed,
object_destroyed, world_saved, rule_change], "limit": 500, "wait": 25}`. From the answer
`{"epoch", "events", "last", "dropped"}` it uses `epoch`, `last`, `dropped`, and for each event only `id` and
`type` (and `ok` of `world_saved`). Nothing else of an event is read: no name, no text, no `playerId`.

- The first poll (`limit: 1`) only finds the newest event number, so what happened before the exporter
  started is not counted. The consumer name appears in `GET /api/plugins` under `stream.consumers`; at most
  16 consumers fit (the 17th gets 429).
- A different `epoch` means the server restarted: the position goes back to 0, the new ring is counted from
  its first event, and `pcds_stream_resets_total` rises.
- The server records events for a consumer only for the minute after its last poll. If the exporter or the
  network is down longer, those events are not counted. The ring holds 512 events: more than that between
  two polls is lost and shows in `pcds_stream_events_dropped_total` (the ring's `dropped`, counted from the
  exporter's start; other consumers' misses are in it too).
- A server that does not know `wait` answers at once; the exporter then pauses 2 seconds between polls.

`GET /api/status`: `state`, `serverVersion`, `gameVersion`, `planet`, `held`, `uptimeSeconds`, `gameTime`,
`terraformation`, `players` (counted), `objects{world,network,inventories}`, `units{NAME:{value,perSecond}}`,
`tickRate`, `fps`, `busyPercent`, `longestLoopMicros`, `tick{samples,p50Micros,p99Micros,maxMicros}`,
`lastSave{ok,at}`, `backup{enabled,lastOkAt,error}`, `s3{enabled,uploading,last{ok,at}}`,
`process{cpuSeconds,memoryBytes}`, `traffic{bytesIn,bytesOut}`, `banks{flips,crashes}`,
`plugins{loaded,running}`. Fields that are missing or not numbers are left out, not guessed.

`GET /api/plugins`: `plugins[].{id, version, status, stats{calls, faults, restarts, slow, shed, denied, refused,
actions, vetoes, fuel, averageMicros, maxMicros, lastFaultAt, http{requests, answered, failed, bytesOut, bytesIn,
inFlight}}}`, `runtime{instances,memoryPages}`, `stream.consumers`. The two requests are made when Prometheus
scrapes, at most once a second however many scrape, 3 seconds each at most (keep `scrape_timeout` at 10 s).

## What it exports

Counters end in `_total`. Label values are few: `plugin` (at most 64), `status` (8), `unit` (about 10), `kind`
(3), `result` (2), `quantile` (3), `source` (3). No player is a label.

| Metric | Source | Meaning |
|---|---|---|
| `pcds_up` | | 1 if the server answered the last scrape's requests (any HTTP answer), else 0 |
| `pcds_exporter_source_up{source}` | | `stream`, `status`, `plugins`: 1 if the last request was answered with 2xx |
| `pcds_exporter_errors_total{source}` | | failed requests |
| `pcds_chat_messages_total`, `pcds_player_joins_total`, `pcds_player_leaves_total` | stream | counted since the exporter started |
| `pcds_objects_placed_total`, `pcds_objects_destroyed_total`, `pcds_rule_changes_total` | stream | |
| `pcds_world_saves_total{result="ok\|failed"}` | stream | |
| `pcds_stream_events_dropped_total`, `pcds_stream_resets_total` | stream | lost events; server restarts seen |
| `pcds_build_info{server_version,game_version,planet,state}` | status | always 1 |
| `pcds_world_running`, `pcds_world_held`, `pcds_uptime_seconds` | status | |
| `pcds_players_online`, `pcds_game_time_seconds`, `pcds_terraformation` | status | |
| `pcds_world_objects{kind}`, `pcds_world_unit_value{unit}`, `pcds_world_unit_per_second{unit}` | status | |
| `pcds_tick_rate`, `pcds_loop_rate`, `pcds_loop_busy_ratio`, `pcds_loop_longest_seconds` | status | |
| `pcds_tick_duration_seconds{quantile="0.5\|0.99\|1"}`, `pcds_tick_samples` | status | the last minute |
| `pcds_last_save_success`, `pcds_last_save_timestamp_seconds` | status | |
| `pcds_backup_enabled`, `pcds_backup_failing`, `pcds_backup_last_success_timestamp_seconds` | status | |
| `pcds_s3_enabled`, `pcds_s3_uploading`, `pcds_s3_last_upload_success`, `pcds_s3_last_upload_timestamp_seconds` | status | |
| `pcds_process_cpu_seconds_total`, `pcds_process_memory_bytes` | status | the server's processes |
| `pcds_network_received_bytes_total`, `pcds_network_sent_bytes_total` | status | |
| `pcds_bank_flips_total`, `pcds_bank_crashes_recent` | status | hot updates; crashes in the front's window |
| `pcds_plugins_loaded`, `pcds_plugins_running` | status | |
| `pcds_stream_consumers`, `pcds_plugin_instances`, `pcds_plugin_memory_pages` | plugins | |
| `pcds_plugin_info{plugin,version}` | plugins | always 1 |
| `pcds_plugin_status{plugin,status}` | plugins | 1 for the plugin's status, 0 for the seven others (`running pending disabled changed unloading invalid incompatible failed`) |
| `pcds_plugin_{calls,faults,restarts,slow_calls,shed,denied,refused,actions,vetoes,fuel}_total{plugin}` | plugins | since the plugin started (a reload resets them: use `increase`) |
| `pcds_plugin_{http_requests,http_answered,http_failed,http_sent_bytes,http_received_bytes}_total{plugin}` | plugins | |
| `pcds_plugin_http_in_flight`, `pcds_plugin_call_average_seconds`, `pcds_plugin_call_max_seconds`, `pcds_plugin_last_fault_timestamp_seconds` | plugins | |

When the server is down or restarting the exporter stays up: `pcds_up 0`, the gauges of the server are left out
(a missing sample, not a stale one), the counters stay where they were, and the stream loop retries with a wait
of 4, 8, 16 ... 60 seconds. Errors are printed by kind and HTTP status only, never a token or a URL.

## `--replay`

`./exporter.py --replay FILE` prints the metrics for a saved bundle and exits, with no network:

```json
{"events": [ {"id": 1, "type": "chat", ...}, ... ], "status": { /api/status answer }, "plugins": { /api/plugins answer }}
```

`events` are the events of the stream's answers (every event counts once, `id` is not checked); `status` and
`plugins` may be left out. `pcds_up` is 1. Anything of an unexpected shape is skipped.

## Without a process of its own: a plugin that pushes

Not built. A WebAssembly plugin can push the numbers itself with `net.http`, with `event.world` for
`world_saved` (every autosave) and `event.tick` for a timer. Sketch: on `world_saved`, `Host::world()`
(`world.read`: players, game time, objects, terraformation) and the counters it kept from `chat`, `player_join` and
`player_leave`; build Influx line protocol (`pcds players=3i,terraformation=1234.5 <unix ms>`) or a
Pushgateway body (`pcds_players_online 3\n`), `POST` it to `http://pushgateway:9091/metrics/job/pcds` or
`/api/v2/write?bucket=...` with a token header as `${config.token}`. It needs `[net] allow` for that host and
`net.http.local` if the Pushgateway is on the LAN (an address in a pattern is refused otherwise: use a name). What it
cannot do: the plugin's own `stats` (faults, fuel) and the tick times are not available to a plugin, which is
what `/api/plugins` and `/api/status` are for; so this variant sees the game but not the server's health.

## What is not built

- No `/api/status` or `/api/plugins` with a scoped token (the server has no scope for them).
- No per-player or per-object-group metrics (cardinality) and no histograms of tick times (the status holds
  only three quantiles of the last minute).
- No TLS or authentication on `/metrics`: bind it to loopback, or put a proxy in front.
- No persistence: the counters start at 0 when the exporter does.

## Licence

MIT (see ../LICENSE).
