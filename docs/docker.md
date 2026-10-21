# Docker, Compose and Kubernetes

How the server runs in a container, how it is updated there (a hot update inside the running
container, or a new image), what happens when it is stopped or killed, and how to run it hardened
(read-only root, no root user, a Kubernetes pod). The README's [Docker](../README.md#docker)
section is the short version. Everything marked "measured" was run for real in Docker 29 (cgroup v2,
Linux 7) against the images this repository builds and the published 0.6.0 to 0.6.2 ones; what was
not run is listed at the end.

## What runs where

```
tini (PID 1)                         the image's init; Kubernetes and `docker run --init=false` work without it
 └ pcds container                    the supervisor: installs this image's program into /data/bin when
    │                                it is newer, reads the game's tables, starts the front, ends last
    └ /data/bin/<active>/pcds serve  the front: UDP and Steam sockets, admin page
       └ /data/bin/<a|b>/pcds bank   the game
```

`/opt/pcds/pcds` is the image's own release: the supervisor and the CLI (`docker exec C pcds ...`).
The front and the game run from `/data/bin/a` and `/data/bin/b`, the two banks, which is why a hot
update works in a container: it writes the other bank in the data volume and hands the world over
to it. Everything is in `/data`, nothing outside it is written (the root filesystem can be
read-only, no `/tmp` is needed).

A hot update replaces the **game**. The **front** (sockets, Steam, admin page) keeps running from
the bank that started it until the server restarts; a restart starts the front of the active bank.
So after a hot update the container runs an old front with a new game, which is fine as long as
the two speak the same front protocol (`pcds upgrade` checks, and refuses with the command to run
when they do not: see "Restarting in place").

## Updating

| What you do | Players | What changes | Next start runs |
|---|---|---|---|
| `docker exec C pcds upgrade [vX.Y.Z]`, the admin page's **Update now**, `POST /api/update`, or the rule `update.auto` | stay connected, the world pauses for the flip (measured: 24 ms on a world of 2500 objects on an idle host, up to 0.8 s while the host was busy compiling, 1.0 s on a world of 100000 objects) | the game (the bank), not the front | the hot-updated release, not the image's |
| `docker exec C pcds upgrade vX.Y.Z --restart` (also `--rollback --restart`) | disconnected; the server restarts in place, about 2 s | game and front, container untouched (same start time, restart count, PID 1) | the release you installed |
| `docker compose pull && docker compose up -d`, `docker run` of a new tag, `kubectl set image` | disconnected; the container is replaced | the image's program, which goes into `/data/bin` only when newer (next section) | see the rule |
| `docker restart C` | disconnected | nothing installed; the front of the active bank starts | the same banks |

A hot update needs the container to reach `api.github.com` and GitHub's release downloads over
https (or a mirror: `PCDS_RELEASES`, https only, except `http://127.0.0.1`). The rule `update.auto`
(`docker exec C pcds rules set update.auto=true`) checks 10 minutes after the start and
then every `update.checkHours` (default 24), installs a later release and flips to it; it is
off by default and the server never contacts GitHub on its own with it off. `pcds upgrade --check`
shows the latest release without installing it.

Players are disconnected by: a new container (compose `up -d` with a new image, `docker restart`),
`--restart`, a bank that crashes, and a game update (the server stops on the old tables until
`pcds update` and a restart). Steam players then get a new invite code unless
`PCDS_STEAM_TOKEN` is set; direct-IP players only join again.

### Which release runs after a new image (the rule)

At every start the supervisor compares the image's program with the two banks in `/data/bin`
(`bank_for` in `cli/setup.rs`):

- a bank that holds this very program (same bytes): nothing to do;
- no release in either bank (a new volume): the image goes into bank `a`;
- the image is newer than **both** banks (a later version, or the same version built later):
  it goes into the standby bank and becomes the active one; the other bank stays as the
  rollback target;
- otherwise the banks win and the server goes on with what they hold: **a pinned image tag does
  not pin the server** once a hot update has put a newer release in `/data/bin`. The same goes
  for the release a rollback left active.

