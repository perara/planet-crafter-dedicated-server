# Monitoring

What to watch on a running server, and how to get it into the tools you already use: a health
route for container probes and uptime monitors, a metrics route for Prometheus (and anything that
reads its text format), a Grafana dashboard and alert rules to start from. `pcds doctor` is the other half: it checks the
setup, and this page watches the running server.

| You want | Use |
|---|---|
| "Is it up?" for Docker, Kubernetes or an uptime monitor | `GET /api/health` (no token) |
| Graphs and alerts | `GET /api/metrics` (a token), the dashboard in [grafana/pcds.json](grafana/pcds.json) |
| A check of the setup, once | `pcds doctor` |

## Health

`GET /api/health` needs no token and answers with the least it can:

| Answer | Means |
|---|---|
| `200 {"ok":true}` | The server serves a world: a game process is running it and the server answers. |
| `503 {"ok":false}` | The server is up but serves no world: it is stopped (`POST /api/world/stop`), still loading (a large world takes seconds; the first start reads the game tables first), or starting the game process again after a crash. Also when the server does not answer within 3 seconds. |

Why no token: a probe has to work before anyone has configured a secret into the orchestrator, the
answer says nothing a scanner could use (it does not even say which of the 503 cases it is: the
token-protected `GET /api/status` does), and the admin port is on `127.0.0.1` by default. If the
port is open beyond that, put it behind a proxy that lets only this route through
([reverse-proxy.md](reverse-proxy.md)).

A hot update does **not** turn it red: while the world is handed from one game process to the
other, the old one keeps serving until the new one has taken over, players stay connected, and a
probe that restarted the server on a 503 would kill the very world being updated.

```bash
curl -fsS http://127.0.0.1:7782/api/health        # exit 0 when healthy, 22 on a 503
pcds health                                        # the same, for scripts and the Docker image: exit 0 or 1
```

The Docker image's `HEALTHCHECK` runs `pcds health`. For Kubernetes see
[kubernetes.yaml](kubernetes.yaml): a startup probe for the first start, a liveness probe that
only restarts after the game process has been missing for several minutes, and a readiness probe.

## Metrics

`GET /api/metrics` answers in the Prometheus text format (version 0.0.4, `text/plain`). It needs a
token: the admin token, or better a token that can do nothing else:

```bash
pcds plugin token create prometheus --scope metrics.read     # shown once; stored as a hash
```

The scope `metrics.read` opens this one route and nothing else (not the status, not the players,
not a kick). Revoke it with `pcds plugin token revoke prometheus`.

### Scrape it

```yaml
# prometheus.yml
scrape_configs:
  - job_name: pcds
    scrape_interval: 15s
    metrics_path: /api/metrics
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/pcds.token      # the token, alone in the file, mode 0600
    static_configs:
      - targets: ["127.0.0.1:7782"]                     # the admin address; through a proxy: https and its host
        labels: { server: main }
```

Prometheus on another machine reaches the admin port only through a TLS proxy
([reverse-proxy.md](reverse-proxy.md)); do not publish the plain port for it. Scraping costs one
request to the game loop (a few milliseconds, at most one at a time per scrape); the default
15 seconds is fine.

### What it measures

Every name starts with `pcds` and an underscore. Times are in seconds, sizes in bytes, a ratio is 0 to 1. A
timestamp is Unix time; its age is `time() - metric` in the query. Counters end in `_total`.
Whatever depends on a running world (`players`, `tick`, `objects`, `plugins`, ...) is absent while
the world is stopped, and `pcds_world_running` is 0.

