#!/usr/bin/env python3
"""Writes small seed inputs for each fuzz target into fuzz/corpus/<target>/.

No game data: captured wire bytes from the test suite (a PlayerReady RPC, a player's transform),
hand-made datagrams and frames, and small JSON. The handoff and game targets seed themselves
(see fuzz/README.md: PCDS_FUZZ_SEED_OUT for a snapshot)."""
import json, os, struct

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "corpus")

def put(target, name, data):
    d = os.path.join(root, target)
    os.makedirs(d, exist_ok=True)
    open(os.path.join(d, name), "wb").write(data if isinstance(data, bytes) else data.encode())

# wire: ops byte 0 then a captured batch header and some packed integers.
put("wire", "ops", bytes(range(14)) + bytes.fromhex("c201017361406248cd8c0b44e82b5340cd0cf74300000000") + b"\x05\x00\x00\x00a\x00b\x00c\x00")
put("wire", "ids", bytes([1, 2, 3, 9, 10, 11, 12, 13]) + bytes([0x09]) + struct.pack("<Q", 2**63) + bytes([0xff] * 12))

# transform: a player's state captured from the real server; one with a quaternion.
put("transform", "player", bytes.fromhex("c201017361406248cd8c0b44e82b5340cd0cf74300000000"))
put("transform", "huge", bytes.fromhex("0501") + b"\x00" + struct.pack("<I", 0) + b"")

# rpcspec: hash of PlayerReadyServerRpc (2442368922, little endian) then its captured arguments;
# the table is picked by hash modulo its size, so these only start near.
body = bytes.fromhex("11010c0000005000630064007300530065006c0066007400650073007400")
put("rpcspec", "player-ready", struct.pack("<I", 0) + body)
put("rpcspec", "chat", struct.pack("<I", 1) + struct.pack("<I", 5) + "hello".encode("utf-16-le"))

# utp: handshake request, a reliable payload with a length-prefixed batch, an ack, fragments.
tok = bytes([1, 2, 3, 4, 5, 6, 7, 8])
def dg(d, flags=1):
    return bytes([len(d) // 6 + 1, flags]) + d.ljust((len(d) // 6 + 1) * 6, b"\0")
def reliable(seq, payload, acked=65535, mask=2**64 - 1):
    return b"\x01" + tok + b"\x03" + bytes([0, 0, 0, 0]) + struct.pack("<HHQ", seq, acked, mask) + payload
stream = struct.pack("<i", 7) + b"batch-1"
put("utp", "reliable", dg(reliable(0, stream[:5])) + dg(reliable(1, stream[5:])) + dg(b"UTP\x01\x01" + tok, 0))
put("utp", "fragments", dg(b"\x01" + tok + b"\x01" + struct.pack("<H", 0x8000 | 0x4000) + struct.pack("<i", 3) + b"abc") + dg(b"\x01" + tok + b"\x02" + b"\0\0" + struct.pack("<H", 0x8000) + b"ab"))

# ipc: a HELLO-like frame and a frame cut short.
def frame(kind, n, body=b""):
    return struct.pack("<IBQ", 9 + len(body), kind, n) + body
put("ipc", "frames", bytes([8]) + frame(1, 0, b'{"v":3}') + frame(13, 7, b'{"method":"GET","path":"/api/status"}') + frame(14, 7)[:5])

# config: a server.env.
put("config", "env", "PCDS_LISTEN=0.0.0.0:7777\nPCDS_ADMIN=127.0.0.1:7782\nPCDS_AUTOSAVE=5\nPCDS_MAX_PLAYERS=16\nPCDS_STEAM=off\n")

# chathooks: method, path, query, body.
put("chathooks", "events", "GET\n/api/chat/events\nafter=0&limit=3\n{}")
put("chathooks", "say", "POST\n/api/chat\n\n{\"text\": \"hello\"}")
put("chathooks", "event", json.dumps({"id": 1, "time": 1, "client": 2, "name": "Ann", "steamId": None, "planet": "Prime", "text": "!tp home", "command": "tp", "args": ["home"]}))

# updater: tags and the JSON of a bank's info.
put("updater", "tags", "v0.6.1\nv0.6.0-rc.1")
put("updater", "info", json.dumps({"tag": "v0.6.1", "protocol": 3, "tablesFormat": 2, "reads": [3, 4]}) + "\n" + json.dumps({"format": 3, "tablesFormat": 2}))

# access and rules: the files an admin writes.
put("access", "lists", json.dumps({"allowedIPs": ["10.0.0.0/8"], "whitelist": ["Ann", "76561198000000001"], "bannedNames": ["Eve"], "bannedIPs": ["203.0.113.7", "2001:db8::/32"]}))
put("rules", "rules", json.dumps({"limits": {"maxPlayers": 4, "buildings": {"Drill4": 10}}, "chat": {"commandPrefix": "!", "timeoutSeconds": 5.0, "webhooks": ["https://example.com/hook"]}, "speed": {"growth": 2.0}}, indent=1))
put("rules", "patch", '{"chat": {"timeoutSeconds": 1e39}}')

# save: the test world's text format (see game::tests::save).
put("save", "world", "﻿\r{\"terraTokens\":0}\r@\r{\"planetId\":\"Prime\"}\r@\r\r@\r{\"id\":208217122,\"gId\":\"EscapePod\",\"pos\":\"558.2,2.47,494.1\",\"rot\":\"0,0,0,1\",\"planet\":-1140328421}\r@\r\r@\r{\"craftedObjects\":0}\r@\r\r@\r\r@\r{\"planetId\":\"Prime\",\"dyingConsequencesLabel\":\"DropSomeItems\"}\r@\r\r@")
put("game", "start", bytes(range(0, 64)))

# front: a Steam split announcement, and the chat settings a bank sends.
put("front", "split", "Split3")
put("front", "chat", json.dumps({"webhooks": ["https://example.com/h"], "timeoutSeconds": 5}))
# handoff is seeded by the target itself: PCDS_FUZZ_SEED_OUT=snapshot.json (a snapshot of the test world).
