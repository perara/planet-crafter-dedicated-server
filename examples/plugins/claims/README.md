# claims

A WebAssembly plugin written in Rust: world protection. A player claims the ground around them with
`!claim`. Inside a claim, nobody but the owner, the players the owner trusts and the server's
operators can build or take things apart. It is the example for the one veto the plugin API has
(`rpc.veto`, docs/plugins.md, "The one veto"), for state keyed by `player_id`, and for a veto that
runs on every matching request and so has to be cheap.

## For a player

A claim is a sphere: the point where you stand, and a radius in metres.

| You type | What happens |
|---|---|
| `!claim` | Claims radius 20 around you (the admin may change the default). |
| `!claim 35` | Claims radius 35. A whole number only; the radius is limited (5 to 50 by default). |
| `!unclaim` | Gives up the claim you stand in (the one whose centre is nearest if you stand in several of yours). |
| `!trust Bo` | Bo, who must be online and called exactly that (case does not matter), may build and take apart in all your claims. A name with spaces is typed with its spaces: `!trust Per Arne`. |
| `!untrust Bo` | Takes it back. Works for a player who has gone offline. |
| `!claims` | Lists your claims: number, centre, radius, and who is trusted. |

Rules: you can have 3 claims (`max_claims`). A claim may not overlap the claim of another player (two
spheres overlap when their centres are nearer than the two radii added; touching is allowed). Your own
claims may overlap each other. Up to 8 players can be trusted. A new claim starts with the players you
trust already.

Inside somebody else's claim, a request to build or to take something apart is refused, and you are told
once (at most once in 3 seconds, however often you press the button):

> This ground is claimed by Ann. You cannot build here.
>
> This ground is claimed by Ann. You cannot take this apart here.

**What you see after a refusal.** A refused request is dropped as if it had never arrived, and the world does
not change. For a refused *build* the server also sends no answer at all: the game's own reply to the client
(`CreateAndInstantiateWorldObjectClientRpc`) is never sent. So your game may show the building ghost, or
seem to hang on the placement, for a moment. You do not have to do anything: the chat line tells you why, and
the game settles by itself. Nothing is built, and no other player sees anything. A refused take-apart leaves the
object where it is; your screen may show it gone until the game next syncs it. (Confirmed in the real game:
the build arguments, the deconstruct arguments and the missing answer; how long the ghost stays is for a person to
look at.)

### Exact texts (for tests)

Every text is a `tell` to the player who typed the command or sent the request.