<!-- metrics -->
| Name | Type | Labels | What it is |
|---|---|---|---|
| `pcds_info` | gauge | `bank`, `bank_version`, `front_protocol`, `game_version`, `snapshot_format`, `version` | The server's version and formats, always 1. |
| `pcds_world_info` | gauge | `planet`, `world` | The world that runs and its planet, always 1 while a world runs. |
| `pcds_world_running` | gauge | none | 1 while the world runs, 0 while it is stopped, starting or recovering. |
| `pcds_paused` | gauge | none | 1 while the world is paused by the admin. |
| `pcds_uptime_seconds` | gauge | none | Seconds since the front started. |
| `pcds_players_online` | gauge | none | Players in the game. |
| `pcds_players_max` | gauge | none | The most players at once (the rule limits.maxPlayers). |
| `pcds_connections` | gauge | `transport` | Connections of players, joining ones included, by transport. |
| `pcds_tick_p50_seconds` | gauge | none | Median time of a game tick over the last minute. |
| `pcds_tick_p99_seconds` | gauge | none | 99th percentile time of a game tick over the last minute. |
| `pcds_tick_max_seconds` | gauge | none | Longest game tick over the last minute. |
| `pcds_tick_samples` | gauge | none | Ticks the tick times above are taken from (the last minute). |
| `pcds_loop_busy_ratio` | gauge | none | Share of the last second the game loop worked (0 to 1). |
| `pcds_loop_longest_seconds` | gauge | none | Longest pass of the game loop in the last second. |
| `pcds_loop_rate_hertz` | gauge | none | Passes of the game loop in the last second. |
| `pcds_memory_resident_bytes` | gauge | `process` | Resident memory of the server's processes: the front, the bank that runs the world, and a bank starting up for a hot update. |
| `pcds_cpu_seconds_total` | counter | none | CPU time of the front and the banks, ended ones included. |
| `pcds_world_objects` | gauge | none | World objects (buildings, items, plants) in the world. |
| `pcds_network_objects` | gauge | none | Objects the clients are told about. |
| `pcds_inventories` | gauge | none | Inventories in the world. |
| `pcds_network_receive_bytes_total` | counter | none | Bytes of game batches received from players. |
| `pcds_network_transmit_bytes_total` | counter | none | Bytes of game batches sent to players. |
| `pcds_save_last_timestamp_seconds` | gauge | none | When the last save ended, Unix time. Absent until the first save of this start. |
| `pcds_save_last_ok` | gauge | none | 1 if the last save was written, 0 if it failed. |
| `pcds_save_last_duration_seconds` | gauge | none | How long the last save took, from the text being made to the file being on disk. |
| `pcds_backup_enabled` | gauge | none | 1 if the world is backed up (the rule save.backupsKept is above 0). |
| `pcds_backup_last_success_timestamp_seconds` | gauge | none | When the last backup was made, Unix time. Absent until one is made after this start. |
| `pcds_backup_last_ok` | gauge | none | 1 if the last backup attempt worked, 0 if it failed. |
| `pcds_s3_enabled` | gauge | none | 1 if backups are copied to an S3 bucket (the rule backup.s3Bucket is set). |
| `pcds_s3_uploading` | gauge | none | 1 while a backup is being copied to the bucket. |
| `pcds_s3_last_upload_ok` | gauge | none | 1 if the last copy to the bucket worked, 0 if it failed. |
| `pcds_s3_last_upload_timestamp_seconds` | gauge | none | When the last copy to the bucket ended, Unix time. |
| `pcds_flips_total` | counter | none | Hot updates and rollbacks that handed the world to the other bank. |
| `pcds_flips_failed_total` | counter | none | Hand-overs that failed and left the old bank running. |
| `pcds_flip_last_stand_seconds` | gauge | none | How long the world stood in the last hand-over. Absent until one has happened. |
| `pcds_crashes_total` | counter | none | Times the game process ended outside a hand-over. |
| `pcds_crashes_recent` | gauge | none | Crashes in the last ten minutes; the front gives up at five. |
| `pcds_watchdog_kills_total` | counter | none | Times the watchdog killed a game process that said nothing (the rule limits.bankWatchdogSeconds). |
| `pcds_queue_waiting_bytes` | gauge | none | Bytes of player frames waiting for the game process now. |
| `pcds_queue_dropped_clients_total` | counter | none | Players dropped because too much of their traffic waited for the game process. |
| `pcds_queue_dropped_bytes_total` | counter | none | Bytes that were waiting for a game process when their player was dropped. |
| `pcds_update_available` | gauge | none | 1 if a newer release than the one running is out (as of the last check). |
| `pcds_plugins` | gauge | `state` | Plugins by state. |
| `pcds_plugin_calls_total` | counter | `plugin` | Events and commands handed to the plugin. |
| `pcds_plugin_faults_total` | counter | `plugin` | Calls that trapped, ran out of fuel or broke a limit. |
| `pcds_plugin_restarts_total` | counter | `plugin` | Times the plugin was started again after a fault. |
| `pcds_plugin_slow_total` | counter | `plugin` | Calls that took longer than the plugin's time budget. |
| `pcds_plugin_shed_total` | counter | `plugin` | Events left out because the plugin was too slow to take them. |
| `pcds_plugin_denied_total` | counter | `plugin` | Calls refused for a capability the plugin was not granted. |
| `pcds_plugin_refused_total` | counter | `plugin` | Calls refused for a limit (size, rate, a bad value). |
| `pcds_plugin_actions_total` | counter | `plugin` | Actions the plugin did in the world (chat, kicks, rule changes). |
| `pcds_plugin_vetoes_total` | counter | `plugin` | Events the plugin vetoed. |
| `pcds_plugin_fuel_total` | counter | `plugin` | WebAssembly fuel the plugin has spent. |
| `pcds_plugin_running` | gauge | `plugin`, `state` | 1 if the plugin runs, 0 in any other state. |
<!-- /metrics -->

### Alerts to start from

Save as `pcds-alerts.yml` and list it under `rule_files:`. `promtool check rules` accepts it. The
thresholds are starting points: the game ticks every 5 ms, saves run every 5 minutes (rule
`save.autosaveMinutes`) and backups every hour (`save.backupMinutes`).