You are told twice: a line at the start of the container's log and the last line of
`pcds status`.

```
pcds: image: pcds 0.6.3; the server runs pcds 0.6.4 from bank b: newer than the image's: a hot update put it in /data/bin, ...
pcds: pcds 0.6.3 is not installed: bank a holds pcds 0.6.4, which is newer, and the server goes on with what the banks hold. To go back to 0.6.3: pcds upgrade v0.6.3 --restart
```

Measured, each with `/data` kept: image 0.6.3 hot-updated to 0.6.4 and recreated from 0.6.3 again:
banks win, both lines. Recreated from 0.6.4 (the same bytes as the bank): the bank, "the image's
version". A volume written by 0.6.3 and an image 0.6.4: the image is installed into the other
bank and runs (0.6.3 stays as the rollback). A volume holding 0.6.1 and 0.6.2 under the published
0.6.0 image: the 0.6.2 bank runs, with the "not installed" line.

To run the image's own release after a hot update: `pcds upgrade vX.Y.Z --restart` (downloads it),
or stop the container and delete `data/bin` (the worlds, settings and tables are untouched, the
image installs itself again).

### Restarting in place

`pcds upgrade vX.Y.Z --restart` is for a release the running world cannot be handed to (its front
protocol differs: `pcds upgrade` without `--restart` then says so and names this command), and for
going back to the release before. In a container it asks the supervisor (SIGUSR1) to stop the
server (the world is saved) and start it again as the same process: the container is not restarted,
so it needs no restart policy and works as PID 1 without tini. It waits until the server answers
again and says which bank runs. Where the front has no such supervisor (a different entrypoint
that runs `pcds serve` itself) it refuses and says `docker restart CONTAINER`. Measured: 1.4 s from
the command to the new world, in Docker with tini and as PID 1.

## Stopping and crashing

