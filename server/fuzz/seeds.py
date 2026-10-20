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
put("config", "env", "PCDS_LISTEN=0.0.0.0:7777\nPCDS_ADMIN=127.0.0.1:7782\nPCDS_STEAM=off\n")

# chathooks: method, path, query, body.
put("chathooks", "events", "GET\n/api/chat/events\nafter=0&limit=3\n{}")
put("chathooks", "say", "POST\n/api/chat\n\n{\"text\": \"hello\"}")
put("chathooks", "event", json.dumps({"id": 1, "time": 1, "client": 2, "name": "Ann", "steamId": None, "planet": "Prime", "text": "!tp home", "command": "tp", "args": ["home"]}))

# updater: tags and the JSON of a bank's info.
put("updater", "tags", "v0.6.1\nv0.6.0-rc.1")
put("updater", "info", json.dumps({"tag": "v0.6.1", "protocol": 3, "tablesFormat": 2, "format": 4}) + "\n" + json.dumps({"format": 3, "tablesFormat": 2}))

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

# ---- the targets of round 3 (docs/audits.md) ------------------------------------------------------

# oldsave: the 2022 layout (nine sections: unit levels, one player, objects, inventories, stats,
# messages, story events, a game mode, colour layers), and the layout between the two (tokens in the first).
old_sections = [
    '{"unitOxygenLevel":15.5,"unitHeatLevel":1.0,"unitPressureLevel":2.0,"unitBiomassLevel":3.0}',
    '{"playerPosition":"1,2,3","playerRotation":"0,0,0,1","unlockedGroups":"Pod4x,BootsSpeed1","playerGaugeOxygen":370.0,"playerGaugeThirst":99.0,"playerGaugeHealth":98.0}',
    '{"id":208716012,"gId":"EscapePod","liId":0,"pos":"4,5,6","rot":"0,0,0,1","wear":0,"grwth":0}|\n{"id":107604566,"gId":"ice","liId":0,"pos":"0,0,0","wear":0}',
    '{"id":1,"woIds":"208716012","size":35}|\n{"id":2,"woIds":"","size":12}',
    '{"craftedObjects":765,"totalSaveFileLoad":0,"totalSaveFileTime":0}',
    '{"stringId":"Message_YouAreAConvict","isRead":true}',
    '{"stringId":"StoryEvent-FirstRain"}',
    '{"mode":"Standard","hasPlayedIntro":false}',
    '{"layerId":"PCLayer1","colorBase":"0.2-0.7-0.2-1","colorCustom":"1-1-1-1","colorBaseLerp":100,"colorCustomLerp":0}',
]
old = "".join("\n%s\n@" % s for s in old_sections)
put("oldsave", "oldest", old)
put("oldsave", "between", old.replace('"unitBiomassLevel":3.0}', '"unitBiomassLevel":3.0,"terraTokens":5,"allTimeTerraTokens":9,"unlockedGroups":"Pod4x,Jetpack2"}', 1).replace('"unlockedGroups":"Pod4x,BootsSpeed1",', "", 1))
put("oldsave", "hardcore", old.replace('"mode":"Standard"', '"mode":"Hardcore","planetId":"Selenea"'))

# savelayout: the current layout (the save target's seed) and the same cut after the eighth section.
put("savelayout", "world", open(os.path.join(root, "save", "world"), "rb").read())

