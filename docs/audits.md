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
| A5 | The CLI accepts the admin host `localhost` and refuses `[::1]`; the server's parser accepts only socket addresses | fixed after 0.7.0: `config::admin_loopback` reads PCDS_ADMIN as `check_env` does (a socket address, on the loopback of its family when it listens on every address); `localhost` is refused by both | `config::tests::the_command_line_reads_the_admin_address_as_the_server_does` |
| A6 | One fact in several places: the frame header size, the crash-loop numbers, the chat length 500, the flag bits, the Steam app id, `DISCONNECT_REASON` in main.rs (now `protocol.rs`), the `"Prime"` fallback planet, the default session 4242 in `Game::new` | fixed after 0.7.0 for the last three: `DISCONNECT_REASON` is in `protocol.rs`; the `"Prime"` fallback is gone (a save with no known planet is refused at start: `game::world_planet`); the default session is read from the one schema entry (`PCDS_SESSION` in `config::KNOBS`). The others were not rechecked one by one | `game::tests::a_save_without_a_known_planet_is_refused`, `protocol::tests::the_disconnect_reason_is_one_message_of_type_8_with_the_text_in_utf16` |
| A7 | The messages between front and banks were JSON built and read by string key: HELLO, READY, the snapshot head, bank.json, `RECOVER`; drifted key names and comments; an unreachable snapshot migration step | fixed in 0.7.0: typed messages with golden tests, one snapshot `format`, no migrate step | `msg::tests::every_message_is_the_json_it_always_was`, `msg::tests::the_notes_of_the_banks_folder_are_the_json_they_always_were` |
| A8 | The tables folder's contract (file names, build id, finding the folder) has no owner; the table reader imports the command line and the network encoders | fixed after 0.7.0: `tables.rs` owns `build_id` and `find_tables`, and the fixed-string encoder is in `wire.rs`; the three debts are gone from `server/layers.txt` | `tables::tests::the_build_id_is_a_hash_of_the_game_code`, `cli::env::tests` (the tables lookup), `tests/layers.rs` |
| A9 | File primitives live in `cli` and are used by the front and the updater; two atomic writers (`save::write_atomic`, `cli::replace_file`) and two lock helpers | partly fixed after 0.7.0: `hold` and `replace_file` are in `util.rs` and the server.env text functions in `config.rs`, so the debts `front -> cli` and `updater -> cli` are gone from `server/layers.txt`. Still two atomic writers (they differ: unique temp names against concurrent writers, and a mode kept; against a fixed name and a mode given) and two lock helpers (`util::hold`, `updater::lock`) | `util::tests::a_file_is_replaced_whole_with_its_mode`, `util::tests::a_lock_is_held_by_one_holder_at_a_time`, `tests/layers.rs` |
| A10 | Small wrong-way edges: the kick batch built through the bank (`reason_batch`), `enforce` called from the settings code, the front reading the settings by name | fixed after 0.7.0 for the kick batch (now `protocol::reason_batch`), `enforce` (now `access::enforce`) and the snapshot's path (now `worlds::snapshot_path`): the three debts are gone from `server/layers.txt`. Open: the front reading the settings by name (step 8) | `debt front -> config`, `front -> rules` in `server/layers.txt`; `tests/layers.rs` |
| A11 | `front.rs`, `bank.rs` and `chathooks.rs` are god modules (the front was 1757 lines of six concerns) | fixed in 0.7.0: each is a directory of small modules | docs/architecture.md, "The front" and "The bank, module by module" |
| A12 | The admin API had three routers, no route table, routes missing from the docs, and two routes (`/api/settings`, `/api/access`) nobody used | fixed in 0.7.0: one route table, a reference, a golden test; the two routes are gone. Typed request bodies and the `err()` helper for hand-built errors are not done | `routes.rs`, `front/admin_routes.rs` tests, docs/admin-api.md, `server/tests/admin_golden.rs` |
| A13 | The admin answers 500 after 10 s while a world stop can take 60 s; the web page's longer timeouts could never be reached | fixed in 0.7.0: world calls get 200 s on the server | `WORLD_ANSWER` in `admin.rs` |
| A14 | World files managed twice (`cli/worlds.rs` and `dashboard.rs`) with different names, filters and modes | fixed in 0.7.0 for names, paths and the listing (`worlds.rs`). Whether the two backup naming conventions were merged was not rechecked | `worlds::tests`, `server/tests/cli.rs`, docs/architecture.md step 9 |
| A15 | Game-version facts in source (group ids, loading RPCs, rocket classes, animal prefab hashes) failed at first use with a panic or a silent drop | fixed in 0.7.0: all are checked when the tables load, and a missing one stops the start with its name | `facts.rs`, `facts::tests::the_test_tables_have_every_fact` |
| A16 | Two game facts stay in source and are not read from the tables: the captured animator sync blob and the owner-writable variable list | open: the second is tested against the tables; the first is not | `ANIMATOR_SYNC` in `animals/spawn.rs`, `OWNER_WRITES` in `net.rs` |
| A17 | README and docs said things the code did not: the S3 rules, `PCDS_RELEASES`, spike sections of docs/hot-update.md, stale comments (`/api/manifest`, "extractor", HELLO and READY keys) | fixed in 0.7.0 for the README and the settings table; the comments are fixed in this branch. The spike sections of docs/hot-update.md stay on purpose as the design record, and the page says so | README "Off-site copies (S3)", docs/env.md, docs/hot-update.md header |
| A18 | Dead code and stale `allow`s: five stale allows, `sig_debug`, `refresh_generation_multiplier`, `PCDS_BANK`, the constant `"standalone"` status field | fixed in 0.7.0: all removed. The two `cfg_attr(not(test), allow(dead_code))` that remain mark fields only tests read | grep finds none of the removed names |
| A19 | Legacy paths kept for 0.5 and 0.6: the clean-up of 0.5 layouts, the 0.6 rules and snapshot compatibility, error shims, `reads` lists | fixed in 0.7.0: removed. Old saves are refused with a message, or imported by `pcds world import` | `oldsave.rs` tests, docs/hot-update.md |
| A20 | Silent fallbacks against the "no fallbacks" rule: a garbage `active` file reads as bank `a`, a missing planet reads as `Prime`, `world::hex` reads bad hex as 0 | fixed after 0.7.0: `updater::active` is an error that says what to do (no file at all is a fresh installation: bank a); a save with no known planet is refused at start; bad hex is an error (`util::from_hex`) or a panic naming the text (`world::hex`). A missing or mistyped RPC argument reading as 0 stays by design (the hostile-RPC test relies on it) | `updater::tests::an_active_file_that_says_neither_bank_is_an_error_and_not_bank_a`, `game::tests::a_save_without_a_known_planet_is_refused`, `util::tests::the_helpers_do_what_their_names_say` |
| A21 | The S3 uploader runs in the game process with process-wide statics that are not in the snapshot, so a flip resets "last upload" | open: the owner's call whether S3 belongs in the front | `s3.rs` (`LAST`, `UPLOADED`, `BUSY`, `PENDING`) |
| A22 | The join rules live in four places (`Game::approve`, `enforce`, the front's trust check, `utp::is_loopback`) and `game` imports `utp` | open: step 12 of the audit's list, never scheduled | `game.rs` calls `crate::utp::is_loopback` |
| A23 | The flip and recovery logic is welded to processes and clocks and is tested only through real servers; `bank::run` and others call `process::exit` inline | open: step 15, the last and the owner's call | `tests/hot_update.rs` is the only coverage of the whole state machine |
| A24 | The table reader (`native/`) is tested by hand, so it is the riskiest code to touch | open: one ignored test plus a comparison script | `native::tables::tests::read_game_writes_the_test_tables`, `tools/dev/tables-check.sh` |
| A25 | Hygiene: duplicate type names (`Ctx`, `Bank`, `Access`), about 220 `pub` items used only in their module, `Result<_, String>` everywhere, raw `println!` logging | by design: the audit judged it not worth a project | none |
| A26 | The web page restates the server: the unit order and symbols (the chat limit and timeouts were also listed) | open for the unit order; the rest was not rechecked | `UNIT_ORDER` in `server/web/index.html` |
| A27 | The same small helper written several times: the clock in seconds, hex, file mtime, parent folder, hand-built `{"error":...}` strings | fixed in part: `util.rs` holds the clock, the parent folder and, after 0.7.0, hex once (`to_hex`, and the strict `from_hex`; the copies in `updater.rs`, `plugins/state.rs` and the test helper in `wire.rs` are gone). The error strings were not counted again | `util::tests::the_helpers_do_what_their_names_say` |

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
| S11 | Between the front and its own bank: the result of sealing a memfd is ignored, and `MSG_CTRUNC` is not checked, so a peer could make the queue of descriptors grow | fixed after 0.7.0: a seal the kernel refuses is an error (`memfd: could not be sealed`); a message with more than 4 descriptors, or more than 16 waiting for frames that take none, breaks the link with the reason in `Link::broken` | `ipc::tests::a_failed_seal_and_a_flood_of_descriptors_break_the_link_loudly` |
| S12 | The S3 uploader stayed busy for ever after a panic, and a failed thread start panicked the game loop | fixed in 0.7.0. A backup made in the microseconds before the uploader finishes waits for the next one, harmless | `s3::tests::a_panicking_upload_does_not_keep_the_next_ones_out` |
| S13 | The admin acceptor spun at 100% CPU on accept errors, ended if a thread could not start, and `/api/logs` could wait for ever on `journalctl` | fixed in 0.7.0: it backs off, survives, and `journalctl` has a 6 s limit | `admin::tests::an_acceptor_that_keeps_failing_backs_off_and_recovers`, `LOGS_WAIT` in `admin.rs` |
| S14 | If the chat hooks' delivery thread ever panics, the hooks stay off with no message | unverified: no known trigger since S5 | `front/chat_hooks.rs` |
| S15 | Two sets grow by one entry per hostile connection id over the server's life (`game.unhandled`, `Limits.warned`) | checked in the memory work: `unhandled` and `misrouted` hold names the RPC tables list (a few for each RPC: about 600 in all, whoever connects); `Limits.warned` goes with its client (`Limits::forget`); the third such set, `refused_inventories`, grew by one id per connection and is dropped when the client leaves | `game::tests::a_client_that_leaves_is_forgotten_by_the_set_of_refused_inventories` |
| S16 | The Steam path had none of the caps the direct-IP path has | fixed in 0.7.0 | `front::transport::steam::tests::steam_connections_have_the_udp_caps` |
| S17 | After a panic in one client's RPC the bank saved the half-changed world | fixed in 0.7.0: the last whole save is kept | `bank::panic::tests::a_panicking_client_is_kicked_and_the_save_on_disk_is_kept_not_replaced_by_its_half_done_work` |
| S18 | Release profile: no overflow checks, `panic = "unwind"` | by design: the parity ports rely on the game's unchecked arithmetic; the overflow-checked fuzz build found one wrap (S7) | `Cargo.toml` has no `[profile]` |
| S19 | `SaveFile::from_text` dropped any entry it could not parse and read a missing section as empty, so a damaged save loaded as a smaller world | fixed in 0.7.0 for damaged entries and unreadable saves, which are refused. A missing section reading as empty was not rechecked | `a_save_with_a_damaged_entry_is_refused`, `truncated_and_garbage_files_are_refused` |
| S20 | A new bank was renamed into place without flushing its files, so a power cut could leave empty files | fixed in 0.7.0 | `updater::tests::a_bank_is_flushed_to_disk_before_it_is_moved_into_place` |
| S21 | `cargo audit`: one warning, `paste` 1.0.15 unmaintained (a compile-time macro pulled by `steamworks`); no vulnerability; licences all compatible | open: nothing to do here until `steamworks` moves. Rerun in the security review for the crates added since (the plugin host, `toml`, `unicode-normalization`): the same one warning, no advisory, all licences compatible (see the review's supply-chain verdict) | `server/Cargo.lock` |
| S22 | The updater's checksum comes from the same server as the tarball, so it guards against damage, not a compromised release or mirror | by design: a limit of the design (the unpacked program runs as the server user), written down in docs/hot-update.md ("the sha256 guards against a damaged download, not a forged release: releases are not signed") | `updater.rs` (`unpack`) |
| S23 | A hard link inside a release tarball passes `plain_files` (it looks like a file) | fixed after 0.7.0: `plain_files` refuses any file with more than one name (tar extracts a hard link as a second name of the same file), as it refuses links and devices | `updater::tests::a_hard_link_in_a_tarball_is_refused` |
| S23 | A hard link inside a release tarball passes `plain_files` (it looks like a file) | fixed after 0.7.0 (security review, H3): a file with a link count above one is refused | `updater::tests::releases_come_over_https_with_plain_tags_and_files` |
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
| D7 | Large worlds cross thresholds: 90,000 objects spike the idle loop to 100 ms; a flip stands over 100 ms from 11,000 network objects; a join takes seconds at 200,000. At rest, half the cost was the 10 Hz scan of every object for explosives | Medium, design | open in part: the snapshot and join work was made faster in 0.7.0 (snapshot in parts on several threads, writing straight to the save). The 10 Hz scan for explosives is fixed after 0.7.0: it runs only while a world object of an explosive group has a network object (a count kept at every place one gains, loses or is replaced; none in most worlds); on the 89,285-object grown bench world a tick went from 68.0 to 11.0 microseconds and the meteo part from 48.9 to 0.05, with the fingerprint unchanged. While a bomb lies about the scan is still over the whole world. Long flips and joins at 90,000 and more objects remain | `meteo::explode::tests::the_count_of_live_explosives_follows_the_world`, `bombs_armed_in_one_scan_go_off_in_slot_order`, `tools/perf/` |
| D8 | After a bank stall the front held up to 16 MiB per client and never gave it back (9 to 226 MB) | Low | fixed in 0.7.0: bounded in all, in bytes, and returned | `what_all_clients_have_waiting_for_the_bank_is_capped_by_dropping_the_slowest`, `the_connections_together_may_not_queue_without_end`, docs/hot-update.md |
| D9 | Saved players are never pruned: each new name adds about 1.2 KB to the save for ever | Low | fixed after 0.7.0 by the opt-in rule `players.forgetAfterDays` (0: keep for ever): a player away that long who never crafted or earned a token, holds no more than the joining kit and is not the host or in the game is forgotten with those items; everyone else is kept | `players::tests::players_away_with_nothing_past_the_kit_are_forgotten_and_no_one_else`, `players::forget_unseen` |
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
* **Limits**: 6 handshakes per 10 s per address (4 before 0.8.1), 8 sessions per address and 128 in all hold and
  recover; 3 million garbage datagrams: no panic, no growth.
* **Stress**: 16,000 RPC/s and 40 players on 16 places: loops under 10 ms. A drone farm of 200
  drones for one game hour: flat memory, worst tick 15 ms.
* **Time**: UTC stamps for 400 years, the bank stopped for 30, 120 and 600 s: game time caught up (changed after 0.7.0: a stall is a third of a second of game time at most, as in the game; see clock.md).

Not covered: the Steam transport and invite codes, a real Unity client for D5, the hourly update
check against a real service, and S3 against a real bucket (a fake endpoint answered).

## Security review v0.7+

An independent review of the code that is new since v0.6.2 (`git diff --stat bb6ae80..main`: 353
files, 61,000 lines), made in October 2026 on `main` at a2a5560 by an adversary's reading: read the
code, attack it, prove each problem with a failing test or a scripted run against a scratch server,
fix it in its own commit with the regression test. The threat actors were: (a) an anonymous game
client, (b) a holder of a scoped token, (c) a malicious plugin author (a module approved by an
admin with some capabilities), (d) a malicious release feed or tarball, (e) a malicious world file,
(f) a local user of the same host, (g) a network attacker against the admin port. Ids: P plugins,
H admin API, updater, install and CI, G game protocol, W worlds and files.

Statuses as above. "Commit" is the fix in this branch (`git log a2a5560..`); every fix has its test
in the same commit.

### Findings

| Id | Finding | Sev. | Status | Evidence |
|---|---|---|---|---|
| P1 | The host functions a module imports did work by the byte for free: `log` (no capability needed) converted a 1 MiB range of the module's memory to a string at every call, `say`, `rules_patch` (JSON parse) and `http_request` (a copy, before its capability was looked at) the same, `query` rebuilt the player list and the rules at every ask; a call costs the module about 8 fuel. One chat event of a plugin that asked for `event.chat` alone held the game thread **44 s** (the watchdog then kills the bank, and the next chat message does it again) | High | fixed (b308058): each host call costs 200 fuel and the bytes it copies (1) or parses (16); a text is read to 4096 bytes, a rules patch is refused over 64 KiB, a query answer is built once per call | `plugins::tests::a_host_call_with_a_megabyte_buffer_is_paid_for_in_fuel`, `asking_the_world_again_and_again_is_paid_for_in_fuel` |
| P2 | wasmi charges `memory.fill`/`copy` one fuel per 64 bytes: one fill of the 64 MiB a module may hold cost the default budget and **14 ms** (66 ms at the 20,000,000 maximum, a 32 MiB copy 200 ms) where a million fuel is meant to be 0.3 ms: three game ticks per event for a plugin with `event.tick` and nothing else | Medium | fixed (fbd22d9): four bytes a fuel | `plugins::tests::a_bulk_memory_operation_is_paid_for_by_the_byte`; endless recursion pinned (a trap in under a millisecond) |
| P3 | A data plugin (no fuel) walks every `every` and `when_players` entry at each tick and renders its texts at each event; the bound was the 8 MiB file: 250,000 entries made every tick **26 ms** (a tick is 5 ms) | Medium | fixed (6f2dcf9): 64 commands, 64 every, 32 when_players, 16 each of on_join and on_leave, 64 vars, texts of 2000 bytes | `plugins::data::tests::a_rules_file_is_bounded_in_what_each_tick_and_event_walks` |
| P4 | Every folder of `plugins/` is read before approval, on the game thread: a **named pipe** in the place of `plugin.sig`, the manifest, the module or `data/<world>.state` (a tar can hold one) blocked the open for ever (hang, watchdog kill, the same hang after the restart); the module's size check read a device's size (0) and then read `/dev/zero` to the end | Medium | fixed (d90eca4): `read_regular` opens without waiting, refuses what is not a regular file, caps the read | `plugins::tests::a_plugin_folder_with_a_pipe_or_a_device_in_place_of_a_file_does_not_hold_the_host` (red before: "prepare() is blocked on a pipe named plugin.sig") |
| P5 | `approve()` pinned the digest of the files it read at the moment of the answer, not of the files the admin was shown: `pcds plugin enable` waits at "[y/N]" as long as the admin likes (the dashboard dialog too), and a folder a sync, `git pull` or its author changes meanwhile was approved unseen | Medium | fixed (faed557): `approve_digest`; the CLI and the dashboard hold the digest they displayed (`POST /api/plugins/enable` takes `digest`) | `plugins::tests::files_swapped_between_the_screen_and_the_approval_are_not_approved` |
| P6 | The author's name, description, authors, license, help texts, the folder's name, a TOML error (it quotes the line) and the name of an import went raw to the admin's terminal: `ESC[8m` conceals the capability list and the question, OSC 52 sets the clipboard, a line break forges a line of the approval; the same in `pcds plugin list` and the journal | Medium | fixed (901145b): manifest texts are one line of plain text (limits in docs/plugins.md); what the host says of a plugin is cleaned (`plugins::clean`). The dashboard sets text only: unaffected | `plugins::tests::a_manifest_text_with_a_control_character_is_refused`, `what_the_host_says_of_a_plugin_has_no_control_character_whatever_the_folder_and_the_files_say` |
| P7 | `net.http.local` opened link-local, which holds the cloud metadata service (169.254.169.254 on AWS, Azure, GCP, OpenStack, a Kubernetes node): the machine's role credentials, back to the plugin through `HttpDone` | Medium | fixed (7186666): those addresses (also 100.100.100.200 and fd00:ec2::254, also IPv4-mapped) are never reachable | `plugins::net::tests::addresses_a_plugin_may_not_reach_are_every_local_private_and_reserved_range` |
| P8 | The system's name lookup cannot be interrupted, so a request that gives up leaves its thread until the resolver does (tens of seconds against a name server that never answers); a manifest may ask for `timeout_ms = 1`, a plugin starts ten requests a second: 300 requests made 300 threads | Low | fixed (269a5de): 16 lookups waiting at once for all plugins | `plugins::net::tests::lookups_that_cannot_be_given_up_are_few` (measured: 2 to 302 threads with the cap off) |
| P9 | A scoped `events.read` token chooses the `consumer` name of every poll: each new name was a table entry, a journal line and a row of `GET /api/plugins` | Low | fixed (0867462): 16 consumers, then 429 | `plugins::stream::tests::the_consumers_are_few` |
| P10 | The README's command trusts the example publisher's key, whose private half is in the repository, with `chat.say` and the event capabilities: every plugin anybody signs with it then runs without approval | Low | fixed (11e8864): a warning in the listing, the approval text and when the key is trusted (`pcds plugin trust`, `POST /api/plugins/trust`) | `plugins::sign_tests::a_plugin_signed_by_the_published_example_key_carries_a_warning` |
| P11 | ABI 1.3, found by its own hostile test: `tokens(i32::MIN)`: `abs()` wraps in a release build, so the bound of 10,000 let the most negative number through | Low | fixed (this work): `unsigned_abs` | `plugins::abi13_tests::give_and_tokens_check_their_arguments_their_capability_and_their_rate` |
| P12 | wasmi translated a function the first time it was called and charged the translation to that call: the first call of an instance cost up to 5 times the next ones (a 90 KB module: 170,000 extra fuel), so a budget that held the steady state faulted the first call after each start, and the restart after the fault did it again until the breaker switched the plugin off | Medium | fixed (this work): the module is translated when it is read (`CompilationMode::Eager`), before the world stops in a hot update | `plugins::abi13_tests::a_module_is_translated_when_read_so_its_first_call_costs_what_the_next_does` |
| P13 | A panic in the host's own code inside a host function (a bug in the world's view, say) would unwind into the interpreter's frame, which cannot unwind: the process aborts ("panic in a function that cannot unwind"), i.e. the bank, the world's last tick and every player. None is known; this is the boundary a plugin author can reach by arguments | Medium (depth) | fixed (this work): every host function runs under `catch_unwind` and a panic is a trap of that call (the plugin's fault, restarted); the call itself is guarded too | `plugins::abi13_tests::a_panic_in_the_hosts_own_code_while_it_serves_a_plugin_is_that_plugins_fault_and_the_game_goes_on` (aborted the test binary before) |
| P14 | New in 1.3: `Q_NEARBY` looks at every object of the world (100,000 on a late-game world) for a plugin that asked | Low | by design: 8 fuel for each object looked at, 64 results, 1000 units; a world too big for the budget faults the plugin | `plugins::abi13_tests::a_scan_of_the_world_costs_fuel_in_proportion_to_the_world` |
| P15 | New in 1.3: `give`, `tokens`, timers and typed settings are new ways for a plugin to act | Low | by design: capabilities with their own rates (10 gives, 2 token changes a second; 50 items, 10,000 tokens), applied after the call through the game's add path (a key or quest item is refused), 16 timers; a setting is checked on set and again on load; a secret is never readable | `plugins::abi13_tests`, the 300 generated modules in `plugins::fuzz_tests` |
| P16 | New in this work: `pcds plugin install` reads a tar file and fetches an address | Low | by design: the package is parsed by this program (regular files, plain names, 64 files, 8 MiB each, 12 MiB all, no links), https only (plain http only for 127.0.0.1), a download pinned by `--sha256` or confirmed, nothing runs at install; an index entry cannot ask for more than the package does | `plugins::pack::tests::a_hostile_package_is_refused`, `tests/cli.rs` (`a_package_is_made_pinned_installed_found_in_an_index...`) |
| H1 | The admin HTTP head was read leniently: `X: a` followed by ` Authorization: Bearer T` (a folded line) was an Authorization header, `Authorization : Bearer T` too, the last of two Content-Lengths won, a 70 KB header was cut and the request went on. A proxy in front reads each of these another way. One request a connection, so nothing was smuggled | Low | fixed (20325ba): 400 for each, 431 for a head over 64 KiB | `admin::tests::a_head_that_can_be_read_two_ways_is_refused_not_guessed` |
| H2 | The page and the API had no policy besides framing: JSON with the status and the plugin listing was cacheable by a proxy in front, and a script that got into the page (none can: no `innerHTML`, `eval` or other sink anywhere in it) could have sent the token anywhere | Low (depth) | fixed (b878920): CSP (`connect-src 'self'`, nothing else loaded, no `<base>`), `nosniff`, `no-referrer`, `no-store` | `admin::tests::the_page_has_a_policy_of_its_own_and_the_api_does_not_need_it`; the nine views opened in a browser against a scratch server, no violation |
| H3 | A hard link in a release tarball passed the post-unpack check (S23) | Low | fixed (0136ab7) | `updater::tests::releases_come_over_https_with_plain_tags_and_files` |
| H4 | CI: actions pinned by moving tags, the release job holds `contents: write` while the dependencies' build scripts run and `actions/checkout` left that token in `.git/config`; `ci.yml` had no `permissions` | Low | fixed (b993624): every action pinned by commit (dependabot proposes updates), `persist-credentials: false`, read-only default. Not run on a runner: the first release is the test | `.github/workflows/`, `.github/dependabot.yml` |
| G1 | `LoadInstanceFromScene` (allowed before PlayerReady): any number of distinct ids, each a network object for every player, a saved world object and a save line (the save is quadratic): 3,000 RPCs, 1,275 objects, 2.1 s | High | fixed (24a937f): ids below 200M, 256 scene wrecks | `instances::tests::a_client_cannot_make_scene_wrecks_without_end` |
| G2 | `SpawnAnimals` carried a trait list of up to 1 MiB: one RPC made 400,021 world objects; spawners grew without bound (also `SpawnerRegistered`, pre-ready) | High | fixed (9636678): 9 trait kinds, scene ids only, 1024 spawners, 256 pending requests | `animals::spawn::tests::a_list_of_traits_cannot_make_millions_of_world_objects`, `a_client_cannot_make_wild_spawners_without_end` |
| G3 | The toxic-area `Register*` RPCs (allowed pre-ready) pushed an entry per call: each is scanned at every tick for every player and is in the snapshot | High | fixed (f996344): 256 a client in each list | `toxic::tests::a_client_cannot_register_for_toxic_areas_without_end` |
| G4 | A client's own object id followed by garbage: a delta cut before its last flag, or a transform state that does not decode, was relayed byte for byte to everybody | High | fixed (3c0fa97): only whole deltas and decodable states are relayed | `game::tests::a_cut_variable_delta_is_not_passed_on`, `a_transform_state_that_is_garbage_is_not_passed_on` |
| G5 | RPC arrays of up to 60,000 ints at cost 1: `GetOrCreateNewInventory` 32 ms an RPC, 32 an RPC budget a tick | Medium | fixed (79fc16b, 0c31b3d): arrays capped at 4096, an RPC costs one more per KiB | `rpcspec::tests::an_array_longer_than_the_games_lists_is_not_decoded` |
| G6 | UDP reflection and amplification: a peer that never receives can drive the reliable window blind (the server's sequence starts at 0) and the forged handshake gets the world: 876 bytes sent, **2.1 MB** answered (x2445) | Medium | **partly fixed** (08886bb): the world goes to sessions from other machines at a burst of max(16, maxPlayers), then one a second. Open: Unity Transport has no return-routability proof | `handlers::tests::joins_have_a_burst_and_a_rate`, `game::tests::worlds_go_to_sessions_from_other_machines_at_a_bounded_rate` |
| G7 | Name bans and the whitelist were dodged with a space, no-break space, ideographic space, U+3164, Braille blank; a relay player (127.0.0.1) is banned by name alone | Medium | fixed (2c1093b) | `access::tests::names_compare_by_unicode_case_folding_and_normalisation` |
| G8 | `StartInterpolation`: any positive id and a speed of 0 never ends; 4,000 RPCs, 1,601 world objects | Medium | fixed (573d45a): scene ids, speed >= 1e-4, 1024 moves | `animals::interpolation::tests::a_client_cannot_start_moves_without_end` |
| G9 | RPCs over a client's budget ran inside `tick` outside the panic guard: a latent handler panic would end the bank for everyone (none found in about 4,000 rounds of every RPC) | Medium (depth) | fixed (8b08232): the panic is caught per RPC, the client kicked | `handlers::tests::a_waiting_rpc_that_panics_kicks_its_client_and_not_the_world` |
| G10 | The NetworkAnimator body was relayed undecoded up to 1 MiB; a player name cut mid-character put invalid UTF-8 in the list every client reads; `SetAndOpenWorldInstanceScene` took a scene name and recipe of any size and sent them whole to everybody at each open | Low | fixed (4ed4284, c2d9b5e, 2b5105e) | `game::tests::animator_server_rpcs_reach_the_other_clients_as_client_rpcs`, `players::tests::a_long_name_is_cut_between_characters_in_the_players_list`, `instances::tests::an_instance_opened_by_rpc_is_the_size_of_a_scenes` |
| W1 | A snapshot directory that claims more entries than bytes asked `Vec::with_capacity` for 1.3 TB and the process aborted (a damaged kept snapshot or a hostile bank) | Low | fixed (b5795d0) | `handoff::tests::a_directory_that_claims_more_entries_than_bytes_is_an_error_not_an_abort` |
| W2 | `write_atomic` took the mode from `metadata`, which follows a symlink, and the rename replaced the link: a local user's link to a 0644 file made the snapshot (Steam ids, names, chat) world-readable | Low | fixed (cf03c39) | `save::tests::a_link_in_the_way_does_not_lend_its_targets_mode` |
| W3 | `pcds world import` and the kept copies in `backups/` were written 0644 (server-written saves are 0600) | Low | fixed (37ce635) | `cli::worlds::tests::import_converts_an_old_save_and_copies_a_current_one` |

### Checked and found sound

* **`net.http`** (`plugins/net.rs`): the allow-list matches scheme, host and port after a strict parse
  (userinfo, `\`, `#`, percent escapes, a trailing dot, uppercase, IDN and punycode, decimal, octal
  and hex IP literals, `127.1`, port tricks and the zone of an IPv6 literal are refused); a wildcard
  covers names below it and never a public suffix; every redirect (relative, scheme-relative, other
  port or scheme) is checked again and `Authorization` and `Cookie` do not cross origins; the
  addresses connected to are those checked (a name with one bad address among its answers is refused
  whole, and the connector looks again); every IPv4 and IPv6 range of the task's list (mapped, 6to4,
  NAT64, link-local, ULA, `::`, 0/8, 100.64/10, 192.0.0/24, multicast, broadcast) is classified;
  `Host`, `Content-Length` and the like are the host's; CRLF cannot enter through `${config.KEY}` (the
  URL and the header values are checked after the substitution, and a value cannot be in the host);
  one wall clock covers DNS to the last byte; the answer is read one byte past its limit; TLS is
  rustls with the Mozilla roots and cannot be turned off; `proxy(None)`: no proxy variable is read
  for a plugin; the log has `METHOD scheme://host:port` and no path, query, header or body.
* **Plugin sandbox**: one memory (4 MiB by default, 64 at most), one table of 10,000, one instance,
  no WASI; every pointer is checked against the module's memory (a range outside it traps); the
  state blob is checked before it is allocated; fuel stops loops; recursion ends in a trap; actions
  are 16 a call and 200 a second; `rules.write` reaches seven sections and never `chat`, `backup` or
  `access`; a veto is for the plugin that holds `rpc.veto` and an RPC its manifest names; the
  digest is computed from the bytes the module is compiled from (no gap between scan and load); the
  entry and folder names cannot leave the plugin's folder (`..`, a leading `/` and empty parts are
  refused); `pcds plugin install` copies a manifest, an entry and a signature, never a tree.