| Event | Result (measured) |
|---|---|
| `docker stop` (SIGTERM), also right after an update | world saved, exit 0; 1.4 s on a small world. The worst case is 80 s (front: 30 s for the bank twice, then 20 s for what is left), so give the container 90 s: `--stop-timeout 90`, Compose `stop_grace_period: 90s`, Kubernetes `terminationGracePeriodSeconds: 120` (a test checks the compose file and README against the code's budget) |
| `docker stop` in the middle of the download of an update | the download ends with the server, the world is saved, exit 0 |
| a bank killed (`kill -9`) | the front starts it again from the last snapshot or save and tells the players to join again; the container goes on |
| the front killed | the bank sees the front gone, saves and ends; the container exits and its restart policy brings it back on the active bank; the world was intact |
| the supervisor (`pcds container`) killed (measured), or the container's OOM kill | the container ends at once (tini ends with its child) and the kernel kills the banks before they can save: the last autosave (5 minutes) is what comes back. The restart policy brought the container back with the world |
| `docker kill` | the same as the line above, and Docker counts it as a stop by you: the container is **not** restarted by its policy; `docker start` brings it back |

After every one the container came back (or was started) on the bank `/data/bin/active` names, with
the world, and the hot-updated release was still the one running. The image's health check
(measured as `pcds api GET /api/status`; it is now `pcds health`, the same request without a token)
was run 195 times in a loop through six flips under three players: no failure, the slowest answer
1.1 s on a loaded host, far below its 15 s timeout.

## Hardened environments

Measured, each with a hot update and a restart:

- **Read-only root filesystem**: `docker run --read-only`, `/data` a volume. No `/tmp` is needed.
- **Another user**: `PUID`/`PGID` (the entrypoint gives `/data` to that user and runs the server as
  it), or `--user UID:GID` / Kubernetes `runAsUser`, where nothing is given away and `/data` must be
  writable by that user already (an arbitrary uid 4242 on a world-writable folder worked; in
  Kubernetes `fsGroup` does it). The entrypoint says what to do when it is not.
- **Dropped capabilities**: with `--user`, `--cap-drop ALL --security-opt no-new-privileges` needs
  nothing more. Started as root (PUID/PGID), keep `CHOWN SETUID SETGID DAC_OVERRIDE FOWNER KILL` (without `KILL` tini cannot pass `docker stop` on to the server, which runs as another user: the container exits 1 and the world is not saved). The
  entrypoint then runs the server with `setpriv --no-new-privs`, so nothing the server starts (a
  chat hook program) can gain rights even where the container itself is not run with
  `no-new-privileges`. [compose.yaml](../compose.yaml) turns all of this on, and `docker/test.sh` runs
  its hot update in a container with exactly these flags (see [hardening.md](hardening.md)).
- **Kubernetes-style**: `--entrypoint /opt/pcds/entrypoint.sh` (no tini, `pcds container` is
  PID 1), `--user 1000:1000 --cap-drop ALL --read-only`, SIGTERM at the end: exit 0 with the
  world saved, and the restart in place works as PID 1.
- **Health check**: the image's HEALTHCHECK runs `pcds health` and the pod's probes ask
  `GET /api/health`, which needs no token. It says 200 once the world runs and keeps saying it through
  a flip, and 503 while the world is stopped, loading, or starting again after a crash
  ([monitoring.md](monitoring.md)).

### Memory and CPU

(Measured again for the memory work after 0.8.0: a bank of 100,000 objects is 43 MiB, of 200,000 66 MiB, of a small world 21 MiB; a flip holds 2.3 to 2.7 times the bank, with the snapshot and the front, in all (114 MiB at 100,000 objects, 179 MiB at 200,000); the table below is of 0.6.2's JSON snapshot and a bank three times as big. docs/performance.md has the sizes.)

A hot update holds the old bank, the world's snapshot (a memfd, which counts against the
container's memory) and the new bank, which peaks above the old one while it restores. Measured
(`bench` world of 2551 objects: bank 69 MB, container 104 MB; a world of 100000 objects: bank
321 MB, snapshot 166 MB, the new bank peaking at 488 MB):

| `--memory` | world of 100000 objects | |
|---|---|---|
| 512m | refused before any download | would have been killed: without the check, the kernel took the running bank (crash restart, players dropped) |
| 1g | first flip works, the second is refused (the first snapshot is still held) | without the check its new bank was killed |
| 2g | every flip works; container peak 1.2 GB, the new bank's 485 MB | |

The front therefore reads the container's limit (cgroup v2) before it downloads or flips and
refuses unless about twice the running bank's memory is free, saying how much to give
(`a hot update needs about 578 MB of memory beyond what runs ... Raise the memory limit to at
least 860 MB ... or restart instead (pcds upgrade --restart VERSION)`). A restart needs no extra
memory. Without a cgroup v2 limit (a host, cgroup v1) nothing is checked. The small world flips
inside `--memory 512m --cpus 1` (two or three flips in a row, world stood 100 to 350 ms on a busy
host). Size the limit as about three times the running bank plus the front: 128 MiB for a small
world (the bank is 21 MiB), 256 MiB for 100000 objects (43 MiB), 512 MiB for 200000 (66 MiB).

## Compose

[compose.yaml](../compose.yaml) is the file to start from. Update notes:

```bash
docker compose exec pcds pcds upgrade        # hot update, players stay (the container keeps running)
docker compose exec pcds pcds status         # which release runs, from which bank, against the image's
docker compose pull && docker compose up -d  # a new image: every player disconnects; the rule above decides what runs
docker compose exec pcds pcds upgrade --check
```

Set the rule `update.auto` (`docker compose exec pcds pcds rules set update.auto=true`) for updates by themselves. With it, or after any
`pcds upgrade`, `docker compose pull` no longer changes what runs until the image is newer than
the banks; if you would rather the image decide, leave auto updates off and update by pulling.
Measured: image 0.6.3 with auto updates on, a release found 10 minutes after the start, installed
and flipped; then `docker compose up -d` with the 0.6.4 image: recreated, the same banks, the
log line says the image's version runs.

## Kubernetes

[kubernetes.yaml](kubernetes.yaml): a StatefulSet of one with a volume for `/data` (worlds, settings,
banks), the game's files in a second volume (the server reads the game's tables from them at
every start; copy them once with `kubectl cp`), a UDP service for 7777, the admin page on a
ClusterIP service only (`kubectl port-forward`), HTTP probes on `/api/health` (no token),
a read-only root, a non-root user with `fsGroup`, all capabilities dropped, and
`terminationGracePeriodSeconds: 120`. It validates against the Kubernetes 1.31 schemas with
kubeconform (`-strict`); `kubectl` was not available here, so `kubectl apply --dry-run=client`
was not run, and no cluster was available either: the pod's flags were run in Docker as above.

Update notes for the pod:

- `kubectl exec sts/pcds -- pcds upgrade` is a hot update: players stay. The pod keeps its image
  tag in the manifest, but a bank newer than the image wins (the rule above), so `pcds status`,
  not the manifest, says what runs. Tools that reconcile manifests (Argo CD, Flux) do not see the
  difference: the image field is unchanged.
- Changing the image (`kubectl set image`, a new tag in Git) replaces the pod: every player is
  disconnected for the time of the restart (the 120 s grace period is a limit, a stop takes
  seconds), and the rule decides what runs from the volume.
- `kubectl exec sts/pcds -- pcds upgrade vX --restart` restarts the server inside the pod; the
  pod is not restarted, so the probes see a gap of a few seconds (`failureThreshold: 3` at 30 s
  covers it).
- A pod that is evicted or OOM-killed loses at most the time since the last autosave.
- Size `resources.limits.memory` for the flip (above), or hot updates are refused.

## Testing it

```bash
make docker-test                       # builds the image, then docker/test.sh on it
PCDS_TEST_DATA=run/data/2.103 docker/test.sh IMAGE
docker/test.sh IMAGE                   # without game tables: part 1 only
```

`docker/test.sh` has two parts. Part 1 needs no game and runs in the release workflow before the
image is published: the image's updater installs a release into the standby bank on a read-only
root filesystem, `pcds status` says which release runs against the image's, and a start
without the game says what to do. Part 2 needs a directory of game tables (the game's extracted
data, which cannot be in the repository, so CI cannot run it: run it where the tables are, before
tagging): a server with a new world and a made-up game folder runs in the container, three
scripted players join, `pcds upgrade` hot-updates it to a release published from a local feed (the
image's own program as `v999.0.0`), all three stay connected, `docker restart` comes back on the
new bank, and `pcds upgrade --rollback --restart` restarts the server inside the running container.
The container uses the host's network because the updater takes plain http only for `127.0.0.1`.
It runs in about a minute and a half (part 1 alone, about a minute) on a host with a busy Docker daemon.

## Not verified

- **Podman, rootless Podman**: not installed here. In rootless Docker and Podman the container's
  root is your user on the host, so the entrypoint's default (root, giving `/data` to `PUID:PGID`
  1000) would make the files belong to a sub-uid; start with `--user` (and Podman's
  `--userns=keep-id`) instead, which is the same code path as the Kubernetes runs above.
  Rootless Docker (29.9) is verified: `docker/test.sh` passes on it, both parts, with an image of
  the same files on a Debian base (it gives its folders to the container user with a throwaway
  root container, as there the host user is the container's root and pcds will not run as root).
- **cgroup v1** hosts: the memory check does nothing there, a flip that does not fit gets a bank
  killed as before.
- **Steam in a flip**: every run had Steam off, as in the tests (the first published-image runs
  had Steam on and logged in anonymously; no invite code was joined).
- **A real Kubernetes cluster**, `kubectl apply --dry-run`, LoadBalancer UDP.
- **Updating the front** without disconnecting players: not built, see
  [hot-update.md](hot-update.md#updating-the-front-without-dropping-players-a-design-note).
