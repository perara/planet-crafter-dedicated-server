#!/usr/bin/env python3
"""Disk full: what the server does when its world's disk has no room left.

Run it through tools/chaos/diskfull.sh, which gives it a small tmpfs (a private mount namespace,
no root needed). Run by hand it refuses unless --tfs is a small tmpfs, because the tool fills
the disk it is given up to the last 20 KB.

The server runs on that tmpfs with autosave (1 min) and backups on, four rejoining players.
  A  the disk is filled: a POST /api/save must fail with an error (not claim success), the
     status' lastSave must say so, the world file must be the last good one (loadable, no
     leftover .tmp files), players stay connected, the autosave of the next minute behaves the
     same, and after freeing the space a save works again.
  B  an acknowledged save is made, the players build more, the disk is filled and the server is
     stopped: its final save fails; after freeing the space and starting again the world must
     still hold everything the acknowledged save held.
  C  the bank is killed (kill -9) on a full disk: the front must keep answering, and after
     freeing the space the world runs again and a save works.

Requirements: python3 (3.9+), a built `pcds` (make build), the game tables, a save to run on
(copied, never written; with backups on it must fit the tmpfs several times), cargo for the
players (the ignored test load::tests::rejoin, see tools/chaos/lib.py), Linux.

Run:  tools/chaos/diskfull.sh --world run/worlds/world.json [--bin ...] [--data ...]
      (diskfull.sh passes its options on, and adds --tfs; TFS_SIZE=8m is the tmpfs size)

A pass: exit status 0 and a last JSON line {"pass": true, ...}: every check of A, B and C holds.
"""
import argparse
import glob
import os
import re
import shutil
import signal
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap)
    ap.add_argument("--tfs", required=True, help="the small tmpfs to fill (diskfull.sh mounts it)")
    args = ap.parse_args()
    lib.check_inputs(args)
    tfs = os.path.abspath(args.tfs)
    st = os.statvfs(tfs)
    if st.f_blocks * st.f_frsize > 64 << 20:
        sys.exit(f"{tfs} is bigger than 64 MB: diskfull.py only fills a small tmpfs; run it through tools/chaos/diskfull.sh")
    if os.path.getsize(args.world) * 8 > st.f_blocks * st.f_frsize:
        sys.exit("the save is too big for this tmpfs (backups need several copies): set TFS_SIZE larger, e.g. TFS_SIZE=32m")
    os.makedirs(args.work, exist_ok=True)
    log = lib.Log(os.path.join(args.work, "diskfull.log"))
    verdict = lib.Verdict("diskfull", log)
    home = os.path.join(tfs, "home")
    base_bad, _ = lib.check_save(args.world)

    def df():
        s = os.statvfs(tfs)
        return s.f_bavail * s.f_frsize

    size = os.path.getsize(args.world)
    leave = min(20000, size // 2)  # room for less than one copy of the world: every save must fail

    def fill():
        free = df()
        if free > leave:
            fd = os.open(os.path.join(tfs, "fill.bin"), os.O_CREAT | os.O_WRONLY | os.O_TRUNC)
            os.posix_fallocate(fd, 0, free - leave)
            os.close(fd)
        if df() >= size:
            sys.exit(f"could not fill the tmpfs below the save's size ({df()} bytes free, the save is {size}): the test would prove nothing")

    def unfill():
        try:
            os.unlink(os.path.join(tfs, "fill.bin"))
        except FileNotFoundError:
            pass

    def leftovers():
        return [os.path.relpath(p, home) for p in glob.glob(os.path.join(home, "**", "*.tmp"), recursive=True)]

    def world_ok(what):
        try:
            bad, counts = lib.check_save(srv.world)
        except Exception as e:
            verdict.fail(f"{what}: the world file is unreadable", error=repr(e))
            return None
        new = [b for b in bad if b not in base_bad]
        if new:
            verdict.fail(f"{what}: world invariants", bad=new[:5])
        return counts

    srv = lib.Srv("tfs", args, home=home, rules={"save": {"autosaveMinutes": 1, "backupsKept": 4, "backupMinutes": 1}})
    srv.start()
    bots = lib.Bots("rejoin", args, f"{lib.LOOPBACK}:{srv.port}", players=4, seconds=3600)
    time.sleep(8)
    log("free before", df())
    s, _ = srv.post("/api/save")
    if s != 200:
        verdict.fail("A: the first save on a free disk failed", status=s)
    log("good world", world_ok("before"))

    # A. disk full
    fill()
    log("filled; free", df())
    for i in range(3):
        t = time.time()
        s, j = srv.post("/api/save")
        log("save on a full disk", s, str(j)[:160], f"{(time.time() - t) * 1000:.0f} ms")
        if s == 200:
            verdict.fail("A: a save on a full disk claimed success")
    st = srv.status()[1]
    if (st.get("lastSave") or {}).get("ok") is not False:
        verdict.fail("A: lastSave does not show the failure", lastSave=st.get("lastSave"))
    world_ok("A after failed saves")
    if leftovers():
        verdict.fail("A: leftover .tmp files", files=leftovers())
    log("waiting for an autosave (70 s) on the full disk")
    time.sleep(70)
    st = srv.status()[1]
    log("lastSave after the autosave", st.get("lastSave"), "players", len(st["players"]), "fps", st.get("fps"))
    if not st["players"] or not st.get("fps"):
        verdict.fail("A: the game did not go on while saves failed", players=len(st["players"]), fps=st.get("fps"))
    world_ok("A after the autosave")
    if leftovers():
        verdict.fail("A: leftover .tmp files after the autosave", files=leftovers())
    unfill()
    time.sleep(1)
    s, _ = srv.post("/api/save")
    ls = srv.status()[1].get("lastSave") or {}
    log("save after freeing", s, ls)
    if s != 200 or ls.get("ok") is not True:
        verdict.fail("A: saves did not work again after freeing the space", status=s, lastSave=ls)

    # B. stop on a full disk
    srv.post("/api/save")
    pre = lib.containers(srv.world)
    time.sleep(5)  # the players build more, unsaved
    fill()
    os.kill(bots.pid, signal.SIGSTOP)  # they stand still so the state at the stop is what we compare
    t = time.time()
    srv.post("/api/stop")
    try:
        rc = srv.proc.wait(90)
    except Exception:
        rc = "timeout"
    log(f"stop on a full disk: front rc {rc} after {time.time() - t:.1f} s")
    if rc == "timeout":
        verdict.fail("B: the server did not stop on a full disk within 90 s")
    text = srv.logtext()
    log("log tail:", [l[:300] for l in text.splitlines() if "save" in l.lower() or "snapshot" in l.lower()][-4:])
    srv.kill_all()
    unfill()
    # When the world could not be saved the front says where it kept the snapshot and where to put it for the next start.
    kept = re.search(r"it is kept in (\S+); move it to (\S+) for the next start", text)
    if kept and os.path.exists(kept.group(1)):
        log("moving the kept snapshot", kept.group(1), "->", kept.group(2))
        shutil.move(kept.group(1), kept.group(2))
    srv.proc = None
    try:
        srv.start()
        srv.post("/api/save")
        post = lib.containers(srv.world)
        log("containers in the acknowledged save", len(pre), "after the restart", len(post), "lost", len(pre - post))
        if pre - post:
            verdict.fail("B: objects of an acknowledged save were lost", lost=len(pre - post), sample=sorted(pre - post)[:5])
        world_ok("B after the restart")
    except Exception as e:
        verdict.fail("B: the server did not start again", error=repr(e), log=srv.logtext()[-600:])

    # C. kill -9 the bank on a full disk
    os.kill(bots.pid, signal.SIGCONT)
    time.sleep(5)
    fill()
    bank = srv.bank_pid()
    log("kill -9 bank on a full disk", bank)
    if bank:
        os.kill(bank, signal.SIGKILL)
    t = time.time()
    answered = lib.wait_for(lambda: srv.status(timeout=5)[0] == 200, 40, 0.5)
    s, j = srv.status()
    log(f"after the bank crash: status {s} {j.get('state') if isinstance(j, dict) else j}; answered {'after %.1f s' % (time.time() - t) if answered else 'never'}; admin port listening {lib.tcp_listening(srv.admin)}; front alive {bool(srv.pids()['serve'])}")
    if s != 200:
        verdict.fail("C: the front stopped answering after the bank died on a full disk", status=s, front_alive=bool(srv.pids()["serve"]))
    unfill()
    time.sleep(5)
    s, _ = srv.post("/api/save")
    log("save after freeing", s)
    if s != 200:
        verdict.fail("C: no save after freeing the space and the bank's restart", status=s)
    world_ok("C final")
    bots.stop()
    srv.stop()
    shutil.copy(srv.log, os.path.join(args.work, "diskfull-front.log"))  # the tmpfs goes away with the test
    shutil.rmtree(home, ignore_errors=True)
    verdict.exit(front_log=os.path.join(args.work, "diskfull-front.log"))


if __name__ == "__main__":
    main()
