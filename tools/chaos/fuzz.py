#!/usr/bin/env python3
"""Fuzz: garbage datagrams at the front's UDP port.

Purpose: the direct-IP transport (server/src/utp.rs) parses whatever arrives. This tool sends a
seeded mix of: pure noise, the transport's signature with a random rest, real sessions
followed by data/heartbeat/disconnect datagrams of random shape on their token, reliable
payloads whose batch length is huge or negative, and oversize datagrams. The server must
neither die nor restart its bank, nor panic, nor grow without bound. (Sources are 127.0.0.N, so
the per-address limits do not apply; tools/chaos/limits.py tests those.)

Requirements: python3 (3.9+), a built `pcds` (make build), the game tables, a save to run on
(copied, never written). Linux (/proc). No cargo needed.

Run:  tools/chaos/fuzz.py --world run/worlds/world.json [--seed 1] [--seconds 60]
          [--max-rss-growth-mb 150]      Ports: --port (default 8420, admin 8421).

A pass: exit status 0 and a last JSON line {"pass": true, ...}: the front and the same bank are
alive after the run and the status answers; neither process grew by more than the allowed MB;
the server log has no panic.
"""
import argparse
import os
import random
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lib  # noqa: E402

SIG = b"UTP\x01"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter, epilog="See the header of this file for requirements and what a pass is.")
    lib.add_server_args(ap)
    ap.add_argument("--seed", type=int, default=1, help="the seed (default %(default)s)")
    ap.add_argument("--seconds", type=int, default=60, help="how long to send (default %(default)s)")
    ap.add_argument("--max-rss-growth-mb", type=int, default=150, help="allowed growth of each process (default %(default)s)")
    args = ap.parse_args()
    lib.check_inputs(args)
    os.makedirs(args.work, exist_ok=True)
    log = lib.Log(os.path.join(args.work, f"fuzz{args.seed}.log"))
    verdict = lib.Verdict("fuzz", log)
    rnd = random.Random(args.seed)
    srv = lib.Srv(f"fuzz{args.seed}", args)
    srv.start()
    fp, bp = srv.pid, srv.bank_pid()
    base = (lib.proc(fp), lib.proc(bp))
    target = (lib.LOOPBACK, srv.port)

    def newsock(i):
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.bind((f"127.0.0.{2 + i % 200}", 0))
        s.setblocking(False)
        return s

    sent, sessions, t0 = 0, [], time.time()
    while time.time() - t0 < args.seconds:
        mode = rnd.randrange(6)
        if mode == 0:  # pure noise
            s = newsock(rnd.randrange(50))
            s.sendto(rnd.randbytes(rnd.randrange(0, 1500)), target)
            s.close()
        elif mode == 1:  # the signature's prefix with a random rest
            s = newsock(rnd.randrange(50))
            s.sendto(SIG[:rnd.randrange(1, 5)] + rnd.randbytes(rnd.randrange(0, 40)), target)
            s.close()
        elif mode == 2:  # a session request; the socket is kept for the datagrams below
            s, tok = newsock(rnd.randrange(50)), rnd.randbytes(8)
            s.sendto(SIG + b"\x01" + tok, target)
            sessions.append((s, tok))
            if len(sessions) > 100:
                sessions.pop(0)[0].close()
        elif mode == 3 and sessions:  # data, heartbeat, disconnect datagrams of random shape on a session
            s, tok = rnd.choice(sessions)
            kind = rnd.choice([1, 1, 1, 2, 3, 0, 255])
            body = bytes([kind]) + tok + rnd.randbytes(rnd.randrange(0, 1300))
            if kind == 1 and rnd.random() < 0.7:
                body = bytes([1]) + tok + bytes([rnd.choice([3, 0, 1, 2, 4, 255])]) + rnd.randbytes(rnd.randrange(0, 40))
            s.sendto(body, target)
        elif mode == 4 and sessions:  # a reliable payload with a huge or negative batch length
            s, tok = rnd.choice(sessions)
            hdr = rnd.randbytes(2) + rnd.randbytes(2) + rnd.randrange(0, 65536).to_bytes(2, "little") + rnd.randrange(0, 65536).to_bytes(2, "little") + rnd.randbytes(8)
            payload = rnd.choice([(2**31 - 1).to_bytes(4, "little"), (-5).to_bytes(4, "little", signed=True), (1 << 20).to_bytes(4, "little"), b"\x00\x00\x00\x00", b""]) + rnd.randbytes(rnd.randrange(0, 200))
            s.sendto(bytes([1]) + tok + bytes([3]) + hdr + payload, target)
        else:  # oversize
            s = newsock(rnd.randrange(50))
            s.sendto(rnd.randbytes(rnd.randrange(1500, 9000)), target)
            s.close()
        sent += 1
        if sent % 2000 == 0:
            time.sleep(0.05)
        if sent % 20000 == 0 and srv.proc.poll() is not None:
            log("the front died at datagram", sent)
            break
    for s, _ in sessions:
        s.close()
    time.sleep(3)
    s, j = srv.status()
    f, b = lib.proc(fp), lib.proc(bp)
    log("datagrams", sent, "status", s, "front", f, "bank", b, "base", base)
    if srv.proc.poll() is not None:
        verdict.fail("the front died", rc=srv.proc.returncode)
    elif s != 200:
        verdict.fail("the status does not answer", status=s)
    elif srv.bank_pid() != bp:
        verdict.fail("the bank was restarted", old=bp, new=srv.bank_pid())
    for name, a, c in (("front", base[0], f), ("bank", base[1], b)):
        if a and c and (c["rss"] - a["rss"]) / 1024 > args.max_rss_growth_mb:
            verdict.fail(f"the {name}'s memory grew", from_mb=a["rss"] // 1024, to_mb=c["rss"] // 1024)
    if "panicked" in srv.logtext():
        verdict.fail("a panic in the server log", log=srv.log)
    srv.stop()
    verdict.exit(seed=args.seed, datagrams=sent)


if __name__ == "__main__":
    main()
