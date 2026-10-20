# Disaster recovery

What to keep, how to know your backups work, and how to get a world back: on the same machine, on a
new one, from the S3 bucket, from a Docker volume. Every command below was run for real on a
scratch installation (see "What was rehearsed" at the end).

## What the server keeps

Everything is in one folder: `run/` of the release or checkout, or `PCDS_HOME`, or `/data` in Docker.

| Path in `run/` | What it holds | Keep a copy? |
|---|---|---|
| `worlds/<name>.json` | the worlds. The one the server runs is `server.env`'s `PCDS_WORLD` | **Yes.** This is the game |
| `worlds/backups/` | timestamped copies of the world (`<world>-YYYYMMDD-HHMMSS.json`; the rules `save.backupsKept` and `save.backupMinutes`) | Off this machine: the bucket (below), or a copy of the folder. A backup on the same disk does not survive the disk |
| `server.env` | the ports, the Steam login token (it keeps the invite code), the admin token | **Yes.** Mode 0600: it is a secret |
| `rules.json` | the rules, with the chat secret, the webhook URLs and the S3 keys | **Yes.** Mode 0600 |
| `access.json` | bans and the lists of who may join | **Yes** |
| `plugins/` | the plugins, what you approved (`approved.json`), their settings and saved state, the scoped tokens | **Yes** if you use any |
| `data/<version>/` | the game's tables, read from your game | No: `pcds update` reads them again |
| `bin/` | the program and its two banks | No: `pcds setup` and `pcds upgrade` install it |
| `game/`, `steamcmd/`, `steam-user` | the game `--steam-user` downloaded | No: `pcds setup --steam-user NAME` downloads it again |
| `game-dir` | which game folder was used last | No |

The world is written whole or not at all (a file next to it, flushed, then moved over it), and so is
each backup, so a copy of `worlds/` made while the server runs holds old files or new ones, never
half of one. For a copy of everything at one moment, run `pcds save` first.

What a failure costs: the world on disk is at most `save.autosaveMinutes` old (5 by default), and
the newest backup at most `save.backupMinutes` (60). A stop (`pcds stop`, `docker stop`) saves
first.

## Keeping copies

