# Audits

Four audits of release 0.6.1 were made in October 2026. This page folds their reports into one
list: what each found, what became of it, and what is still open at 0.7.0.

The audits were made on 0.6.1. Release 0.6.2 and the 0.7.0 work came after, and most findings were
fixed in them. Every status below was checked against the tree at v0.7.0: the fix and its test are
there, or the finding is marked open. A finding that could not be checked is marked `unverified`
instead of guessed.

| Audit | Looked at | Kind |
|---|---|---|
| Architecture | One source for each fact, modules and their dependencies, dead code, consistency | Read-only: the code was read and `cargo check` run, nothing was changed |
| Update | The update path: `pcds upgrade`, the admin API, the dashboard, the automatic update, the flip, rollback, the banks | Dynamic: real releases, real processes, a fake release feed, kills in every phase |
| Static | Clippy, unsafe code, dependencies, fuzzing, Miri, AddressSanitizer, ThreadSanitizer, concurrency, ignored errors | Tools run on the source, then each real hit fixed with a test |
| Dynamic | Soak, churn, chaos, disk full, limits, large worlds, long uptime, restart equivalence | Scratch servers with scripted players on a busy host |

Statuses:

* **fixed**: the fix is in the tree. The version says which release has it; the evidence column
  names the test (`module::tests::name`) or the file.
* **open**: not fixed. The last section lists them all with the smallest next step.
* **by design**: seen, understood and kept on purpose, with the reason in the docs.
* **not reproducible**: seen once or only under conditions that do not belong to the server.
* **unverified**: not rechecked at 0.7.0.

Version "0.7.0" means the work between 0.6.2 and 0.7.0. Release 0.6.2 itself holds only one fix
(U1).

## How to repeat each audit

The full commands are in [testing.md](testing.md). In short:

* **Architecture**: `scripts/check-layers` runs the layering test (`server/tests/layers.rs`) that
  keeps the modules in their layers (docs/architecture.md). The audit's graph and size scripts were
  throwaway; the layering test is what stays.
* **Update**: `cargo test --test hot_update` (real front and bank processes, kills in every flip
  phase, a fake release feed) and `make gate` (`tools/flip-gate.sh`: the last release hot-updates
  into this build). See docs/hot-update.md, "The flip gate".
* **Static**: `cargo clippy`, `cargo audit`, `cargo deny`, the fuzz targets in `server/fuzz/`, and the
  sanitizer and Miri runs described in [testing.md](testing.md).
* **Dynamic**: the scripts in `tools/chaos/` (soak, churn, chaos, disk full, limits, stress, fuzz
  datagrams), the measurements in `tools/perf/`, `tools/grow-world.py` for large worlds, and the
  ignored benches in `server/src/bench.rs` (`restart`, `soak_growth`).

## Architecture audit