# manifest: the first byte picks the parser (0 manifest, 1 data rules, 2 JSON files, 3 signature and keys).
put("manifest", "wasm", b"\x00" + b'[plugin]\nid = "hello-stats"\nname = "Hello"\nversion = "0.1.0"\nkind = "wasm"\nentry = "plugin.wasm"\nabi = 1\ncapabilities = ["event.chat", "event.tick", "world.read", "chat.say"]\n[events]\nsubscribe = ["tick"]\n[[command]]\nname = "stats"\n')
put("manifest", "net", b"\x00" + b'[plugin]\nid = "notify"\nname = "Notify"\nversion = "0.1.0"\nkind = "wasm"\nentry = "plugin.wasm"\nabi = 1\ncapabilities = ["event.chat", "net.http"]\n[events]\nsubscribe = ["chat"]\n[net]\nallow = ["https://discord.com", "https://*.example.com:8443"]\ntimeout_ms = 5000\nmax_response_bytes = 4096\n[[config]]\nkey = "hook"\ndescription = "the webhook path"\n')
put("manifest", "rules", b"\x01" + b'[vars]\ndiscord = "discord.gg/x"\n[[command]]\nname = "rules"\nreply = "Be kind, {name}. {discord} ({players} on {planet}, {time}) {args} {arg1}"\ncooldown_seconds = 10\n[[on_join]]\nsay = "Welcome {name}"\ntell = "Try !rules {{braces}}"\n[[every]]\nseconds = 60\nsay = "Join {discord}"\n[[when_players]]\nat_least = 3\nset_rules = { speed = { growth = 2.0 } }\notherwise_rules = { speed = { growth = 1.0 } }\n')
put("manifest", "approved", b"\x02" + json.dumps({"hello": {"enabled": True, "grant": ["chat.say"], "digest": "ab" * 32, "limits": {"fuelPerEvent": 1000}, "approvedAt": 5, "signer": "k"}}).encode())
put("manifest", "keys", b"\x02" + json.dumps({"keys": [{"id": "d6d9b10b298c53f9fe73f4c9d8570e69", "publicKey": "00" * 32, "name": "Acme", "grant": ["chat.say"], "addedAt": 1}], "revoked": [{"id": "x", "name": "Old", "at": 2}]}).encode())
put("manifest", "tokens", b"\x02" + json.dumps({"tokens": [{"id": "abcd1234", "name": "relay", "hash": "ab" * 32, "scopes": ["events.read"], "ratePerMinute": 60, "createdAt": 3}]}).encode())
put("manifest", "sig", b"\x03" + json.dumps({"publicKey": "00" * 32, "signature": "00" * 64, "digest": "ab" * 32}).encode())

# pluginnet: which function (0 pattern, 1 URL, 2 redirect, 3 request, 4 expansion, 5 address), then its input.
put("pluginnet", "pattern", b"\x00https://*.example.com:8443")
put("pluginnet", "url", b"\x01https://Discord.com:443/api/webhooks/1/x?wait=true")
put("pluginnet", "redirect", b"\x02https://a.example/x/y?z=1\n../q")
put("pluginnet", "expand", b"\x04https://discord.com/${config.hook}?k=${config.a}")
put("pluginnet", "metadata-azure", bytes([5, 168, 63, 129, 16]))
put("pluginnet", "metadata-ecs", bytes([5, 169, 254, 170, 2]))
put("pluginnet", "metadata-aws", bytes([5, 169, 254, 169, 254]))
put("pluginnet", "v6-mapped", bytes([0x83]) + bytes([0] * 10 + [0xff, 0xff, 169, 254, 169, 254]))

# adminhttp: requests as a client sends them.
put("adminhttp", "status", b"GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer fuzz-admin-token-0123456789\r\n\r\n")
put("adminhttp", "post", b"POST /api/rules HTTP/1.1\r\nAuthorization: Bearer fuzz-admin-token-0123456789\r\nContent-Length: 28\r\n\r\n" + b'{"speed": {"growth": 2.0}}  ')
put("adminhttp", "page", b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
put("adminhttp", "folded", b"GET /api/status HTTP/1.1\r\nX: a\r\n Authorization: Bearer fuzz-admin-token-0123456789\r\n\r\n")
put("adminhttp", "bare-lf", b"POST /api/kick HTTP/1.1\nAuthorization: Bearer fuzz-admin-token-0123456789\nContent-Length: 9\n\n{\"id\": 1}\n")

# msg: which message (0 BankInfo ... 11 ChatEvent), then its JSON.
put("msg", "info", b"\x00" + json.dumps({"version": "0.8.0", "protocol": 5, "format": 7, "tablesFormat": 2, "tag": "v0.8.0", "installed": 5, "from": "build"}).encode())
put("msg", "hello", b"\x01" + json.dumps({"version": "0.8.0", "protocol": 5, "gameVersion": "2.103"}).encode())
put("msg", "admin", b"\x07" + json.dumps({"method": "POST", "path": "/api/kick", "body": {"id": 3}}).encode())

# names: world names as typed.
put("names", "plain", "Standard-1.json")
put("names", "path", "/home/me/saves/../x.json")

# adminroutes: a request or several (the bytes pick the method, path and body).
put("adminroutes", "any", bytes(range(1, 90)))
# toxic, tessera, bincodec, wasmhost, loadsave, twin: bytes of any kind are inputs.
put("toxic", "keys", struct.pack("<iiiii", 0, 0, 5, 1000, -2147483648))
put("tessera", "seed", b"\x00" + struct.pack("<i", 12345))
put("loadsave", "ops", bytes(range(1, 60)))
put("twin", "ops", bytes(range(1, 120)))
put("wasmhost", "calls", bytes(range(1, 200)))
put("bincodec", "world-object", b"\x02" + bytes(range(1, 50)))