1. **Off-site backups.** The rules `backup.s3Endpoint`, `backup.s3Bucket`, `backup.s3AccessKey` and
   `backup.s3SecretKey` copy every new backup to an S3-compatible bucket (AWS S3, Cloudflare R2,
   Backblaze B2, MinIO, ...) as `<backup.s3Prefix><world>-<time>.json`; see the README's
   [Off-site copies](../README.md#off-site-copies-s3). Use a key that may write and read this
   bucket and nothing else. Set a lifecycle rule on the bucket to expire old copies; the server
   never deletes from it.
2. **The settings.** `server.env`, `rules.json`, `access.json` and `plugins/` change rarely and are
   small. Copy them when you change them, to somewhere that is not this machine, and keep them as
   private as the files are (they hold the S3 keys, the Steam token and the admin token).
   ```bash
   tar czf pcds-settings-$(date +%F).tar.gz -C run server.env rules.json access.json plugins
   ```
3. **The whole folder** (optional, for a quick return): stop the server or run `pcds save`, then
   ```bash
   tar czf pcds-run-$(date +%F).tar.gz -C run worlds server.env rules.json access.json plugins
   ```
4. **Docker.** The volume (or the folder mounted at `/data`) is all of it:
   ```bash
   docker exec pcds pcds save
   docker run --rm -v pcds-data:/data:ro -v "$PWD":/backup alpine \
     tar czf /backup/pcds-data-$(date +%F).tar.gz -C /data worlds server.env rules.json access.json plugins
   ```
   With a folder mounted (`./data:/data`) it is the same `tar` on the host.

## Knowing a backup works

A backup that does not load is found out when it is needed. Look before:

```bash
pcds backup list                      # this world's backups, newest first, with age and size
pcds backup verify                    # the newest 3, loaded the way the server loads a world
pcds backup verify --all              # every one
```

`verify` reads each file with the loader the server uses (the layout, the content against the
game's tables, every object and inventory built), says what it holds (planet, world objects, network
objects, inventories, saved players), and says if the server would refuse to start it (a DLC planet
you have not stated that you own, or no tables for it). The exit code is 1 when one does not load,
so it can run from `cron` or a timer:

```bash
# /etc/cron.d/pcds, weekly: mail the output when a backup does not load
0 4 * * 0  per  /home/per/pcds/pcds backup verify --count 5 || echo "pcds backups do not load" | mail -s pcds you@example.com
```

With an S3 bucket set, `verify` also asks the bucket whether the newest backup is there and has the
same size as the file here.

To check every backup as it is made, set the rule `save.verifyBackups` (off by default):

```bash
pcds rules set save.verifyBackups=true
```

After each backup (the hourly one, and `POST /api/backup`) the file is read back on a thread of
its own (the layout and the content against the tables; the full world build stays with
`pcds backup verify`), the result is a line in the log (`backup world-... reads back: 33345 world
objects`, or an `ERROR` that says what is wrong), and the metrics `pcds_backup_verify_last_ok` and
`pcds_backup_verify_last_timestamp_seconds` carry it, with an alert rule in
[monitoring.md](monitoring.md).

## Getting a world back

### On the same machine

```bash
pcds backup list
pcds stop                                    # the server must be stopped to replace the world it runs
pcds backup verify                           # pick one that loads
pcds backup restore 20261020-170930          # asks first; --yes does not
pcds start
```

`restore` loads the backup first, and refuses (changing nothing) if it does not load or if the
server would not start it. It asks before it replaces the world, and keeps the world it replaces
in `worlds/backups/` as `<world>-replaced-<time>.json`. It refuses while the server runs. NAME is a
stamp (`20261020-170930`), a file name in `worlds/backups/`, or the path of a file (one you brought
from elsewhere).

To look at a backup without touching the world, restore it as another world; this works while the
server runs:

```bash
pcds backup restore 20261020-170930 --as inspect
pcds world use inspect && pcds restart       # when you want to run it
```

### On a new machine

The machine needs what any server needs ([README](../README.md#quick-start)): the release, the game
(`pcds setup --game DIR`, or `--steam-user NAME` to download it), a user to run it as.

```bash
# 1. The program and the game tables, and a throw-away new world to have a complete installation:
./pcds setup --game "/path/to/The Planet Crafter" --new
# 2. Your settings, from the copy you kept (stop the unit first if it runs):
tar xzf pcds-settings-2026-10-20.tar.gz -C run        # server.env, rules.json, access.json, plugins/
chmod 600 run/server.env run/rules.json
# 3. The world. From the bucket (rules.json names it):
./pcds backup fetch                                   # the newest backup of the world PCDS_WORLD names
./pcds backup verify
./pcds backup restore world-20261020-170930 --yes     # replaces the throw-away world
#    or from a file you copied: ./pcds backup restore /path/to/world-20261020-170930.json --yes
# 4. Run it, and see that a player gets in:
./pcds install-service        # or ./pcds start
./pcds doctor
./pcds selftest
```

Things that differ on a new machine:

* **The invite code** changes with the machine's Steam login. With the same `PCDS_STEAM_TOKEN` in
  `server.env` it is the same. Players using direct IP need the new address and the port forwarded.
* **A world on a DLC planet** needs the rule `world.dlcPlanets` (`rules.json` has it) and the DLC in
  the game folder; `pcds doctor` and `pcds backup restore` say if either is missing.
* **The game version** of the new machine's game must have the tables for the world: a save from
  a newer game than the one installed is refused with `pcds update`.
* **Players' progress** (what they carry, where they stand, what they unlocked) is in the world file,
  saved under their name or SteamID, so they find it again on the new machine.

### From the S3 bucket

`pcds backup fetch` needs only the four `backup.*` rules (put them in `run/rules.json` before the
server starts: `{"backup": {"s3Endpoint": "...", "s3Bucket": "...", "s3AccessKey": "...", "s3SecretKey": "..."}}`).
It copies the newest object under `<backup.s3Prefix><world>-` into `worlds/backups/`, or the one
you name (`pcds backup fetch world-20261020-170930`). Then `verify` and `restore` as above. Without
`pcds`, any S3 client does it: `aws s3 cp s3://BUCKET/pcds/world-....json run/worlds/backups/`.

### From a Docker volume

```bash
docker stop pcds
docker run --rm -v pcds-data:/data -v "$PWD":/backup alpine tar xzf /backup/pcds-data-2026-10-20.tar.gz -C /data
docker start pcds                 # the entrypoint gives /data to PUID:PGID again
```

### Moving a world between machines

Copy `worlds/<name>.json` (or a backup) to the other machine and:

```bash
pcds world import /path/to/world.json NAME     # converts a save of the game's old layouts, copies a current one
pcds world use NAME && pcds restart
```

The game's own saves work the same way (the README's [Worlds and backups](../README.md#worlds-and-backups)).

### When no backup loads

* `pcds backup verify --all` finds any that does; an older one loses the play since then, nothing more.
* After a crash during a save the previous state may be kept next to the world as `world.json.before-panic`
  (a save made after a contained panic) or `world.json.snapshot` (the last hot update's state, which the
  next start runs when it is newer than the save). `pcds backup restore /path/to/world.json.before-panic --as rescued`
  tries the first one.
* The files in `worlds/backups/` with `-replaced-` in the name are the worlds `restore`, `setup --world`
  and `pcds new` replaced.

## What was rehearsed

On a scratch installation, with a world from the late game (33,000 objects, 2,200 inventories, a
DLC planet) and a local S3 server (versitygw) in Docker:

1. The source server ran with the S3 rules and `save.verifyBackups`; `POST /api/backup` made a
   backup, copied it to the bucket and read it back (log line, metrics).
2. A second, empty `PCDS_HOME` on other ports got `pcds setup --new`, `rules.json` copied over,
   `pcds backup fetch`, `verify` (the same counts of objects and inventories as the source), `restore`, `start`; the
   `selftest` player and two scripted players (`pcds load`) joined and played in the restored world.
3. Restoring over a running world, a backup that is cut short, a name that does not exist, a world
   that exists, and an answer of no, were each refused or declined with the world unchanged.

Not rehearsed: AWS S3 itself (only an S3-compatible server), Cloudflare R2, Backblaze B2; a restore from
a Docker volume archive (the `tar` commands are the standard ones; the container itself was tested
with its hot update, [docker.md](docker.md)); a different CPU or a different Linux.