| When | Text |
|---|---|
| `!claim` done | `Claimed radius 30 around (100, 5, -40). Only you and the players you !trust can build or take apart here.` (the centre in whole metres, cut, not rounded) |
| `!claim` with a bad argument (`abc`, `-5`, `5.5`, `1e3`, empty) | `Use: !claim [radius]. The radius is a whole number of metres, 5 to 50; 20 if you give none.` |
| radius out of range | `The radius must be from 5 to 50 metres.` |
| too many claims | `You have 3 claims, the most is 3. Stand in one and use !unclaim to give it up.` (`1 claim` for one) |
| overlap | `That overlaps the claim of Ann. Move away from it or ask for a smaller radius.` |
| the server's 400 claims are all taken | `The server has no room for more claims, tell an admin.` |
| `!unclaim` done | `Removed your claim at (0, 0, 0), radius 20.` (an operator removing another's: `Removed the claim of Ann at (0, 0, 0), radius 20.`) |
| `!unclaim` outside any claim | `You are not standing in a claim.` |
| `!unclaim` in another player's claim | `This ground is claimed by Ann, not by you.` |
| `!trust` done | `Bo can now build and take apart in your claims.` |
| `!trust` again | `Bo is already trusted.` |
| `!trust` yourself | `Your own claims are always yours.` |
| `!trust` with no claim | `You have no claims yet: stand where you want one and use !claim.` |
| `!trust` offline name | `No player called Nobody is online.` |
| two players of one name | `More than one player is called Twin.` |
| 8 trusted already | `Your claims already trust 8 players: use !untrust first.` |
| `!untrust` done | `Bo can no longer build or take apart in your claims.` |
| `!untrust` not trusted | `Bo is not trusted by you.` |
| `!claims` | `Your claims (2 of 3): #1 at (100, 5, -40) radius 30, trusted: Bo; #2 at (500, 0, 0) radius 20;` (more lines when it gets long; a claim with more than 3 trusted shows `trusted: A, B, C and 5 more`) |
| no claims | `You have no claims. Stand where you want one and use !claim.` |
| the server will not say who is online (`world.read` denied) | `I cannot see who is online right now, ask an admin.` |

## For the admin

Install and approve it as any plugin (`pcds plugin install .`, `pcds plugin enable claims`); the approval
shows the two RPC names it may veto. Its claims are kept with the world (they survive restarts, reloads
and hot updates).

### Capabilities and why

| Capability | Why |
|---|---|
| `event.chat` | the commands |
| `event.rpc` | to be told of the two RPCs named in `[events] rpc` |
| `rpc.veto` | to refuse them (the only way to protect anything) |
| `world.read` | where a player stands, who is online (and who is an operator), and where an object is |
| `chat.say` | the answers and the refusals |

The RPCs, exactly as the manifest names them:

- `GroupNetworkBase.DeconstructServerRpc`: taking an object apart. The event's `object` is the **network** id
  of the object; the plugin finds its position with `host.object(ObjectRef::Net(id))`.
- `WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc`: placing a new object. Its `args_json` is
  like `[-12345,[1.5,2.0,-3.25],[0.0,0.5,0.0,1.0],true,false,true,false]`; the second item is the position.

### Settings (`pcds plugin config set claims KEY VALUE`)

| Key | Type | Default | Bounds | Meaning |
|---|---|---|---|---|
| `max_radius` | int | 50 | 5 to 200 | the largest radius of a claim, in metres |
| `min_radius` | int | 5 | 1 to 100 | the smallest radius (if it is above `max_radius`, `max_radius` is raised to it) |
| `default_radius` | int | 20 | 1 to 200 | the radius of a plain `!claim` (kept between the two above) |
| `max_claims` | int | 3 | 1 to 20 | claims per player |

A changed setting reloads the plugin and keeps the claims. Claims that exist stay as they are if a
limit is lowered.

### Operators

Operators (`access.operators`) pass the veto everywhere, and can `!unclaim` any claim they stand in. That
is the way to free ground whose owner is gone. There is no list of all claims and no command to move one.

## Cost

Measured with `pcds plugin check` and `pcds plugin test` (the first call costs the same as the next ones):

- With no claims at all, a matching RPC costs about 6,600 to 7,200 fuel (the event arrives and the plugin
  looks at an empty list).
- With claims, a build request outside every claim: about 13,000 fuel (the position is read by a short
  way, see "How it is made"). A request inside a claim asks the host for the one player (`Host::player`,
  not the whole roster) and costs 25,000 to 40,000 fuel.
- A command: 18,000 to 36,000 fuel; `!trust` about 99,000 (it reads the roster to find the name).
- Budget in the manifest: `fuel_per_event = 200000`, `fuel_per_save = 10000000` (400 claims with 8 trusted
  players each are about 150 KB of state; saving them was not measured at that size).
- 1,000,000 fuel is about 0.3 ms, so even a busy builder costs a few microseconds a request.

## How it is made

`src/lib.rs`, about 480 lines of code and as many of tests.

- **State.** `Claims.claims` is a `Vec<Claim>`; a claim holds the owner's `player_id`, the owner's name (to
  tell others whose ground it is; refreshed whenever the owner uses a command), the centre, the radius and
  up to 8 `(player_id, name)` that are trusted. `save` writes it with a version number (1) and always, even
  when empty, so giving up the last claim is kept. `load` reads it back defensively: it keeps the claims
  that are whole and sane and drops the rest (a radius that is 0, NaN, infinite or over 1,000 metres, a
  centre that is not a number), cuts trusted lists to 8, and refuses a version it does not know (starts
  empty and says so in the log).
- **Bounds.** 400 claims on the server, 8 trusted each, names cut to 32 characters and without control
  characters, a typed name at most 64 characters, `!claims` at most 12 chat lines of 380 characters, the
  list of "who was told lately" at 32 players.
- **The veto path.** `rpc_before` for the two names. First test: no claims, return. Then the position: for
  a deconstruct the object's, from `host.object`; for a build the one in `args_json`. If no claim holds the
  position, return. Only then is the player read, with `Host::player(client)` (to know who asks and whether they are an
  operator). A request that cannot be understood (an arguments text that is not the one we know, an object
  that is unknown to the host) is let through, and the log says so once: a bug here must never stop the game.
  If the player cannot be read (`world.read` denied, or they just left) the request is let through too.
- **Reading the position cheaply.** `json::parse` builds a tree for the whole text and costs about 40,000
  fuel. Since this runs for every build request, `quick_position` cuts the second item out of the text and
  reads three numbers (about 6,000 fuel); anything that is not exactly that falls back to `json::parse`.
- **No clock in an RPC event.** To tell a player at most once in 3 seconds the plugin takes the game time
  from `host.world()` (only when it is about to tell).
- **Names and ids.** A player is matched by `player_id`, which is the same on every visit; `client` is
  only this visit's connection number and is used only to answer.

What it teaches: the veto and when not to use it; the cheap path first; state with a version, bounds and a
defensive `load`; typed settings with bounds the code also enforces; never trusting text typed by a player
(garbage radius, giant names, control characters), and answering with the usage instead of failing.

## Build, test, run

```bash
cd examples/plugins/claims
cargo test                                           # 20 tests with the mock server, no game
cargo clippy --all-targets -- -D warnings
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/claims.wasm plugin.wasm     # or scripts/build-plugin-examples
pcds plugin check .                                  # static checks, fuel of a sample of each command and RPC
pcds plugin test .                                   # scenarios/*.toml in the real host
pcds plugin install .
pcds plugin enable claims
```

Scenarios: `claim-and-protect.toml` (a stranger is refused, the owner, a trusted player and an operator
pass, an unreadable request is let through and logged once, a hot update keeps the claims, `!untrust`,
`!unclaim`) and `limits.toml` (the radius and claim limits from the settings).

## Limits and known gaps

- **Only two requests are guarded**: taking an object apart and placing a new one. Other ways to change
  things inside a claim are not: opening a container and taking what is in it, other tools, terrain,
  planting, moving or rotating, a meteor, a machine. The plugin API gives a veto on client RPCs by name; add
  more names to `[events] rpc` and to `guard` when you know them.
- **An object counts as inside when its own position is inside the sphere.** A big building whose centre is
  outside but that reaches into a claim is not protected there.
- **A deconstruct is checked with the object's network id.** An object that has no network id (the host
  answers "no such object") is let through.
- **Spheres, not squares and not the whole height.** A claim covers the same distance up and down.
- **Trust is for all of an owner's claims at once**, not for one.
- **A player is trusted by the name typed**, among those online. Two players with the same name (apart from
  case) cannot be trusted; ask one to change the name.
- **No claim of the server itself** (a spawn area). An operator can stand there and `!claim` it, then
  trust nobody.
- **The refused player's screen** may not match the world for a moment (see above).
- Reading the position of a build request depends on the shape of its arguments; a game update that
  changes them makes the plugin let every build through and log one warning, until it is changed.

## Checked in the real game, and still to look at

Confirmed by the integration tests against the real game: the two RPC names arrive as `rpc_before` with
`client` set; the build arguments are `[hash,[x,y,z],[qx,qy,qz,qw],true,false,true,false]` (position second);
the deconstruct arguments are `[2.0]` and its `object` is the network id that `host.object(ObjectRef::Net(id))`
resolves; a vetoed build gets no answer.

Still for a person to look at:

1. How long the ghost of a refused build stays on the player's screen, and whether the items spent on the
   building come back or were never taken.
2. That an operator (`access.operators`) shows as `operator` in `world.players()` and so passes.
3. That the state is still there after a server restart and after `pcds upgrade` (hot update).
