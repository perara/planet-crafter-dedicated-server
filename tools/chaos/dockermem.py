#!/usr/bin/env python3
"""A server in a container with a memory limit: does it start, play, refuse a hot update that does not fit, and
take a restart update?

Purpose: the hot update keeps two banks alive for a moment and the front refuses a flip when the container's cgroup
has less free than twice the live bank (front/metrics.rs `flip_memory`), instead of letting the kernel kill a
bank. This runs the real image (docker/test.sh's setup: a made-up game folder, the tables, a release feed made of the
image's own program) on a late-game world under `docker run --memory SIZE --memory-swap SIZE` with 8 scripted
players (run on the host, so the container's memory is the server's), and checks:
  - the server starts and the 8 players play for --minutes;
  - `pcds upgrade vA` (a hot update) either flips with the players kept, or is refused with a message that says
    why, with the server and the players untouched;
  - `pcds upgrade vB --restart` (install, stop, start in place) works either way;
  - the kernel killed nothing (memory.events oom_kill is 0) and the container did not restart by itself.
Numbers: the cgroup's memory.current before, at its peak (memory.peak) and after each step.

Requirements: python3 (3.9+), docker (rootless is fine: DOCKER_HOST), an image built from the Dockerfile (make docker),
the game tables, a save, a built `pcds` for the players (`pcds load` on the host).

Run:  tools/chaos/dockermem.py --image IMAGE --data TABLES --world SAVE --bin PCDS --work DIR --memory 512m [--port 8648]
Exit status 0: the checks above held. Ports: --port (UDP), +1 (admin), +2 (the release feed), +3 spare.
"""
import argparse
import hashlib
import http.server
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib  # noqa: E402


