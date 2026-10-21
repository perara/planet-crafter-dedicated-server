# The game's clock

The game keeps time in a 32-bit float (Unity's `Time.time`) and every timer in it is
`Time.time + interval` in that float. A float counts 1 ms at the first hour, 0.25 s from 24 days and
1 s from 97 days; a 0.1 s step added to a due time of 2^21 s adds nothing at all. The game is never
run for months; this server is. So the server keeps its own clock in 64 bits and runs every timer on it,
and for the first hour of a run it computes with the floats the game computes with, so that everything
it sends and saves in that hour is what the game's would be (the parity runs, the bench fingerprints
and the handoff proof depend on it).

## Design

`server/src/clock.rs`, one small type and two functions:

* **`GameTime`**: a point on the clock in seconds, an `f64`, used for the clock itself (`Game::time`)
  and for every due time. `GameTime::at(clock)` is the only place a time is made from the real clock,
  and the only place that decides which arithmetic is used.
* **Below `EXACT_AFTER` (3600 s) a sum is the game's f32 sum**: `time + interval` rounds as the game
  rounds it, and the f32 result is widened to the f64 without a change (every f32 is an f64), so the
  comparisons of the first hour are the game's, bit for bit. **From there it is an f64 sum**: a timer
  keeps its microsecond for the next hundred years. The rule is decided by the value the sum starts
  from (`now + interval`, `due + step`), so call sites pass no clock around and read as the game's.
  `GameTime::f32()` is the one way back into a float (an RPC argument, a difference of two times).
* **`GameTime::take_due(now, step)`** is `while now >= next { next += step; ... }` for a timer that
  repeats at a fixed pace, as a count: a loop while the timer is young (at most an hour of steps), a
  division after, so that a jump of the clock costs the same as a step (finding 14 of the audit: 3 days
  of game time was 14 CPU minutes in `units::tick`). A step that has side effects keeps its loop; there
  is none left (below).
* **`add_young(total, delta, now)`** is the same two phases for an accumulator that is not a time:
  the world units' totals. The game's `WorldUnit._currentTotalValue` is a **float** (decompiled
  `SpaceCraft.WorldUnit`: `protected float _currentTotalValue`, `Compute` adds `perSec * dt * factor`), so
  a total stops growing when its step is under half of its own (rate × 1.7e6: the bench world's Heat at
  17.5 a second, 4.9 million, in 36 days). Ours is an f64 that holds the float's values in the first
  hour and is summed exactly after; what the clients read and the save writes is `total as f32`, as the
  game's float. One exception in the first hour, on purpose: a float that has outgrown its step (it
  would not move at all, in the game, whatever the buildings make) is summed in f64 at once, so a
  server restarted in a late world does not stand still for the hour. Everything the float does move
  by is still its sum.
* **The tick counter** (`Game::tick_count`, `u64`): the game's `NetworkTime.Tick` is an `int` made by
  `(int)(time / interval)`. `Game::tick_number()` is that int on the wire: the same up to 2^31 ticks
  (828 days at 30 a second), then the low 32 bits, wrapping as an unchecked int does. What the game's
  own conversion does there is undefined in C# (the game would stay at one value for good: its network
  time would stop); the clients' `NetworkTimeSystem` hard-resets its time when the server's jumps by
  0.2 s (`k_HardResetThresholdSeconds`), so the wrap costs a client one resync. Nothing that counts ticks
  on the server stops: the logistics frame loop runs on `tick_count`.

The state of the game (`Game::time`, every due time, `WorldObject::lock_time` and `pet_time`, the units'
totals) is part of the snapshot, so **`handoff::FORMAT` is 6** (was 5): the front refuses a hot flip across
the format and a restart install does it, as `docs/hot-update.md` says. **The save text does not change**
(the game does not save its clock; `unitXLevel` is the float `total as f32`).

### After a restart