* **Signing**: Ed25519 over `pcds-plugin-v1:` and the digest of manifest, zero byte and entry (a
  manifest cannot hold a zero byte: pinned); the signature file is checked against the files as they
  are at every load and reload; a signature that does not hold is judged as no signature; revoked
  keys do not trust; an unreadable `trusted-keys.json` trusts no key; nothing outside the manifest and
  the entry is ever loaded as code or rules.
* **Scoped tokens and the admin API**: 192 bits from `SystemRandom`, only the sha256 stored (0600),
  compared in constant time, revoked at the next request, rate limited; the scope check and the
  routes read the same (method, path), and a matrix of every route, method and spelling of its path
  with a token of each scope (1,374 requests live against a scratch server, and a unit test) lets
  through exactly the five requests the scopes name; no CORS header is sent and every state-changing
  route needs the Bearer header, so a web page cannot make a request; a DNS-rebinding page gets
  the page's HTML and nothing else (no secret is in it, the token is in the browser's storage of
  another origin); the long polls are 8 and 25 s; a request is read in 3 s (head) and 10 s (all), 64
  connections, one request each; no secret is in `/api/status`, `/api/update`, `/api/config` ("(set)")
  or the logs; the page builds all of its DOM with text (0 `innerHTML`-class sinks).
* **Updater and install**: GNU tar 1.35 refuses a member that writes through a symlink, absolute and
  `..` names (tried with a hostile archive); only plain files and folders are kept (now also single
  names); the tag is `v?[0-9A-Za-z._-]+`; https only (plain http for `http://127.0.0.1` alone); the
  downloaded program runs with `PATH`, `HOME` and `LANG` only; the bank swap is under a lock and
  flushed; `server.env` and `tokens.json` are 0600; the unit holds no secret (an `EnvironmentFile`);
  the container's entrypoint drops to `PUID:PGID` with `setpriv`, `chown -h`; compose publishes the
  admin port on 127.0.0.1, the Kubernetes example runs non-root with no capability and a read-only
  root, and its admin Service is a ClusterIP.