def sh(*a, check=True, timeout=300, **kw):
    r = subprocess.run(a, capture_output=True, text=True, timeout=timeout, **kw)
    if check and r.returncode:
        raise RuntimeError(f"{' '.join(a)[:200]}: rc {r.returncode}: {r.stdout[-600:]} {r.stderr[-600:]}")
    return r


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap, port=8656)
    ap.add_argument("--image", default="pcds-qa-soak2")
    ap.add_argument("--memory", default="512m", help="docker --memory (and --memory-swap) of the container (default %(default)s)")
    ap.add_argument("--minutes", type=float, default=10, help="how long the players play before the updates (default %(default)s)")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    name = f"pcds-qa-soak2-dm-{args.memory}"
    W = os.path.join(args.work, f"dm-{args.memory}-{int(time.time())}")  # a folder of an earlier run can hold files of the container's user
    os.makedirs(W)
    log = lib.Log(os.path.join(W, "events.log"))
    verdict = lib.Verdict(f"dockermem-{args.memory}", log)
    UDP, ADMIN, FEED = args.port, args.port + 1, args.port + 2
    uid, gid = os.getuid(), os.getgid()
    rootless = "rootless" in sh("docker", "info", "--format", "{{.SecurityOptions}}", check=False).stdout

    def given(path):
        if rootless:
            sh("docker", "run", "--rm", "--user", "0:0", "--entrypoint", "chown", "-v", f"{path}:/data", args.image, "-R", f"{uid}:{gid}", "/data")

    def once(data, *extra, entry="pcds", cmd=()):
        return sh("docker", "run", "--rm", "--read-only", "--tmpfs", "/tmp", "--network", "host", "--user", f"{uid}:{gid}", "-e", f"PCDS_RELEASES=http://127.0.0.1:{FEED}/releases", "-v", f"{data}:/data", *extra, "--entrypoint", entry, args.image, *cmd, check=False)

    # The feed: the image's own program as two releases.
    cid = sh("docker", "create", args.image).stdout.strip()
    stage = os.path.join(W, "stage")
    for f in ("pcds", "libsteam_api.so", "web/index.html"):
        os.makedirs(os.path.dirname(os.path.join(stage, f)), exist_ok=True)
        sh("docker", "cp", f"{cid}:/opt/pcds/{f}", os.path.join(stage, f))
    sh("docker", "rm", cid)
    feed = os.path.join(W, "feed")
    os.makedirs(os.path.join(feed, "releases", "tags"))
    os.makedirs(os.path.join(feed, "download"))
    for tag in ("v999.0.0", "v999.0.1"):
        rel = f"pcds-{tag}-linux-x86_64"
        tar = os.path.join(feed, "download", rel + ".tar.gz")
        with tarfile.open(tar, "w:gz") as tf:
            for f in ("pcds", "libsteam_api.so", "web/index.html"):
                tf.add(os.path.join(stage, f), arcname=f"{rel}/{f}")
        sha = hashlib.sha256(open(tar, "rb").read()).hexdigest()
        open(tar + ".sha256", "w").write(f"{sha}  {rel}.tar.gz\n")
        base = f"http://127.0.0.1:{FEED}/download"
        doc = {"tag_name": tag, "assets": [{"name": rel + ".tar.gz", "browser_download_url": f"{base}/{rel}.tar.gz"}, {"name": rel + ".tar.gz.sha256", "browser_download_url": f"{base}/{rel}.tar.gz.sha256"}]}
        json.dump(doc, open(os.path.join(feed, "releases", "tags", tag), "w"))
    shutil.copy(os.path.join(feed, "releases", "tags", "v999.0.1"), os.path.join(feed, "releases", "latest"))

    class Quiet(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **k):
            super().__init__(*a, directory=feed, **k)

        def log_message(self, *a):
            pass

        def end_headers(self):  # the updater must not meet a closed socket it expected to reuse
            self.send_header("Connection", "close")
            super().end_headers()

    httpd = http.server.ThreadingHTTPServer((lib.LOOPBACK, FEED), Quiet)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()

    # The server's folder: a made-up game folder whose build id the tables carry, the world, rules.
    game = os.path.join(W, "game")
    os.makedirs(os.path.join(game, "Planet Crafter_Data", "Managed"))
    open(os.path.join(game, "Planet Crafter.exe"), "w").close()
    open(os.path.join(game, "Planet Crafter_Data", "Managed", "Assembly-CSharp.dll"), "w").write("a made-up game build for dockermem.py\n")
    d2 = os.path.join(W, "d2")
    os.makedirs(os.path.join(d2, "data"))
    os.makedirs(os.path.join(d2, "worlds"))
    shutil.copytree(args.data, os.path.join(d2, "data", "test"))
    shutil.copy(args.world, os.path.join(d2, "worlds", "world.json"))
    d0 = os.path.join(W, "d0")
    os.makedirs(d0)
    given(d0)
    said = once(d0, "-e", "PCDS_GAME_DIR=/game", "-v", f"{game}:/game:ro", cmd=("status",))
    import re
    m = re.search(r"game build ([0-9a-f]+) has no tables", said.stdout + said.stderr)
    if not m:
        sys.exit("pcds status did not name the made-up game's build:\n" + said.stdout + said.stderr)
    open(os.path.join(d2, "data", "test", "buildid"), "w").write(m.group(1))
    open(os.path.join(d2, "server.env"), "w").write(f"PCDS_LISTEN=127.0.0.1:{UDP}\nPCDS_STEAM=off\nPCDS_ADMIN=127.0.0.1:{ADMIN}\nPCDS_ADMIN_TOKEN=test\nPCDS_WORLD=world\nPCDS_RELEASES=http://127.0.0.1:{FEED}/releases\n")
    os.chmod(os.path.join(d2, "server.env"), 0o600)
    json.dump({"save": {"autosaveMinutes": 1, "backupsKept": 3, "backupMinutes": 5}, "limits": {"maxPlayers": 16}, "world": {"dlcPlanets": ["Humble"]}}, open(os.path.join(d2, "rules.json"), "w"))
    given(d2)  # the container's user 1000 writes here

    def cg(path):
        r = sh("docker", "exec", name, "cat", f"/sys/fs/cgroup/{path}", check=False)
        return r.stdout.strip() if r.returncode == 0 else ""

    def mem():
        cur, peak = cg("memory.current"), cg("memory.peak")
        ev = dict(l.split() for l in cg("memory.events").splitlines() if l.strip())
        return dict(current_mb=round(int(cur) / 1048576, 1) if cur else None, peak_mb=round(int(peak) / 1048576, 1) if peak else None, limit=cg("memory.max"), oom_kill=int(ev.get("oom_kill", -1)), oom=int(ev.get("oom", -1)), max_events=int(ev.get("max", -1)))

    def status():
        r = sh("docker", "exec", name, "pcds", "api", "GET", "/api/status", check=False)
        try:
            return json.loads(r.stdout)
        except ValueError:
            return {}

    def healthy(timeout=300):
        end = time.time() + timeout
        while time.time() < end:
            if status().get("state") == "running":
                return True
            if sh("docker", "inspect", "-f", "{{.State.Running}}", name, check=False).stdout.strip() != "true":
                return False
            time.sleep(1)
        return False

    sh("docker", "rm", "-f", name, check=False)
    t_start = time.time()
    sh("docker", "run", "-d", "--name", name, "--network", "host", "--read-only", "--tmpfs", "/tmp", "--memory", args.memory, "--memory-swap", args.memory, "--stop-timeout", "90",
       "-e", f"PUID={uid}", "-e", f"PGID={gid}", "-v", f"{d2}:/data", "-v", f"{game}:/game:ro", args.image)
    out = dict(memory=args.memory)
    try:
        up = healthy()
        out["start_seconds"] = round(time.time() - t_start, 1)
        out["after_start"] = mem()
        log("start", up, out["start_seconds"], "s", out["after_start"])
        if not up:
            verdict.fail("the server did not start within the limit", logs=sh("docker", "logs", "--tail", "15", name, check=False).stderr[-800:], mem=out["after_start"])
            raise SystemExit
        started_at = sh("docker", "inspect", "-f", "{{.State.StartedAt}}", name).stdout.strip()
        bots = subprocess.Popen([args.bin, "load", f"127.0.0.1:{UDP}", args.data, "8", str(int(args.minutes * 60 + 600)), "200"], stdout=open(os.path.join(W, "load.out"), "ab"), stderr=subprocess.STDOUT)
        end = time.time() + 90
        while time.time() < end and len(status().get("players", [])) < 8:
            time.sleep(2)
        n = len(status().get("players", []))
        log("players", n)
        if n < 8:
            verdict.fail("8 players did not get in", players=n)
        time.sleep(args.minutes * 60)
        out["after_play"] = mem()
        out["status_play"] = {k: status().get(k) for k in ("busyPercent", "longestLoopMicros")}
        st = status()
        out["bank_rss_mb"] = round(((st.get("banks") or {}).get("live") or {}).get("pid", 0) and lib.proc(st["banks"]["live"]["pid"])["rss"] / 1024, 1) if lib.proc(((st.get("banks") or {}).get("live") or {}).get("pid", 0)) else None
        log("after play", out["after_play"])

        # A hot update.
        t = time.time()
        r = sh("docker", "exec", name, "pcds", "upgrade", "v999.0.0", check=False)
        out["hot"] = dict(rc=r.returncode, seconds=round(time.time() - t, 1), text=(r.stdout + r.stderr).strip()[-500:], mem=mem())
        players_after = len(status().get("players", []))
        out["hot"]["players_after"] = players_after
        refused = "memory" in (r.stdout + r.stderr).lower() and r.returncode != 0
        out["hot"]["refused_for_memory"] = refused
        log("hot update", out["hot"])
        if r.returncode != 0 and not refused:
            verdict.fail("the hot update failed for a reason other than memory", text=out["hot"]["text"])
        if r.returncode != 0 and players_after < 8:
            verdict.fail("a refused hot update cost players", players=players_after)
        if r.returncode == 0 and players_after < 8:
            verdict.fail("a hot update dropped players", players=players_after)
        if not status().get("state") == "running":
            verdict.fail("the world is not running after the hot update attempt")

        # A restart update: install, stop, start inside the container.
        t = time.time()
        r = sh("docker", "exec", name, "pcds", "upgrade", "v999.0.1", "--restart", check=False, timeout=600)
        up = healthy(300)
        out["restart"] = dict(rc=r.returncode, seconds=round(time.time() - t, 1), world_back=up, text=(r.stdout + r.stderr).strip()[-500:], mem=mem())
        log("restart update", out["restart"])
        if r.returncode != 0 or not up:
            verdict.fail("the restart update did not work", text=out["restart"]["text"], world_back=up)
        if sh("docker", "inspect", "-f", "{{.State.StartedAt}}", name).stdout.strip() != started_at:
            verdict.fail("the container itself restarted")
        time.sleep(30)
        out["players_end"] = len(status().get("players", []))
        out["final"] = mem()
        if out["final"]["oom_kill"] not in (0, -1):
            verdict.fail("the kernel killed a process of the container", memory=out["final"])
        bots.terminate()
    except SystemExit:
        pass
    except Exception as e:
        verdict.fail("raised", error=repr(e))
    finally:
        out["logs_tail"] = sh("docker", "logs", "--tail", "12", name, check=False).stderr[-700:]
        sh("docker", "rm", "-f", name, check=False)
        sh("docker", "run", "--rm", "--user", "0:0", "--entrypoint", "chown", "-v", f"{W}:/w", args.image, "-R", f"{uid}:{gid}", "/w", check=False)
        httpd.shutdown()
    print(json.dumps({"tool": "dockermem", "pass": not verdict.failures, "failures": verdict.failures, **out}, default=str))
    sys.exit(1 if verdict.failures else 0)


if __name__ == "__main__":
    main()