The game starts its clock at 0 and so does the server after a restart: every timer is made again
at 0 (`init`, `on_instantiated`), the units' totals come from the save's float levels, and the first hour
is the game's float arithmetic again. A hot flip keeps the clock (`started` and `paused` are in the
snapshot). A restart is a new game on the same save, not the continuation of the old one: see "What is
not the same" below.

## Where time is used

`GameTime` is the type of every field that holds a time of the game; a float is not accepted where one is
expected, so the table is what the compiler checks. (`grep -rn "GameTime" server/src`.)

| Where | What | Verdict |
|---|---|---|
| `game.rs` `Game::time` | Time.time, set every tick from the clock minus the paused seconds | converted (was f32) |
| `game.rs` `next_time_sync` | TIME_SYNC to the clients every second | converted |
| `game.rs` `tick_count` / `tick_number` | network tick | `u64` inside, the int of the game on the wire; wraps at 2^31 (above) |
| `units.rs` `next_define`, `next_compute` | the 1 s and 0.1 s steps of the world units | converted; `take_due`, define passes capped at 3, compute in one sum |
| `units.rs` `Unit::total` | the world units' totals | f64 (the game's float while young; above) |
| `gauges.rs` `next_tick` | the 0.2 s drain of every player | converted (`advance`) |
| `energy.rs` `next_check` | RequireEnergyHandler, every 2 s | converted |
| `scene.rs` `Timers` | Invoke timers: despawn of drops, final deconstruction | converted |
| `handlers.rs` `DefaultColors.pending` | 0.75 s after a building is placed | converted |
| `model.rs` `lock_time`, `pet_time`; `inventories.rs` | locked items; the last petting | converted |
| `machines.rs` `next_due`, `converter`, `floater` | the skip bound of the machine walk, converters (2 s), floaters | converted |
| `machines/production.rs` | generators, mining rays, auto-crafters, disintegrators (`next`) | converted |
| `machines/growers.rs` | linked growers (2 s), vegetation coroutines, harvestables | converted |
| `machines/veins.rs` `next_check` | the ore veins' 1 s check | converted |
| `animals.rs` `next_frame` | the 30 Hz frame of the animals, the vehicles and the interpolators | converted |
| `animals/interpolation.rs` `start_time` | an elevator's start, told to the clients as the float | converted (`f32()` on the wire) |
| `animals/spawn.rs` `last_update` | the hunger and growth of shelter animals | converted |
| `animals/vehicles.rs` `next_check`, `last_frame` | rigidbody check (0.15 s), the frame's delta | converted |
| `status.rs` | the start of dying | converted |
| `meteo.rs` | the try loop, UpdateMeteo, the asteroids (`next_check`, `last_sent`, `landings`), the day/night cycle (`next`, `full_phase_reached`) | converted |
| `meteo/story.rs`, `meteo/explode.rs` | the story events' try, the delayed blasts | converted |
| `rockets.rs`, `rockets/back_and_forth.rs` | the despawn of ignited rockets, the departure's `switch_at`, `last_tick` (a delta), the update and relaunch of exchange rockets | converted |
| `instances.rs` | the opened instance's second (`next_second`), the generators' despawn | converted |
| `logistics.rs` | the frame loop (`last_tick`: `u64`), the 1 s task refresh (`next_tasks`) | converted; the tick-to-time conversion is the float's while young |
| `events.rs`, `plugins/` (`view.rs`, `mod.rs`), `bank/` | `game_ms`, `gameTime` of the status | read from `GameTime::secs()` |
| `game.rs` `clock`, `paused`, `last_clock` | the real clock, the seconds the world stood still | not needed: already f64 (seconds from an `Instant`, 1e-8 s at a year) |
| `handlers.rs` `Limits::chat`, `motion` | the clients' rate limits | not needed: f64 clock, differences only |
| `bank/`, `front/`, `chathooks/`, `updater.rs`, `utp.rs`, `ipc.rs`, `admin.rs`, `cli/`, `s3.rs`, `save.rs` | deadlines, timeouts, backups and stamps | not needed: `Instant` or wall-clock seconds in 64 bits, not the game's time |
| `plugins/mod.rs` `next_tick`, `last_tick` | the host's own ticks | not needed: `i64`, wall clock |
| `meteo.rs` `DayNight.pong`, `value` | the day's accumulators | not needed: reset every cycle (a few hundred) |
| `WorldObject::growth`, `hunger` | grown / starving percentages | not needed: bounded 0..100 and −100..100, steps are deltas |
| `net.rs` `next_id`, `removed`; `units.rs` `constructed_index`; `logistics.rs` `next_task`; `meteo.rs` `next_id` | counters | not needed: `u64` or `u32` at a rate of one per several seconds at most (the shortest wrap is the asteroids': 2 400 years) |

The two loops that ran a step per step of a jump are `units::tick` (`take_due`, above) and
`logistics::tick` (at most one second of frames, `ponytail:`); every other timer fires once per tick
(`if now >= due { due = now + interval }`), so a jump costs the same as a tick.

## The proof

* `clock::tests`: the first hour is the game's float arithmetic, step by step; an old clock keeps its
  steps (1 to 3650 days); `take_due` is the loop it replaces, count and due time; the accumulator.
* `units::tests`: the totals grow at the buildings' rate at 0 to 400 days; a total of 40 million keeps
  growing; a jump of 3 days is one catch-up (under a second); three define passes are all there are.
* `timetravel::every_system_takes_its_steps_at_the_same_pace_at_any_age_of_the_server`: the same
  world, with a player, a heater on a generator, a water collector, an incubator, a vegetube, a moving
  elevator and a dropped item, played for eight minutes of game time at the age of 0, 1, 3, 7, 25, 60,
  200, 400 and 900 days (the clock of the game begins there, so every timer was set at that age); the
  ticks at which each system took a step are the same as at age 0.
  `cargo test --release timetravel -- --nocapture` prints the table.

Steps taken in 460 s of game time (a step is a change seen from outside) and, below it, how many ticks (7.3 ms)
the steps are from those of age 0 at most. Before: this change's parent (integrate with the units, gauges
and vehicle fixes; the clock begun at the age). After: this change.