* **Worlds, saves, S3** (reviewed by a second reader): world names are `[A-Za-z0-9._-]`; temp files
  are `create_new`; hostile saves (NaN, infinity, 2^31 and 2^63 boundaries, huge exponents) loaded
  and re-saved without a panic; the old-format import is linear and its output is parsed again; the
  bank link is an inherited socket pair, the memfd is sealed and checked before `mmap`; the S3
  canonical request is the sent path, the `Host` header is signed as sent, no redirect is followed,
  and the keys never reach a log; tessera has one solver thread, a queue of 32 and 1000 seeds a wreck.
* **Game protocol**: UTP handshake and token binding to the address, per-address and handshake-rate
  limits, fragment, batch and queue caps, the Steam gate, the pre-ready RPC list, owner-only RPCs and
  the whitelist of variables an owner may write, finite floats, chat limits, the chat hook's payload
  (JSON, an HMAC over the body, an exec program without a shell and with a minimal environment).
* **Supply chain**: see the verdict below.

### Supply chain

`cargo audit` (advisory database of 20 October 2026, 93 crates): no vulnerability; one warning,
`paste` 1.0.15 unmaintained (RUSTSEC-2024-0436, a compile-time macro under `steamworks`, as S21).
New runtime crates since v0.6.2: `wasmi` 2.0 with `wasmi_core`, `wasmi_ir`, `wasmi_collections` and
`wasmparser` 0.228 (MIT/Apache-2.0, Apache-2.0 WITH LLVM-exception), `string-interner`, `hashbrown`,
`foldhash` (Zlib), `spin`, `libm` (MIT), `toml` 1.1 with `toml_parser`, `toml_datetime`,
`serde_spanned`, `winnow` (MIT), `unicode-normalization` with `tinyvec` ((MIT OR Apache-2.0) AND
Unicode-3.0, Zlib OR Apache-2.0 OR MIT); dev only: `wat`, `wast`, `wasm-encoder`, `bumpalo`. All are
compatible with GPL-3.0-or-later. Build scripts: `wasmi`, `wasmi_ir`, `wasmparser` 0.228 and `libm`
(read: code generation and compiler feature probes; no network, no process but `rustc --version`).
`cargo deny` is not installed; the licences were read from `cargo metadata`. Actions are now pinned
by commit (H4). The Docker bases (`rust:1-trixie`, `debian:trixie-slim`) were tags, not digests;
they are pinned by digest now (I1). Both the updater and S3 take the operator's `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY`
(ureq's default: a corporate proxy is a use of the feature; TLS is end to end); the plugins and
the chat webhooks set `proxy(None)` and never do.

### Fixed after 0.8.0

* **I1** Docker base images pinned by tag only: fixed. The Dockerfile pins `rust:1-trixie` and `debian:trixie-slim` by digest
  (the tag stays for the reader), and Dependabot's `docker` entry proposes the new digest. The build and `docker/test.sh` ran on
  the pinned images.
* **I2** The systemd unit without sandboxing, `setpriv` without `--no-new-privs`: fixed. The unit carries a sandbox that scores 2.1
  (OK) in `systemd-analyze --user security` where the old one scored 9.4 (UNSAFE), tried against the real server, Steam, hot
  updates, S3, webhooks, hook programs, a plugin with `net.http` and `steamcmd`; the options that break `steamcmd` or Node and Java
  hooks (`MemoryDenyWriteExecute`, `SystemCallArchitectures=native`) are documented, not set. The entrypoint runs the server with
  `setpriv --no-new-privs`, `compose.yaml` turns on `read_only`, `no-new-privileges`, dropped capabilities and a process limit, and
  `docker/test.sh` runs its hot update with them. [hardening.md](hardening.md).
* **I9** The admin port exposed past the loopback can be held by slow connections: fixed as far as one address goes. The server
  gives other machines 48 of the 64 slots and one address 16, answers a connection over its share `503` with `Retry-After` at once,
  waits 2 seconds (was 3) for a request head, warns at every start (and says the token can run programs through `chat.exec`), and
  `pcds doctor` warns; [reverse-proxy.md](reverse-proxy.md) has a Caddy and an nginx configuration tried against the server. Many
  addresses together can still take the 48: a firewall or the proxy's own limits answer that, and behind a proxy on the same machine
  every client is the loopback, so the proxy has to tell them apart (the document gives the nginx lines). A Host-header check was not
  added: a DNS-rebinding page reads only the HTML. Binding past the loopback stays allowed (a container, a private network); making it
  an error is the owner's decision.

### Open

* **I3** The trust root of an update is the release feed (S22): the sha256 comes from the same place
  as the tarball, and `pcds` of the new bank is run (`bank-info`) before it is installed. A feed that
  is compromised runs code as the server's user. Next: sign releases (the plugin keys' Ed25519 would
  do) and check the signature before `ask`.