```yaml
groups:
  - name: pcds
    rules:
      - alert: PcdsDown
        expr: up{job="pcds"} == 0
        for: 2m
        labels: { severity: critical }
        annotations: { summary: "The pcds server does not answer its metrics ({{ $labels.instance }})" }
      - alert: PcdsNoWorld
        expr: pcds_world_running == 0
        for: 5m
        labels: { severity: critical }
        annotations: { summary: "The server is up but serves no world: stopped, or the game process keeps failing (pcds logs)" }
      - alert: PcdsCrashLoop
        expr: increase(pcds_crashes_total[10m]) >= 2
        labels: { severity: critical }
        annotations: { summary: "The game process ended twice in 10 minutes; at 5 the front gives up" }
      - alert: PcdsWatchdogKill
        expr: increase(pcds_watchdog_kills_total[15m]) > 0
        labels: { severity: warning }
        annotations: { summary: "The watchdog killed a game process that said nothing (a hung world; pcds logs)" }
      - alert: PcdsSaveFailed
        expr: pcds_save_last_ok == 0
        for: 10m
        labels: { severity: critical }
        annotations: { summary: "The last save failed (disk full, permissions); progress since the one before is at risk" }
      - alert: PcdsSaveStale
        expr: pcds_world_running == 1 and time() - pcds_save_last_timestamp_seconds > 1800
        for: 5m
        labels: { severity: warning }
        annotations: { summary: "No save for over 30 minutes (autosave is off, or the world stands still)" }
      - alert: PcdsBackupFailed
        expr: pcds_backup_enabled == 1 and pcds_backup_last_ok == 0
        for: 30m
        labels: { severity: warning }
        annotations: { summary: "The last backup failed" }
      - alert: PcdsBackupStale
        expr: pcds_backup_enabled == 1 and time() - pcds_backup_last_success_timestamp_seconds > 6 * 3600
        for: 10m
        labels: { severity: warning }
        annotations: { summary: "No backup for 6 hours" }
      - alert: PcdsS3Failing
        expr: pcds_s3_enabled == 1 and pcds_s3_last_upload_ok == 0
        for: 1h
        labels: { severity: warning }
        annotations: { summary: "The copy of the backups to the bucket fails (keys, endpoint, network)" }
      - alert: PcdsSlowTicks
        expr: pcds_tick_p99_seconds > 0.01
        for: 10m
        labels: { severity: warning }
        annotations: { summary: "The p99 game tick is over 10 ms, two frames: players feel lag (a world of 33,000 objects idles near 4 ms)" }
      - alert: PcdsLoopBusy
        expr: pcds_loop_busy_ratio > 0.8
        for: 10m
        labels: { severity: warning }
        annotations: { summary: "The game loop works more than 80 percent of the time" }
      - alert: PcdsQueueDrops
        expr: increase(pcds_queue_dropped_clients_total[10m]) > 0
        labels: { severity: warning }
        annotations: { summary: "Players were dropped because their traffic piled up for a busy game process" }
      - alert: PcdsFlipFailed
        expr: increase(pcds_flips_failed_total[1h]) > 0
        labels: { severity: warning }
        annotations: { summary: "A hot update failed and the old game process went on (pcds status, GET /api/update)" }
      - alert: PcdsPluginFailed
        expr: pcds_plugins{state="failed"} > 0
        for: 5m
        labels: { severity: warning }
        annotations: { summary: "A plugin is switched off after a fault (pcds plugin list)" }
      - alert: PcdsPluginFaults
        expr: increase(pcds_plugin_faults_total[1h]) > 10
        labels: { severity: warning }
        annotations: { summary: "Plugin {{ $labels.plugin }} faulted more than 10 times in an hour" }
      - alert: PcdsMemoryHigh
        expr: pcds_memory_resident_bytes{process="bank"} > 2e9
        for: 15m
        labels: { severity: warning }
        annotations: { summary: "The game process holds over 2 GB; a hot update needs about as much again free (pcds doctor)" }
      - alert: PcdsUpdateAvailable
        expr: pcds_update_available == 1
        for: 3d
        labels: { severity: info }
        annotations: { summary: "A newer release has been out for 3 days (pcds upgrade)" }
```

A probe from outside (Blackbox exporter, an uptime service) on `GET /api/health` catches what
metrics cannot: a server so dead it cannot be scraped. Alert on the probe failing for several
minutes, not once: a crash restarts the game process within seconds.

## Grafana

Import [grafana/pcds.json](grafana/pcds.json) (Dashboards, New, Import; pick your Prometheus data
source). It shows the state at a glance (world running, players, last save and backup age, the
S3 copy), then the game loop (tick time p50, p99 and max, how busy it is), memory by process, CPU,
traffic, the size of the world, saves and backups, hand-overs and crashes, the waiting queue, and
the plugins. A `Server` selector at the top picks the machine when you scrape several.
