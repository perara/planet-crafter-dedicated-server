<!-- What changed and why, in plain words. See CONTRIBUTING.md. -->

## Checklist

- [ ] Tests for the change; `PCDS_TEST_DATA=none PCDS_GAME_DIR=none cargo test --release --locked` and `cargo clippy --release --locked` are clean
- [ ] No game data, saves, tokens, Steam ids or personal data in the diff
- [ ] Docs updated; generated files (docs/env.md, docs/admin-api.md) regenerated
- [ ] CHANGELOG.md `[Unreleased]` says what a user will notice, with Breaking changes and Upgrade when there are any
- [ ] A change to the snapshot, the front protocol or the tables bumps `FORMAT`, `PROTOCOL` or `TABLES_FORMAT`