* **I4** (G6) A UDP source address can be forged: sessions cannot be proven return-routable inside
  Unity Transport. The reflection is bounded to about one world a second per server; a spoofer can
  also fill the 128 half-open slots for 10 s at a time. Next: a cookie in the handshake would need a
  client that speaks it, so none here.
* **I5** A ready player can create records without a quota (`CreateAndInstantiate`,
  `CreateAndDropOnFloor`, `AddWorldObjectToInventory`, inventory contents: up to about 6,400 RPCs a
  second). Next: a rule `limits.maxWorldObjects`.
* **I6** The game's host-trust RPCs are open to any ready player, as in the original game:
  `GainTokens`, `UnlockGroupGlobally`, `SwitchToPlanet`, `LaunchSpecificMeteoEvent`,
  `Deconstruct` of anyone's building, `ResetOwnership`. The name ban and the whitelist are the gates;
  a name-only whitelist on direct IP does not authenticate (README).
* **I7** A tessera solve that fails (1000 seeds) leaves its wreck ungenerated and a client can ask
  again; no seed that fails is known. Next: a persisted `failed` flag (the snapshot format changes).
* **I8** A plugin with `rules.write` can set `limits.bankWatchdogSeconds` (an operational setting,
  not gameplay) and `limits.maxPlayers`. Next: take the watchdog out of `PLUGIN_RULES` by key.