The audit found the game logic well organised and the edges (settings, files, the front's size) not.
Its refactor list became the steps in docs/architecture.md, "The order of the work". The layering
test, which the audit asked for, now exists. Steps 4, 7, 8, 13 and 15 are not done.

| Id | Finding | Status | Evidence |
|---|---|---|---|
| A1 | Settings defined three or four times (the schema, the env-file text, the README, the main.rs header); `PCDS_RELEASES` missing from the schema; scattered env names | fixed in 0.7.0: an operator meets 12 variables, everything else is a rule | docs/env.md is generated from `config.rs`; `config::tests::write_env_doc` fails when it is out of date; a test holds the budget of 12 |
| A2 | The list of hidden secrets (`rules::HIDDEN`) was a second copy of the schema's secret marks, so a new secret could leak through `/api/rules` | fixed in 0.7.0: one list, taken from the schema | `rules::tests::every_secret_rule_is_hidden_from_the_endpoints_and_the_log`, `bank::admin::settings` test `the_rules_endpoints_never_show_hook_secrets` |
| A3 | Values read three ways: `PCDS_TRACE=` turned tracing on, `PCDS_TRUST_LOOPBACK` re-read per connection, the player limit read from the environment per join, process env and server.env in different order per command | fixed in 0.7.0: gameplay values are rules, the test hooks have one accessor `config::dev`. The order between process env and server.env was not rechecked | `Rules::max_players` in `rules.rs`; `PROCESS_ENV` and `DEV_HOOKS` in `config.rs` |
| A4 | Test hooks (`failForTest`, fail-on-purpose banks, flip verify) are honoured by the production binary | by design: they sit behind `PCDS_DEV`, are listed in docs/env.md, and an unknown hook stops the start. The end-to-end tests need the real binary. A cargo feature was not adopted | docs/env.md, "Developer hooks" |
| A5 | The CLI accepts the admin host `localhost` and refuses `[::1]`; the server's parser accepts only socket addresses | open | `cli/api.rs` (`Ctx::api`) against `config::check_env` |
| A6 | One fact in several places: the frame header size, the crash-loop numbers, the chat length 500, the flag bits, the Steam app id, `DISCONNECT_REASON` in main.rs, the `"Prime"` fallback planet, the default session 4242 in `Game::new` | open for the last three (still in `main.rs`, `scene.rs`, `players.rs`, `game.rs`); the frames and bank messages are typed now (A7). The others were not rechecked one by one | grep for `DISCONNECT_REASON`, `unwrap_or("Prime")` |
| A7 | The messages between front and banks were JSON built and read by string key: HELLO, READY, the snapshot head, bank.json, `RECOVER`; drifted key names and comments; an unreachable snapshot migration step | fixed in 0.7.0: typed messages with golden tests, one snapshot `format`, no migrate step | `msg::tests::every_message_is_the_json_it_always_was`, `msg::tests::the_notes_of_the_banks_folder_are_the_json_they_always_were` |
| A8 | The tables folder's contract (file names, build id, finding the folder) has no owner; the table reader imports the command line and the network encoders | open: step 7 of the order of work | `debt bank -> cli`, `native -> cli`, `native -> net` in `server/layers.txt` |
| A9 | File primitives live in `cli` and are used by the front and the updater; two atomic writers (`save::write_atomic`, `cli::replace_file`) and two lock helpers | open: step 13 | `debt front -> cli`, `updater -> cli` in `server/layers.txt` |
| A10 | Small wrong-way edges: the kick batch built through the bank (`reason_batch`), `enforce` called from the settings code, the front reading the settings by name | open: steps 4 and 8 | `debt front -> bank`, `bank -> front`, `config -> bank`, `front -> config`, `front -> rules` in `server/layers.txt` |
| A11 | `front.rs`, `bank.rs` and `chathooks.rs` are god modules (the front was 1757 lines of six concerns) | fixed in 0.7.0: each is a directory of small modules | docs/architecture.md, "The front" and "The bank, module by module" |
| A12 | The admin API had three routers, no route table, routes missing from the docs, and two routes (`/api/settings`, `/api/access`) nobody used | fixed in 0.7.0: one route table, a reference, a golden test; the two routes are gone. Typed request bodies and the `err()` helper for hand-built errors are not done | `routes.rs`, `front/admin_routes.rs` tests, docs/admin-api.md, `server/tests/admin_golden.rs` |
| A13 | The admin answers 500 after 10 s while a world stop can take 60 s; the web page's longer timeouts could never be reached | fixed in 0.7.0: world calls get 200 s on the server | `WORLD_ANSWER` in `admin.rs` |
| A14 | World files managed twice (`cli/worlds.rs` and `dashboard.rs`) with different names, filters and modes | fixed in 0.7.0 for names, paths and the listing (`worlds.rs`). Whether the two backup naming conventions were merged was not rechecked | `worlds::tests`, `server/tests/cli.rs`, docs/architecture.md step 9 |
| A15 | Game-version facts in source (group ids, loading RPCs, rocket classes, animal prefab hashes) failed at first use with a panic or a silent drop | fixed in 0.7.0: all are checked when the tables load, and a missing one stops the start with its name | `facts.rs`, `facts::tests::the_test_tables_have_every_fact` |
| A16 | Two game facts stay in source and are not read from the tables: the captured animator sync blob and the owner-writable variable list | open: the second is tested against the tables; the first is not | `ANIMATOR_SYNC` in `animals/spawn.rs`, `OWNER_WRITES` in `net.rs` |
| A17 | README and docs said things the code did not: the S3 rules, `PCDS_RELEASES`, spike sections of docs/hot-update.md, stale comments (`/api/manifest`, "extractor", HELLO and READY keys) | fixed in 0.7.0 for the README and the settings table; the comments are fixed in this branch. The spike sections of docs/hot-update.md stay on purpose as the design record, and the page says so | README "Off-site copies (S3)", docs/env.md, docs/hot-update.md header |
| A18 | Dead code and stale `allow`s: five stale allows, `sig_debug`, `refresh_generation_multiplier`, `PCDS_BANK`, the constant `"standalone"` status field | fixed in 0.7.0: all removed. The two `cfg_attr(not(test), allow(dead_code))` that remain mark fields only tests read | grep finds none of the removed names |
| A19 | Legacy paths kept for 0.5 and 0.6: the clean-up of 0.5 layouts, the 0.6 rules and snapshot compatibility, error shims, `reads` lists | fixed in 0.7.0: removed. Old saves are refused with a message, or imported by `pcds world import` | `oldsave.rs` tests, docs/hot-update.md |
| A20 | Silent fallbacks against the "no fallbacks" rule: a garbage `active` file reads as bank `a`, a missing planet reads as `Prime`, `world::hex` reads bad hex as 0 | open for these three; a missing or mistyped RPC argument reading as 0 is by design (the hostile-RPC test relies on it) | `updater::active`, `scene.rs`, `world.rs` |
| A21 | The S3 uploader runs in the game process with process-wide statics that are not in the snapshot, so a flip resets "last upload" | open: the owner's call whether S3 belongs in the front | `s3.rs` (`LAST`, `UPLOADED`, `BUSY`, `PENDING`) |
| A22 | The join rules live in four places (`Game::approve`, `enforce`, the front's trust check, `utp::is_loopback`) and `game` imports `utp` | open: step 12 of the audit's list, never scheduled | `game.rs` calls `crate::utp::is_loopback` |
| A23 | The flip and recovery logic is welded to processes and clocks and is tested only through real servers; `bank::run` and others call `process::exit` inline | open: step 15, the last and the owner's call | `tests/hot_update.rs` is the only coverage of the whole state machine |
| A24 | The table reader (`native/`) is tested by hand, so it is the riskiest code to touch | open: one ignored test plus a comparison script | `native::tables::tests::read_game_writes_the_test_tables`, `tools/dev/tables-check.sh` |
| A25 | Hygiene: duplicate type names (`Ctx`, `Bank`, `Access`), about 220 `pub` items used only in their module, `Result<_, String>` everywhere, raw `println!` logging | by design: the audit judged it not worth a project | none |
| A26 | The web page restates the server: the unit order and symbols (the chat limit and timeouts were also listed) | open for the unit order; the rest was not rechecked | `UNIT_ORDER` in `server/web/index.html` |
| A27 | The same small helper written several times: the clock in seconds, hex, file mtime, parent folder, hand-built `{"error":...}` strings | fixed in part in 0.7.0: `util.rs` holds the clock and the parent folder. `hex` still has three copies and the error strings were not counted again | `util.rs`; `fn hex` in `updater.rs`, `plugins/state.rs`, `world.rs` |

What the audit said to leave alone is still true: the settings schema and its tests, `coverage.rs`
and `coverage.txt`, the encapsulation of the subsystems, the low layers, `ipc.rs`, `utp.rs`,
`updater.rs`, `handoff.rs`, the version numbers with one owner each, and the I/O-free game logic.

## Update audit

The update path was driven with real releases from the real release feed, a scratch systemd unit,
bots playing, and a fake feed for the broken cases. It found one real bug that stopped every update
of a late-game world (U1) and several smaller ones. Finding numbers are those of the report.

| Id | Finding | Severity | Status | Evidence |
|---|---|---|---|---|
| U1 | A 0.6.0 snapshot did not restore in 0.6.1 (`missing field water_only`): a table value kept beside a machine's state was not rebuilt | High | fixed in 0.6.2 for `water_only`; in 0.7.0 for every table value a machine keeps (harvestables, vegetation, linked growers, auto crafters, disintegrators, optimizers) | `machines::tests::a_restore_sets_the_table_values_again_and_keeps_the_state`; the release gate (U16) |
| U2 | A hot rollback after an update is refused when the snapshot format changed; it is a restart (`pcds upgrade --rollback --restart`) | Medium | by design: the old release cannot read the new snapshot. The message, the CLI, the dashboard and the docs now say so | `updater::tests::a_rollback_across_a_snapshot_format_is_a_restart_and_says_which`, docs/hot-update.md |
| U3 | An update while the world is stopped answered "under way"; the dashboard went stale | Medium | fixed in 0.7.0: it answers "the world is stopped" and the Updates page follows | `front::updates::tests::an_update_of_a_stopped_world_says_it_is_stopped`, `tests/hot_update.rs` |
| U4 | A release was compared by the program's own version, not its tag; a mirror or mistaken release could flip at every check and destroy the rollback bank | Medium | fixed in 0.7.0: compared by tag | `updater::tests::a_release_is_compared_by_its_tag_when_the_bank_has_one`, `hot_update::the_automatic_update_takes_a_new_release_once_and_leaves_the_rolled_back_one` |
| U5 | A release that flipped fine and then kept crashing was restarted for ever; the fallback covered only "cannot load the world" | Medium | fixed in 0.7.0: after the crash limit the release is given up for the other bank, unless that is the bank the owner rolled back from | `hot_update::a_release_that_keeps_crashing_after_its_update_is_given_up`, `a_crashing_release_is_not_given_up_for_the_bank_the_owner_rolled_back_from`, `updater::tests::a_release_that_keeps_crashing_is_given_up_for_the_other_bank_when_there_is_one` |
| U6 | A failed or skipped automatic check waited a whole interval | Low | fixed in 0.7.0: retried within the hour | `front::updates::tests::a_failed_or_skipped_automatic_check_is_made_again_sooner` |
| U7 | The automatic-update settings were read only when the front started | Low | fixed in 0.7.0: `update.auto` and `update.checkHours` are rules, read at each check | `front/updates.rs` |
| U8 | A partial tarball was left beside the banks when the disk filled | Low | fixed in 0.7.0 | `updater::tests::a_broken_tarball_is_refused_and_leaves_the_banks_as_they_were` and the unwritable-folder test `a_banks_folder_that_cannot_be_written_fails_the_install_and_changes_nothing` |
| U9 | A `pcds` that cannot run was reported as "a release from before hot updates" | Low | fixed in 0.7.0 | `updater::tests` (the install tests; the report's own test used a program that cannot run) |
| U10 | A crash between the two renames in `updater::put` leaves the standby bank missing and a `.<bank>.old` folder behind; the active bank is never touched | Low | fixed after 0.7.0: `updater::recover` (the front's start, and every `put` before it moves anything) puts the old bank back when only `.<bank>.old` is left, and removes it when a whole new bank stands beside it | `updater::tests::a_crash_inside_an_install_is_made_whole_at_the_next_start` (crash injected after each rename) |
| U11 | `pcds upgrade --restart` set `active` to the new bank before reading its tables; if that failed, the next reboot would have left the server down | Medium | fixed in 0.7.0: `active` goes back to the bank that runs | `tables_or_undo` test in `cli/upgrade.rs` |
| U12 | Flips stood 35 to 1330 ms against 6 to 17 ms measured elsewhere, and two e2e tests failed once, while the host load was 33 to 114 | Info | not reproducible: seen only on a loaded host; rerun alone they pass | docs/hot-update.md, "Measured" |
| U13 | Alternating two releases shows systemd's "unit file changed on disk" because each release's `pcds run` rewrites the unit | Info | by design: cosmetic | `refresh_unit` in `cli/systemd.rs` |
| U14 | The generator machine mixes table values with state and is not refreshed after a restore | Info | by design: written down as the rule for the next machine | docs/hot-update.md, "State and the next release" |
| U15 | `lastSave` is bank-local, so a pending save failure shows again only at the next attempt | Info | by design: saves still fail honestly across a flip (tested) | `tests/hot_update.rs` |
| U16 | Nothing restored the release before's snapshot, which is how U1 got out | Systemic | fixed in 0.7.0 in a different form than the audit built: `make gate` by hand before a tag, and only between releases of one snapshot format. The committed fixtures and the CI job with a secret were dropped | `hot_update::a_hot_update_from_the_release_before_works` (ignored), `tools/flip-gate.sh`, docs/hot-update.md "The flip gate" and "Release checklist" |

Kill tests were added for every flip phase (the front, the old bank, the new bank), for requests and
players during a flip, for a stopped, paused and failing world, for a second update, for a tables
format that differs, and for broken downloads (cut, damaged, bad hash, no hash, HTML). All pass in
`tests/hot_update.rs`.

Not covered by this audit: Steam connections through a flip, a 24-hour wait for the automatic
check, GitHub rate-limit answers beyond the status-code unit test, and the successful live flip
clicked through the dashboard (the button posts what the API run posted).

## Static audit

Tools were run on the whole source; each real hit was fixed with a test. The release profile keeps
`overflow-checks` off and `panic = "unwind"` (the panic guards need unwind); this is by design (S18).

| Id | Finding | Status | Evidence |
|---|---|---|---|
| S1 | `cargo clippy` did not compile (`approx_constant`, a deny lint); 3 real findings and 70 style lints | fixed in 0.7.0: real ones fixed, style lints allowed crate-wide with a reason each | the allow list in `main.rs` |
| S2 | A movement state merged onto a huge quaternion overflowed to NaN; no snapshot could be written and the save held "NaN" | fixed in 0.7.0 | `transform::tests::a_finite_state_that_overflows_when_merged_is_refused` |
| S3 | Repeating f32 timers stopped after weeks: the world units hung the bank at 24 days, the gauges drained 40 times too fast from 48 days | fixed in 0.7.0, first by f64 due times and then by one f64 game clock (see D1, D2, D11 to D14) | `units::tests::units_grow_at_their_rate_whatever_the_uptime`, `gauges::tests::gauges_drain_at_their_pace_after_weeks_of_uptime`, docs/clock.md |
| S4 | A client's float of 1e30 (`LoadInstanceFromScene`, owner-written variables) made NaN world objects and blocked hot updates | fixed in 0.7.0: floats past 1e9 are refused where they are decoded | `rpcspec::tests::floats_past_the_sane_range_are_not_decoded`, `net::tests::owner_written_floats_must_be_numbers_in_range` |
| S5 | A `rules.json` number past f32 passed validation and panicked the chat hooks | fixed in 0.7.0 | `rules::tests::a_rules_file_number_past_f32_is_refused` |
| S6 | The inventory sort's comparer was no total order, so Rust's sort panicked from 21 items | fixed in 0.7.0: the server now sorts as the game does (its introsort port) | `inventories::tests::items_with_and_without_traits_sort_as_the_games_sort_does`, `sorting_an_inventory_of_many_equal_items_is_the_games_introsort` |
| S7 | `SetInventorySize` with an offset of `i32::MAX` wrapped to a negative size and dropped the inventory | fixed in 0.7.0: the size is clamped (the audit had left it) | `inventories::tests::a_size_change_stays_within_the_bounds_of_an_inventory` |
| S8 | A snapshot with a clock of 1e20 seconds panicked in `Duration::from_secs_f64` | fixed in 0.7.0 | `handoff::tests::a_corrupt_clock_is_an_error_not_a_panic` |
| S9 | A snapshot's slot map with a corrupt free list was accepted and would panic or overwrite later | fixed in 0.7.0 | `idmap::tests::slots_round_trip_and_a_corrupt_free_list_is_refused` |
| S10 | The control buffer for passing file descriptors was a byte vector, which has no alignment promise | fixed in 0.7.0: held in `u64`s | `ipc::tests::frames_and_memfds_cross_the_socket` |
| S11 | Between the front and its own bank: the result of sealing a memfd is ignored, and `MSG_CTRUNC` is not checked, so a peer could make the queue of descriptors grow | open: the peer is the server's own bank | `ipc.rs` (`F_ADD_SEALS`, `recv_with_fds`) |
| S12 | The S3 uploader stayed busy for ever after a panic, and a failed thread start panicked the game loop | fixed in 0.7.0. A backup made in the microseconds before the uploader finishes waits for the next one, harmless | `s3::tests::a_panicking_upload_does_not_keep_the_next_ones_out` |
| S13 | The admin acceptor spun at 100% CPU on accept errors, ended if a thread could not start, and `/api/logs` could wait for ever on `journalctl` | fixed in 0.7.0: it backs off, survives, and `journalctl` has a 6 s limit | `admin::tests::an_acceptor_that_keeps_failing_backs_off_and_recovers`, `LOGS_WAIT` in `admin.rs` |
| S14 | If the chat hooks' delivery thread ever panics, the hooks stay off with no message | unverified: no known trigger since S5 | `front/chat_hooks.rs` |
| S15 | Two sets grow by one entry per hostile connection id over the server's life (`game.unhandled`, `Limits.warned`) | unverified: the memory clients can make the server hold is bounded in bytes since 0.7.0, these two sets were not checked | `game.rs` (`unhandled`) |
| S16 | The Steam path had none of the caps the direct-IP path has | fixed in 0.7.0 | `front::transport::steam::tests::steam_connections_have_the_udp_caps` |
| S17 | After a panic in one client's RPC the bank saved the half-changed world | fixed in 0.7.0: the last whole save is kept | `bank::panic::tests::a_panicking_client_is_kicked_and_the_save_on_disk_is_kept_not_replaced_by_its_half_done_work` |
| S18 | Release profile: no overflow checks, `panic = "unwind"` | by design: the parity ports rely on the game's unchecked arithmetic; the overflow-checked fuzz build found one wrap (S7) | `Cargo.toml` has no `[profile]` |
| S19 | `SaveFile::from_text` dropped any entry it could not parse and read a missing section as empty, so a damaged save loaded as a smaller world | fixed in 0.7.0 for damaged entries and unreadable saves, which are refused. A missing section reading as empty was not rechecked | `a_save_with_a_damaged_entry_is_refused`, `truncated_and_garbage_files_are_refused` |
| S20 | A new bank was renamed into place without flushing its files, so a power cut could leave empty files | fixed in 0.7.0 | `updater::tests::a_bank_is_flushed_to_disk_before_it_is_moved_into_place` |
| S21 | `cargo audit`: one warning, `paste` 1.0.15 unmaintained (a compile-time macro pulled by `steamworks`); no vulnerability; licences all compatible | open: nothing to do here until `steamworks` moves. Not rerun for the crates added since (the plugin host, `toml`, `unicode-normalization`) | `server/Cargo.lock` |
| S22 | The updater's checksum comes from the same server as the tarball, so it guards against damage, not a compromised release or mirror | by design: a limit of the design (the unpacked program runs as the server user), written down in docs/hot-update.md ("the sha256 guards against a damaged download, not a forged release: releases are not signed") | `updater.rs` (`unpack`) |
| S23 | A hard link inside a release tarball passes `plain_files` (it looks like a file) | fixed after 0.7.0: `plain_files` refuses any file with more than one name (tar extracts a hard link as a second name of the same file), as it refuses links and devices | `updater::tests::a_hard_link_in_a_tarball_is_refused` |
| S24 | `world::hex` slices a string by bytes and panics on non-ASCII input (offline probe and load tools only) | fixed (consolidation after 0.7.0: no panic; bad hex still reads as 0, as the tools expect) | `world::tests::hex_reads_pairs_and_does_not_panic_on_text_that_is_not_ascii` |
| S25 | Small suspects never reproduced: `serde_json` float parsing may move a huge value by one bit per save, `set_var` in a chat-hook test races other tests, `request.accept()` results ignored in the front | unverified | none |

The fuzz harness (`server/fuzz/`) has 15 targets: access, chathooks, config, front, game, handoff,
ipc, rpcspec, rules, save, savesweep, transform, updater, utp, wire. The 41 seed files hold no game
data. A corpus replay under Miri is available (`--cfg pcds_corpus`). Fuzzing and reading found the
defects S2 to S6, S8 and S9; the last runs found nothing more.

Not covered: the live Steam path (only its pure helpers were fuzzed), the live UDP loop under a
fuzzer, `cli/` and `native/` (offline tools), and `hot_update` under ThreadSanitizer.

## Dynamic audit

The scripts ran against scratch servers on a shared, busy host (load average 50 to 90), so every
stall is real but inflated. Finding numbers are those of the report.

| Id | Finding | Severity | Status | Evidence |
|---|---|---|---|---|
| D1 | After 24.3 days of game time the bank hangs in `units::tick` (an f32 step stops adding); nothing detects it | High | fixed in 0.7.0: due times in f64 once the clock is old, then one f64 game clock | `units::tests::units_grow_at_their_rate_whatever_the_uptime`, `timetravel` test `every_system_takes_its_steps_at_the_same_pace_at_any_age_of_the_server`, docs/clock.md |
| D2 | The gauges' 0.2 s step runs slow from 12 days and 40 times too fast from 48 days | High | fixed in 0.7.0 | `gauges::tests::gauges_drain_at_their_pace_after_weeks_of_uptime` |
| D3 | The save cut default fields in place per cut, which is quadratic: 16,000 objects took 10 s per autosave in the game loop | Medium | fixed in 0.7.0: one pass | `save::tests::a_big_save_is_written_in_time` (50,000 objects), `the_defaults_are_cut_as_they_were_cut_one_by_one` |
| D4 | The autosave's two atomic writes (four fsyncs) ran in the game loop: loops of 5 to 25 s on a loaded disk | Medium | fixed in 0.7.0: written on a thread; any later save waits for it | `bank::persist::tests::an_autosave_is_written_by_a_thread_and_never_overtakes_a_later_save`, `the_autosave_does_not_wait_for_a_slow_disk_and_a_still_world_writes_nothing` |
| D5 | The UTP sender's window counted packets, not sequence numbers; after 64 packets between two acks a session was dropped (16 busy bots lost 2 to 4 in 200 s) | Medium | fixed in 0.7.0. Real Unity clients were not tried | `utp::tests::the_sender_never_runs_a_window_past_the_oldest_unacknowledged_packet` |
| D6 | No watchdog on a bank: a hung or stopped bank is never replaced | Medium, design | fixed in 0.7.0: rule `limits.bankWatchdogSeconds` (default 60), not during a flip, a stop or a save | `hot_update::a_hung_bank_is_replaced_but_a_slow_paused_or_stopped_one_is_not`, README rules table |
| D7 | Large worlds cross thresholds: 90,000 objects spike the idle loop to 100 ms; a flip stands over 100 ms from 11,000 network objects; a join takes seconds at 200,000. At rest, half the cost was the 10 Hz scan of every object for explosives | Medium, design | open in part: the snapshot and join work was made faster in 0.7.0 (snapshot in parts on several threads, writing straight to the save); the 10 Hz full scan is still in place, marked in the code | `meteo/explode.rs` (`ponytail:` comment), `tools/perf/`, docs/hot-update.md "Flip speed" |
| D8 | After a bank stall the front held up to 16 MiB per client and never gave it back (9 to 226 MB) | Low | fixed in 0.7.0: bounded in all, in bytes, and returned | `what_all_clients_have_waiting_for_the_bank_is_capped_by_dropping_the_slowest`, `the_connections_together_may_not_queue_without_end`, docs/hot-update.md |
| D9 | Saved players are never pruned: each new name adds about 1.2 KB to the save for ever | Low | open: a retention rule is a product decision | no pruning code in `players.rs` or `save.rs` |
| D10 | A client that vanishes holds its slot and name for the 30 s UDP timeout; this filled the churn runs with "session full" | Low | by design: the slot is held to the UDP timeout so a client can come back | README, "Connection slots can be flooded" |
| D11 | Unit totals were f32 and stop growing above about rate x 1.7e6 | Suspect | fixed in 0.7.0: totals are exact f64 after the first hour, the f32 the game shows is derived | `clock::tests::a_total_of_millions_keeps_growing_after_the_first_hour` |
| D12 | The network tick counter saturates at 828 days and the logistics frame loop stops | Suspect | fixed in 0.7.0: 64-bit inside, wraps on the wire like the game's int | docs/clock.md, "Design" (the tick counter) |
| D13 | Machine, rocket and meteo timers degrade with the f32 clock's resolution (2.5% to 10% slow at 48 to 194 days) | Suspect | fixed in 0.7.0 by the f64 clock | `clock::tests::an_old_clock_keeps_its_steps`, docs/clock.md |
| D14 | A jump of game time was paid by catch-up loops: 3 days cost 14 CPU minutes | Suspect | fixed in 0.7.0: `take_due` counts instead of looping | `clock::tests::take_due_is_the_loop_it_replaces`, `units::tests::a_jump_of_the_clock_is_one_catch_up` |
| D15 | If the wall clock steps back (NTP), backups stamped in the future block new backups until it catches up | Low | fixed after 0.7.0: a stamp from the future does not count as a recent backup, is ordered before all others (pruned first, never "the newest" for S3), and the copy just made is never the one removed | `save::tests::backups_stamped_in_the_future_block_nothing_and_go_first`, `save::backup_stamps` |
| D16 | A restarted world's heat differs from a straight run by 0.0004% to 0.06% | Low | by design: a restart is a new game on the same save and the game's own float behaves the same | docs/clock.md, "What is not the same"; `bench::restart` |

Experiments that found nothing (kept for the record, no finding):

* **Soak**, 2 h 05 min, 8 bots: memory, threads, descriptors and save size flat; all rotating
  backups load.
* **Churn**, 6,300 sessions of joins, vanishes, handshakes only: no leak in the front.
* **Chaos**, 752 actions in 7 runs (kill -9 and SIGSTOP of bank and front, flips with a kill mid-flip,
  config and rules, S3 faults, read-only folder): no failed invariant.
* **Disk full**: no corrupt or partial world; the snapshot is kept and restored.
* **Limits**: 4 handshakes per 10 s per address, 8 sessions per address and 128 in all hold and
  recover; 3 million garbage datagrams: no panic, no growth.
* **Stress**: 16,000 RPC/s and 40 players on 16 places: loops under 10 ms. A drone farm of 200
  drones for one game hour: flat memory, worst tick 15 ms.
* **Time**: UTC stamps for 400 years, the bank stopped for 30, 120 and 600 s: game time catches up.

Not covered: the Steam transport and invite codes, a real Unity client for D5, the hourly update
check against a real service, and S3 against a real bucket (a fake endpoint answered).

## Open

Everything below is still open at 0.7.0. Each line gives the reason and the smallest next step.
Items already fixed in this branch (the plugin examples build across checkout paths, `docker/test.sh`
on rootless Docker, stale module names in comments) are not repeated.

Architecture:

* **A5** The CLI and the server parse the admin address differently. Next: make `cli/api.rs` use the
  parser of `config::check_env`.
* **A6** `DISCONNECT_REASON`, the `"Prime"` fallback planet and the default session 4242 stay in
  source. Next: move the id to `protocol.rs` and make the planet and session required values.
* **A8** The tables folder has no owner module. Next: step 7, a `tables` module; `native` stops
  importing `cli` and `net`.
* **A9** File primitives are in `cli`. Next: step 13, one module for atomic write and lock.
* **A10** Wrong-way edges between the front, the bank and the settings. Next: step 4, then step 8.
* **A12** Typed admin requests, the `err()` helper and one timeout table are not done. Next: type
  the request bodies of kick, ban, teleport.
* **A16** The animator sync blob and the owner-write list are game facts in source. Next: have the
  reader emit them into the tables (bumps the tables format).
* **A20** Three silent fallbacks (`updater::active`, `"Prime"`, `world::hex`). Next: make each an
  error.
* **A21** S3 uploads run in the game process with statics outside the snapshot. Next: owner decides
  whether to move them to the front.
* **A22** Join rules in four places. Next: one pure admission function with tests.
* **A23** The flip and recovery logic has no model tests and `bank::run` exits inline. Next: extend
  `tests/hot_update.rs` first, then step 15.
* **A24** The table reader has one ignored test. Next: run `tools/dev/tables-check.sh` on every game
  update and before touching `native/`.
* **A26, A27** The web page restates the unit order; `hex` has three copies. Next: serve the unit
  order from `/api/status`; use one `hex`.

Static:

* **S11** The memfd seal result and `MSG_CTRUNC` are unchecked (own bank only). Next: check both
  and fail the frame.
* **S21** `paste` is unmaintained (build time, via `steamworks`). Next: rerun `cargo audit` and
  `cargo deny` for the crates added since 0.6.1.

Dynamic:

* **D7** The 10 Hz scan of every world object for explosives remains, and large worlds still
  produce long flips and joins. Next: drive `explode::scan` from changes, not from a timer, and
  re-measure with `tools/perf/` at 90,000 objects.
* **D9** Saved players are never pruned. Next: owner decides a retention rule.

Lead from the tools' smoke runs, not a finding: in two of three disk-full runs against 0.6.2,
`GET /api/status` was refused for 8 s after a `kill -9` of the bank on a full disk although the
front's log showed the bank restarting. Not reproduced on 0.7.0; run `tools/chaos/diskfull.sh` on a
0.7.0 build to see.

Unverified (could not be checked, listed so they are not lost): **S14** (chat hooks stay off after a
delivery panic), **S15** (two sets growing per hostile connection id), **S25** (small suspects), and
the parts of A3, A14, A19, A26 and A27 named in their rows.