```
before
    days    animals  collector     gauges  incubator  time sync     timers      units   vegetube         elevator       heat grown    network ticks   oxygen drained  vegetube growth
       0      12602          3        263         25        460          1       4599        459        91.996063       137.996017     13799.000000       100.000000        41.195946
       1      14719          3        263         25        460          1       4599        459        91.995316       137.996017     13799.000000       100.000000        41.195946
       3      14720          3        263         25        461          1       4599        460        92.000000       137.996017     13799.000000       100.000000        41.223572
       7       7360          3        263         25        461          1       4600        460        92.000000       138.026016     13799.000000       100.000000        41.223572
      25       1840          3        263         25        461          1       1840        460        92.000000       137.996017     13799.000000       100.000000        41.223572
      60        920          3        263         25        461          1        920        460        92.000000       138.026016     13799.000000       100.000000        41.223572
     200        230          3        263         25      31623          1        230       2832        92.000000       137.996017     13799.000000       100.000000       100.000000
     400        115          3        263         25      63013          1        115       2832        92.000000       137.996017     13799.000000       100.000000       100.000000
     900         57          3        263         25      63013          1         57       2832        91.200005       136.826065         0.000000       100.000000       100.000000

ticks (of 0.0073 s) the steps are apart from age 0, at most
    days    animals  collector     gauges  incubator  time sync     timers      units   vegetube
       0          0          0          0          0          0          0          0          0
       1       9063          0          1          1          6          0          3          6
       3       9065          1          2          2          8          1          4          7
       7      26209          4          4          5         11          4          7         10
      25      53796         17         18         18         24         17      37791         23
      60      58379         34         35         35         41         34      50377         40
     200      61726        137        128        138      61878        137      59726      61877
     400      62164        548        274       6825      62424        548      61164      62423
     900      61632        274        548       6825      62424        822      61137      62423

after
    days    animals  collector     gauges  incubator  time sync     timers      units   vegetube         elevator       heat grown    network ticks   oxygen drained  vegetube growth
       0      12602          3        263         25        460          1       4599        459        91.996063       137.996017     13799.000000       100.000000        41.195946
       1      12602          3        263         25        460          1       4599        459        91.996063       138.000005     13799.000000       100.000000        41.195946
       3      12602          3        263         25        460          1       4599        459        91.996063       138.000005     13799.000000       100.000000        41.195946
       7      12602          3        263         25        460          1       4599        459        91.996063       138.000005     13799.000000       100.000000        41.195946
      25      12602          3        263         25        460          1       4599        459        91.996063       138.000005     13799.000000       100.000000        41.195946
      60      12602          3        263         25        460          1       4599        459        91.996063       138.000005     13799.000000       100.000000        41.195946
     200      12602          3        263         25        460          1       4599        459        91.996063       138.000005     13799.000000       100.000000        41.195946
     400      12602          3        263         25        460          1       4599        459        91.996063       138.000005     13799.000000       100.000000        41.195946
     900      12602          3        263         25        460          1       4599        459        91.996063       138.000005     13799.000000       100.000000        41.195946

ticks (of 0.0073 s) the steps are apart from age 0, at most
    days    animals  collector     gauges  incubator  time sync     timers      units   vegetube
       0          0          0          0          0          0          0          0          0
       1          0          0          1          0          0          0          3          0
       3          0          0          1          0          0          0          3          0
       7          0          0          1          0          0          0          3          0
      25          0          0          1          0          0          0          3          0
      60          0          0          1          0          0          0          3          0
     200          0          0          1          0          0          0          3          0
     400          0          0          1          0          0          0          3          0
     900          0          0          1          0          0          0          3          0
```