* **I10** Unchanged from the audits: S11 (the memfd seal result of the front's own bank), U10.

## Round 3: fuzzing, properties, sanitizers (v0.8.0)

A search for what the audits above did not find, with other methods than reading: long fuzzing
runs under AddressSanitizer with persistent corpora, differential and property tests against a
reference, the sanitizers and overflow checks on the whole suite, a fresh reading of the riskiest
modules and fault injection. Made in October 2026 on `main` at c5ca29e (v0.8.0). The rules were
those of the other sections: a finding is proved by a failing test or run, fixed in its own commit
with the regression test, and no protection is weakened. Ids: R.

### Findings

| Id | Finding | Sev. | Status | Evidence |
|---|---|---|---|---|
| R1 | `net.http` reached Azure's WireServer: 168.63.129.16 is a **public** address, so a plugin with `net.http` and no `net.http.local` could read an Azure VM's extension settings and secrets through any allowed name that resolved to it (P7 closed only the link-local metadata addresses). The container credentials of AWS ECS and EKS (169.254.170.2, 169.254.170.23, fd00:ec2::23), Tencent's metadata (169.254.0.23) and Google's IPv6 metadata (fd20:ce::254) were open to `net.http.local` | Medium | fixed (76579b4): never reachable by any plugin | `plugins::net::tests` (the address-class test fails for 168.63.129.16 without the change); found by the `pluginnet` target against a reference from the IANA registries |
| R2 | The plants of a Farm or a VegetableGrower whose items were destroyed (`RemoveAndDestroyAllItemsFromInventory`, which the game itself calls when a Farm is deconstructed) keep their network objects, as the game's clients do, but nothing removed them when the machine went: a network object for a world object that is gone stayed for good in every client's scene, every join and every snapshot | Medium | fixed (88ffaab): the machine's destruction takes them along | `machines::growers::tests::a_growers_plants_keep_their_network_objects_when_their_inventory_is_destroyed` (fails before: "the machine takes its plants along"); the hostile-RPC test on the late-game world failed on seeds 7, 8 and 9 before it and passes with it |
| R3 | A save whose planets, players, world objects, inventories, messages or story events held a string, number, array or null after the first entry loaded (the loader looked at the first entry of each list only); the game's code sets fields on these entries and serde_json panics on any value but an object, so the world played and then **could never save**: the panic came at the first autosave and every one after it | Medium | fixed (e259acc): every entry is a record or the file is refused at the start | `save::tests::an_entry_that_is_no_record_is_refused_not_kept_to_panic_a_save_later`; found by the `loadsave` target (`cannot access key "host" in JSON string`) |
| R4 | serde_json's default float parsing is an ulp off for about one f64 in ten (91,000 to 105,000 of a million written and read back differed): the planets' unit totals of a loaded save moved, and load and save was not a fixpoint (the soak's "save fixpoint: 1 lines differ" was this); the same in the messages between the front and the banks | Low | fixed (6aa6e24, d38e0f7): `float_roundtrip`; a number past f32 in a rules file is now refused when it is read | `save::tests::a_float_in_a_save_reads_back_as_it_was_written`; `rules::tests::a_rules_file_number_past_f32_is_refused` |
| R5 | A rules file or a rules change that is a JSON array was read by position: `[[0.5]]` set `meteo.eventChance` | Low | fixed (9ba1624) | `rules::tests::rules_are_an_object_of_sections_never_an_array_read_by_position` |
| R6 | A toxic area's match used `(id - key).abs()` on a key any client may send (before PlayerReady): the key `i32::MIN` matched the area at the origin, `i32::MIN + 1` the one at `i32::MAX`; with overflow checks the subtraction panicked in the tick | Low | fixed (e9a1c1a): `abs_diff` | `toxic::tests::a_key_far_from_every_area_finds_none_whatever_the_numbers` |
| R7 | The admin HTTP reader ended the head at any line that was empty after trimming (a proxy that reads such a line as the fold of the line before sees another head), and read `Content-Length: +5` as 5 | Low | fixed (d2e0f54): the head ends at an empty line only, a line of blanks is a 400, a length is digits | `admin::tests::a_head_that_can_be_read_two_ways_is_refused_not_guessed` |
| R8 | `libraryfolders.vdf` and the game's app manifest were parsed by recursion without a bound: a file of `"a" {` 250,000 times ended `pcds setup` and `up` with a stack overflow | Low | fixed (12f4594): 32 levels | `cli::steam::tests::a_file_nested_without_end_is_refused_not_recursed_into` |
| R9 | The fuzz crate had not compiled since the game tables became raw JSON (8 errors): none of the 15 targets of the earlier audits could be run again | Process | fixed (341c6cb): it builds, and CI and `make fuzz-check` type-check it | CI job `fuzz` |
| R10 | The snapshot reader set the length of the world objects and inventories it read to `capacity()`; a `Vec` has at least the room asked for, not exactly that | Hardening | changed (25cf37c): the count read | no behaviour changes; not triggered |
| R11 | The text format of the log wrote a message as it came: a carriage return in it (a name, a bucket's error text, a release feed's message) moved the cursor back over the line's own time and level, an escape sequence cleared or recoloured the screen of `pcds logs`, `journalctl` and `docker logs` (line breaks were already marked line by line) | Low | fixed (0c80ad5): control characters are written as `\u{..}` | `logging::tests::a_message_cannot_steer_a_terminal_through_the_log` (fails before with the raw `\r` and `\u{1b}` in the line) |
| R12 | The error text of a failed S3 request took the XML `Code` and `Message` raw: control characters to the admin's terminal (`pcds backup fetch`), the status and the log, entities unread, up to 16 KiB | Low | fixed (d006c59): plain text, 200 characters a field | `s3::tests::an_error_body_is_plain_text_and_a_listing_reads_back_its_keys` |
| R13 | Two advisories of 19 and 20 October against the TLS stack of the updater, the S3 copies and `net.http`: rustls-webpki 0.103.15 accepted chains through distrusted intermediates (RUSTSEC-2026-0382), webpki-roots 1.0.9 bundled the emSign roots with them trusted (RUSTSEC-2026-0381) | Medium | fixed (c4851a3): 0.103.16 and 1.0.10 | `cargo audit`: 2 vulnerabilities before, none after |

R11 and R12 are in code that came with the operability release (`pcds doctor`, the metrics, the
structured log, `pcds backup`), merged into this branch from `integrate` after the first findings.

### Coverage

**Fuzzing** (libFuzzer with AddressSanitizer and overflow checks on, one process per target, corpora
kept between runs, seeds from `server/fuzz/seeds.py` and, for `utp`, from the real traffic of a bot
session on the late-game world recorded through a UDP proxy). Executions are the sum of every run
of a target on this branch; the corpus is what libFuzzer kept (not minimised).

| Target | Executions | Runs | Corpus files |
|---|---|---|---|
| `access` | 4,084,352 | 3 | 2,903 |
| `adminhttp` | 2,927,221 | 3 | 4,363 |
| `adminroutes` | 193,963 | 3 | 2,229 |
| `backup` | 676,421 | 3 | 1,438 |
| `bincodec` | 29,809,283 | 3 | 7,568 |
| `chathooks` | 15,673,354 | 3 | 13,507 |
| `config` | 9,999,666 | 3 | 3,287 |
| `doctor` | 216,675 | 3 | 3,092 |
| `front` | 26,257,000 | 3 | 11,072 |
| `game` | 108,623 | 4 | 1,884 |
| `handoff` | 34,673 | 3 | 917 |
| `ipc` | 29,488,993 | 3 | 162 |
| `loadsave` | 9,725 | 4 | 948 |
| `logline` | 7,754,878 | 3 | 6,005 |
| `manifest` | 11,359,161 | 3 | 37,720 |
| `msg` | 27,357,285 | 3 | 18,661 |
| `names` | 40,099,227 | 3 | 118 |
| `oldsave` | 9,537,181 | 3 | 14,148 |
| `pluginnet` | 40,793,396 | 3 | 2,378 |
| `pluginsign` | 12,035,291 | 3 | 3,948 |
| `prom` | 7,965,688 | 3 | 7,180 |
| `rpcspec` | 26,668,265 | 3 | 10,567 |
| `rules` | 16,759,873 | 3 | 10,708 |
| `s3resp` | 1,837,897 | 3 | 2,432 |
| `save` | 503,089 | 3 | 3,190 |
| `savelayout` | 2,166,900 | 3 | 5,897 |
| `tessera` | 670 | 3 | 218 |
| `toxic` | 66,558,613 | 3 | 46 |
| `transform` | 38,047,111 | 3 | 121 |
| `twin` | 3,729 | 4 | 340 |
| `updater` | 18,056,136 | 3 | 9,007 |
| `utp` | 11,244,189 | 3 | 3,912 |
| `wasmhost` | 179,851 | 3 | 2,415 |
| `wire` | 44,906,309 | 3 | 861 |
| 34 targets | 503,314,688 | | 193,242 |

The last run of every target (15 to 45 minutes each) was on the final code but for the last three commits (a JSON line of the log, the S3 error text and the doctor target; the five targets of the operability surfaces ran again after them). The `game`, `twin` and
`loadsave` targets run on the late-game save (`PCDS_FUZZ_SAVE`) and execute a few executions a
second: their number is small on purpose; a "tiny" corpus of a small world ran beside them.
`tessera` found no defect: the slowest seed (2644773, generator 2) takes 1.2 s in the release build
(47 s under ASan). An earlier version of the `game` target let no game time pass (its "time passes"
input added to a clock the tick overwrites), so no machine, gauge or unit timer had ever come due in
the fuzzing of the earlier audits; it moves the game's own clock now.

**Differential and property tests** (all in the suite, seeded, no new dependency): the range rules of
`access` against a bit-by-bit reference for both address families and every prefix length (200,000
cases); `clock::take_due` against the loop it replaces (20,000 cases around the hour where the rule
changes); rules patches (a merge patch sets what it names, null puts the default back, for every
rule; what is accepted reads back from `rules.json`); every RPC of the game's table written and read
back as it was (40 bodies each, a body cut anywhere does not panic); a rotation survives its Euler
angles everywhere (200,000 cases); the UTP layer through a relay that loses 10 %, duplicates 10 %
and reorders by up to 40 ms (150 batches each way arrive once, in order, byte for byte, the window
never passes 64). The save writer's fixpoint, the snapshot round trip and "the game restored from
its snapshot sends the same bytes for 1000 ticks" are the `savelayout`, `bincodec` and `twin`
targets and the bench's handoff run (EQUAL); inventory algebra and the logistics invariants are the
invariants of the hostile-RPC test below, not separate property tests.

**The hostile-RPC test on the late-game world** (`hostile_rpcs_on_a_real_world_keep_it_whole`, 18,900
RPCs of every kind with random arguments to two clients and a tick every seventh round, invariants
after each: holders, inventories, network objects, constructed and pickable lists, a world loaded from
its own save): seeds 1 to 11, all pass at the final code. Seeds 7, 8 and 9 failed before R2 was fixed.
With overflow checks on, seeds 1 to 6 at 30,000 rounds, the whole suite, the late-game bench and a
3600 s soak find no overflow in what a client can reach.

**Sanitizers**: AddressSanitizer on the whole suite (the unit tests, `cli`, `admin_golden`,
`hot_update` with the front and the banks as ASan processes): no memory error; three tests failed,
all explained (R4's rules test, which found the float feature's effect, and two timing assertions
that a sanitizer's slowdown breaks). ThreadSanitizer on the unit suite and on `hot_update` with
the reports of the child processes collected: no data race. **Miri** on 32 tests of the pure modules
(`wire geometry transform protocol clock toxic bincodec idmap msg worlds`, the plugin address, manifest
and data tests, `access`, the Steam parser, `rules`, `config`): no undefined behaviour; the other 41
tests selected are property tests too slow for Miri and were stopped after two hours without a failure.
**cargo audit**: R13. **clippy**: `cargo clippy --all-targets` did not compile (a deny lint on a
quaternion constant in three tests, fixed in a94552d); `-W clippy::pedantic` gives 3,590 warnings,
551 of them narrowing casts, and was not read line by line.

**Fault injection**: `tools/chaos/diskfull.sh` (an 8 MB tmpfs, the world of the bench, four rejoining
players) passes: a save on a full disk fails with the error and keeps the last good file, the autosave
says so in `lastSave`, the final save on stop fails without losing the acknowledged one, a `kill -9` of
the bank on a full disk is answered by the front within 0.5 s (the lead of the dynamic audit, an 8 s
refusal, does not reproduce), and a save works again after freeing the space.

### Checked and found sound

* **The save and the snapshot**: load and save are a fixpoint on the late-game save (after R4); the
  snapshot restored in a new game continues byte for byte for 1000 ticks (`twin`); a damaged save one
  field at a time is refused or loads and saves (`loadsave`, after R3); the handoff reader refuses a
  cut or lying directory (W1), and the length it sets is the count it read (R10).
* **The exposition of the metrics** (`front/prometheus.rs`): any JSON the front puts together gives text
  a strict reader of the format accepts (labels escaped, only finite numbers, one family at a time,
  counters named `_total`); the documentation lists every name.
* **The secrets of the log**: a registered secret of six characters or more, a scoped token and a
  bearer token never appear in either format; masking twice is masking once.
* **`pcds doctor` and `pcds backup` on hostile files**: no panic on any server.env, rules.json,
  access.json, world, approvals or backup file name; the list holds this world's backups only, newest
  first; a name resolves to a file that exists; the unit files for a path with quotes, percent signs,
  spaces or a dollar have the lines of a plain path (a path with a line break is refused).
* **The admin API's routes** with hostile bodies on a game with the example plugins: no 500 but the
  ones that report a disk, the answer is JSON, the game still snapshots (`adminroutes`).
* **The wasm host boundary**: a module made from the input calls every host function with pointers on
  the edge of its memory; every one traps or is refused (`wasmhost`).
* **The plugin host's network classes** against a reference from the IANA registries (`pluginnet`),
  after R1; **the signing** and the key files (`pluginsign`); the manifests and data plugins' files
  (`manifest`).
* **Read, nothing found**: the flip state machine and the supervisor, the bank's persistence, the serve
  loop and the panic guard, the codec, the updater, the token store, `access`, the S3 client's signer
  (AWS's own examples) and its uploads, `replace_file`'s fixed temporary name (a local attacker in the
  same directory is outside the model: the directory is the user's), the install script, the Docker
  entrypoint and Dockerfile.

### Not done, and leads

* **`clock::take_due`** loops for ever on a timer of negative infinity; nothing in the server makes
  one. Not changed.
* **`PCDS_UNIT`** is not validated: a name with `/` writes the unit files beside, not in, the unit
  folder. It is the operator's own environment; noted in case units are ever written for a name
  from elsewhere.
* **Miri** did not run the 41 slow property tests or anything with sockets, `ring`, fsync or memfd.
* **The fuzz targets that need a world** (`game`, `twin`, `loadsave`, `adminroutes`) run a few
  executions a second; a cluster of long runs would find more.
* **Clippy pedantic** was not read through.
* **The chaos tools** (`chaos.py`, `churn.py`, `limits.py`, `stress.py`, `soak.py`) were not run again
  by this round; the long soak and chaos of the final build is a separate task.

## Open

Everything below is still open at 0.7.0. Each line gives the reason and the smallest next step.
Items already fixed in this branch (the plugin examples build across checkout paths, `docker/test.sh`
on rootless Docker, stale module names in comments) are not repeated.

Architecture:

* **A9** Two atomic writers and two lock helpers remain (`save::write_atomic` and `util::replace_file`; `updater::lock` and `util::hold`). Next: decide whether the differences are needed, then one of each.
* **A10** The front reads settings and rules by name (`front → config`, `front → rules`). Next: step 8.
* **A12** Typed admin requests, the `err()` helper and one timeout table are not done. Next: type
  the request bodies of kick, ban, teleport.
* **A16** The animator sync blob and the owner-write list are game facts in source. Next: have the
  reader emit them into the tables (bumps the tables format).
* **A21** S3 uploads run in the game process with statics outside the snapshot. Next: owner decides
  whether to move them to the front.
* **A22** Join rules in four places. Next: one pure admission function with tests.
* **A23** The flip and recovery logic has no model tests and `bank::run` exits inline. Next: extend
  `tests/hot_update.rs` first, then step 15.
* **A24** The table reader has one ignored test. Next: run `tools/dev/tables-check.sh` on every game
  update and before touching `native/`.
* **A26** The web page restates the unit order. Next: serve the unit order from `/api/status`.

Static:

* **S21** `paste` is unmaintained (build time, via `steamworks`). Next: rerun `cargo audit` and
  `cargo deny` for the crates added since 0.6.1.

Dynamic:

* **D7** Re-measured after 0.8.0 (docs/performance.md, docs/hot-update.md "Flip speed"): at 200,000 objects a join is 12 ms of the game thread (2.4 MB sent), a flip stands 95 to 129 ms (real front and banks, no players; 162 ms in process), the idle tick on a late-game save went from 196 to 24 M instructions a second. The longest phase of the flip was the restore's re-reading of the machines' tables, now once for each group; what is left is the decoding of the world objects.

Lead from the tools' smoke runs, not a finding: in two of three disk-full runs against 0.6.2,
`GET /api/status` was refused for 8 s after a `kill -9` of the bank on a full disk although the
front's log showed the bank restarting. Not reproduced on 0.7.0; run `tools/chaos/diskfull.sh` on a
0.7.0 build to see.

Unverified (could not be checked, listed so they are not lost): **S14** (chat hooks stay off after a
delivery panic), **S15** (two sets growing per hostile connection id), **S25** (small suspects), and
the parts of A3, A14, A19 and A26 named in their rows.
