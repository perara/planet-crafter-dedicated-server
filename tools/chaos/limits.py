#!/usr/bin/env python3
"""Limits: the front's connection limits, checked with raw datagrams.

Purpose: the direct-IP transport (server/src/utp.rs) caps new sessions per address (6 per 10 s),
sessions per address (8) and sessions in all (128), and drops sessions that stay silent for 30 s.
Loopback addresses are exempt from the per-address caps, so this tool needs the server to listen
on a real local address (--address, default: the one the host routes outward from; nothing is
sent outward). The tool plays the handshake itself (a request datagram, an accept back) and keeps
sessions alive with heartbeats.

  T1  20 handshakes in 2 s from one address: 6 accepted.
  T2  after the 10 s window 4 more are accepted (8 sessions); a ninth is refused, while another
      address (--sources) still gets in.
  T3  with no heartbeats the sessions are gone within 60 s and the address can handshake again.
  T4  140 handshakes from distinct 127.0.0.N addresses fill the table at 128 sessions; one
      real player (the ignored cargo test load::tests::rejoin) tries to join meanwhile
      (only reported).
  T5  once the flood stops the table empties within 90 s and a player joins.

Requirements: python3 (3.9+), a built `pcds` (make build), the game tables, a save to run on
(copied, never written), cargo for the player in T4/T5 (the test is driven by PCDS_LOAD_SERVER,
PCDS_LOAD_PLAYERS, PCDS_LOAD_SECONDS and PCDS_TEST_DATA; or --test-bin a binary built with
`cargo test --release --no-run`). Linux. The server listens on --address:--port only.

Run:  tools/chaos/limits.py --world run/worlds/world.json [--address 192.168.1.20]
          [--sources 172.17.0.1,10.0.0.5]
      --sources: more local addresses of this machine (up interfaces) for the "another address
      still gets in" check; without them that check is skipped. Ports: --port (default 8420).

A pass: exit status 0 and a last JSON line {"pass": true, ...}: every number above is as
listed, and the front and the bank are alive at the end.
"""
import argparse
import os
import socket
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib  # noqa: E402

SIG = b"UTP\x01"


def local_address():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 9))  # a UDP connect sends nothing; it only picks the outgoing address
        return s.getsockname()[0]
    except OSError:
        sys.exit("no local address found: pass --address")
    finally:
        s.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap)
    ap.add_argument("--address", default=None, help="a non-loopback local address for the server to listen on (default: auto)")
    ap.add_argument("--sources", default="", help="more non-loopback local addresses, comma-separated (default: none)")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    lan = args.address or local_address()
    if lan.startswith("127."):
        sys.exit("--address must not be a loopback address: loopback is exempt from the per-address limits")
    sources = [a for a in args.sources.split(",") if a]
    port = args.port
    log = lib.Log(os.path.join(args.work, "limits.log"))
    verdict = lib.Verdict("limits", log)

    srv = lib.Srv("limits", args, listen=f"{lan}:{port}")
    srv.start()

    def request(src):
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.bind((src, 0))
        s.settimeout(0.05)
        tok = os.urandom(8)
        s.sendto(SIG + b"\x01" + tok, (lan, port))
        return s, tok

    def accepted(s, tok, wait=0.3):
        t = time.time()
        while time.time() - t < wait:
            try:
                d, _ = s.recvfrom(2048)
                if d[:4] == SIG and d[4] == 2 and d[5:13] == tok:
                    return True
            except socket.timeout:
                pass
        return False

    def sessions():
        s, j = srv.status()
        return len(j["players"]) if s == 200 else -1

    def hold(lst):
        for s, tok in lst:
            s.sendto(bytes([3]) + tok, (lan, port))  # a heartbeat

    def data(lst):
        for s, tok in lst:
            s.sendto(bytes([1]) + tok + b"\x00", (lan, port))  # a DATA datagram marks the session as used

    def keep(lst, secs):
        t = time.time()
        while time.time() - t < secs:
            hold(lst)
            time.sleep(0.5)

    def expect(what, got, want):
        log(what, "->", got, "(expected", want, ")")
        if got != want:
            verdict.fail(what, got=got, expected=want)

    socks, acc = [], 0
    for _ in range(20):
        s, tok = request(lan)
        socks.append((s, tok))
        acc += accepted(s, tok, 0.1)
    expect("T1 handshakes accepted of 20 from one address", acc, 6)
    data(socks)
    keep(socks, 10.5)
    more = 0
    for _ in range(10):
        s, tok = request(lan)
        more += accepted(s, tok, 0.2)
        socks.append((s, tok))
    expect("T2 more accepted after the window", more, 4)
    data(socks)
    keep(socks, 11.5)
    expect("T2 sessions held by the address", sessions(), 8)
    extra = 0
    for _ in range(5):
        s, tok = request(lan)
        extra += accepted(s, tok, 0.2)
        socks.append((s, tok))
    expect("T2 a ninth session from the same address", extra, 0)
    for src in sources:
        try:
            s, tok = request(src)
        except OSError as e:
            log("T2 source", src, "skipped:", e)
            continue
        expect(f"T2 another address {src} accepted", accepted(s, tok, 0.5), True)

    t = time.time()
    while time.time() - t < 80 and sessions() != 0:
        time.sleep(2)
    left = sessions()
    log(f"T3 sessions after silence: {left} after {time.time() - t:.0f} s")
    if left != 0 or time.time() - t > 60:
        verdict.fail("T3 silent sessions were not dropped within 60 s", left=left)
    s, tok = request(lan)
    expect("T3 the address can handshake again", accepted(s, tok, 0.5), True)

    for s, _ in socks:
        s.close()
    socks, acc = [], 0
    for i in range(140):
        s, tok = request(f"127.0.0.{2 + i}")
        socks.append((s, tok))
        acc += accepted(s, tok, 0.05)
    data(socks)
    keep(socks, 2.5)
    expect("T4 sessions held (limit 128)", sessions(), 128)
    log("T4 handshakes accepted of 140:", acc)

    flood = threading.Event()

    def beat():
        while not flood.is_set():
            hold(socks)
            time.sleep(0.5)

    th = threading.Thread(target=beat, daemon=True)
    th.start()
    bots = lib.Bots("rejoin", args, f"{lan}:{port}", players=1, seconds=12)
    bots.p.wait(timeout=120)
    log("T4 a player while the table is full: joined", [d.get("joined") for _, d in bots.events][:3], "failed", bots.failures()[:2])
    flood.set()
    th.join()
    t = time.time()
    for s, _ in socks:
        s.close()
    while time.time() - t < 90 and sessions() != 0:
        time.sleep(2)
    left = sessions()
    log(f"T5 sessions after the flood: {left} after {time.time() - t:.0f} s")
    if left != 0:
        verdict.fail("T5 the table did not empty within 90 s", left=left)
    bots = lib.Bots("rejoin", args, f"{lan}:{port}", players=1, seconds=6)
    bots.p.wait(timeout=120)
    joined = [d.get("joined") for _, d in bots.events if d.get("joined") is not None]
    log("T5 a player after the flood: joined", joined)
    if not joined:
        verdict.fail("T5 no player could join after the flood", failures=bots.failures()[:2])

    p = srv.pids()
    if not p["serve"] or not p["bank"]:
        verdict.fail("the server is not up at the end", pids=p)
    log("front", lib.proc(srv.pid), "bank", lib.proc(srv.bank_pid() or 0))
    srv.stop()
    verdict.exit(address=lan)


if __name__ == "__main__":
    main()