Before, a day-old clock already ran the animals' 30 Hz frame at 35 Hz (the float clock steps by 8 ms and the
frame is due when it has stepped over 33 ms) and the vegetube's coroutine late; at 25 days the
units' totals changed 1840 times instead of 4599 (the steps were taken, in lumps, a quarter of a second apart:
the totals at the end are right, the clients saw them 4 times a second); at 200 days the clients got 31 623
time syncs in 460 s, the vegetube grew in 2 832 steps, and a machine's 150 s cycle was up to 137 ticks (1 s) late;
at 900 days the network tick had stopped (0 ticks in 460 s) and with it the drones. After, the steps
of every system are those of day 0; what differs is the units' total of day 0, the game's float, whose
sum drifts by 3e-5 of what it grew by in eight minutes (138.000005 against 137.996017 for 4599 steps of 0.03).

`logistics::tests::drones_fly_at_the_same_pace_at_any_age_of_the_server` does the same with the oracle's
drone errand through `Game::tick` at 0, 3, 60, 400, 900 and 5 000 days (the delivery tick, the transforms
sent and the frames run are those of day 0; the wire's tick wraps at 828 days), and
`meteo::tests::day_night_keeps_its_schedule_at_any_age_of_the_server` the day and night cycle's 0.5 s schedule
at 0 to 1 000 days.

### With a real save

`PCDS_BENCH_START_DAYS=N` begins the bench's clock at N days (`bench.rs`): the bench world, four scripted
players acting every 200 ms, 60 s of game time, the same randomness, at 0, 1, 3, 7, 25, 60, 200, 400 and 900
days. The saves written at the end are the same text but for two kinds of field: every world object,
inventory, growth, machine product and message is equal. What differs:

* the planet's unit levels: at age 0 the game's float sum is biased at a total of millions (a step of 1.75 on
  4.9 million, whose float step is 0.5, comes out as 1.5 or 2.0): of the 1 050 or so that Heat grows in the 60 s,
  the first hour's float sum ends 117 below the exact one; the aged runs (f64 sums) are equal to each other;
* the players' gauges by a 0.2 s step at most: the bench's tick is 5 ms and the drain is due at exact multiples
  of it, so a tie goes either way when the sums are f64 and not f32.

The handoff proof is `EQUAL` at the first hour (`bench::handoff`) and the bench fingerprints of the first
hour are those of the parent: 15c299165b6b3eb0 (walking) and eab3e8e67a9290d9 (acting every 200 ms; it was 4e119fd2aacba604 until the pick-up fix of the second soak round: a failed pick-up of an object the world had not got left it behind, and the ids that followed were one higher).

## What is not the same

* **The first hour of a world with a float total that has outgrown its step** (a total of 40 million with a
  step of 0.03 a tenth of a second): the game's float never moves; here it moves (in f64) from the first tick.
  That is a deviation from the game on purpose and only where the game stands still.
* **A restart is not a continuation, and the difference is the game's float.** The game's clock restarts at 0 and
  every timer with it; the units' totals come back as the save's floats; and the first hour is the game's float
  arithmetic again. At a total of millions that arithmetic does not add the nominal rate: a step of 1.75 on
  4.9 million (float step 0.5) is added as 2.0 every time. Measured with `bench::restart` (an idle bench world,
  Heat at 4.9 million, nominal 17.5 a second): the first game hour grows Heat by 71 958 (20.0 a second, +14%);
  a straight run's second hour, in exact f64 sums, by 63 000 (17.5); a run restarted at one hour grows 71 958
  in its next hour again. So a restart is 8 958 (0.18%) ahead of the run that went on at two hours, and 22
  ahead at half an hour (the sub-step phase). The audit's restart bench (0.06% apart at 12 h) is this, plus the random
  state and the partial cycle of every machine, which the save does not hold. The rate of the game itself
  is the +14% one for as long as a session lasts; here it is so for the first hour of a run and nominal
  after. If an operator would rather have the nominal rate from the first tick, `add_young` is the one place that
  says otherwise (the first hour's fingerprints would change with it).
* **A stall is not game time past one frame, as in the game.** Unity bounds a frame to
  `Time.maximumDeltaTime` (a third of a second): across a hitch the game's time goes on by that much and no more.
  So does this server's (`clock::MAX_FRAME`, applied in `Game::tick`): a bank stopped for ten seconds or ten minutes
  (SIGSTOP, a long save, a busy host) gets a third of a second of game time at its next tick, the rest stands like a
  pause (`Game::paused`). Nothing fast-forwards, no timer is due for it, and the gauges do not drain 40 times as
  fast while they catch up. Measured by `game::tests::a_stall_of_the_loop_is_not_game_time_past_one_frame` (the world
  after a gap of 0.34 s, 1.5 s, 10 s, 10 min or a day is the world after a frame of a third of a second: the same
  game time, network tick, oxygen and water); before, a gap of 10 s was 10 s of game time, 420 network ticks and 11
  points of oxygen gone in the two seconds after, and 10 min was 604 s. A loop gap of a second or more is logged.
  A hot flip's stand is not game time either: the new bank takes the clock as it stood when the snapshot was made
  (`started` is written as an age), and the old bank that is told to go on after a failed flip takes the time it
  stood off the clock (`Game::resume_after_stand`). The first hour's floats, the bench fingerprints and the
  handoff proof are untouched (the bench steps its clock by 5 ms; the limit is never reached). `world.pauseWhenEmpty`
  and the admin's pause are the world's rules for standing still on purpose.
* **Network ticks past 828 days** wrap on the wire (above). A client that has been connected through the wrap
  re-syncs its time once (0.2 s hard reset); transforms sent by the server with a tick that goes back by 2^32
  are what the game's own clients would receive if the game's tick wrapped. Not tested with a real client.
