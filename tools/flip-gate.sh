#!/usr/bin/env bash
# The flip gate: the last release's world taken over, live, by this build (docs/hot-update.md,
# "The release before"). It fetches the last published release that is not this build's version,
# runs it on a world with one of every building and players, and has this build's bank take the
# world over. The first hot update (0.6.0 to 0.6.1) failed on a snapshot field the release
# before did not write; only this shows that. Run before every tag (the release workflow does).
#
#   tools/flip-gate.sh [TAG]      TAG: the release to flip from (default: the last one before this version)
#   needs PCDS_TEST_DATA: a game tables folder (the gate runs a real world on them)
set -euo pipefail

cd "$(dirname "$0")/.."
repo=perara/planet-crafter-dedicated-server
version=$(sed -n 's/^version = "\(.*\)"/\1/p' server/Cargo.toml | head -1)
: "${PCDS_TEST_DATA:?PCDS_TEST_DATA: a folder of game tables (run/data/<version> of an installed server)}"

tag=${1:-}
if [ -z "$tag" ]; then
  # Newest first; the release of this version itself (a re-run after the tag) does not count.
  tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=20" | grep -o '"tag_name": *"[^"]*"' | cut -d'"' -f4 | grep -vx "v$version" | head -1)
fi
[ -n "$tag" ] || { echo "flip gate: no release before v$version to flip from" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
name="pcds-$tag-linux-x86_64"
echo "flip gate: $tag into v$version"
curl -fsSL -o "$work/$name.tar.gz" "https://github.com/$repo/releases/download/$tag/$name.tar.gz"
curl -fsSL -o "$work/$name.tar.gz.sha256" "https://github.com/$repo/releases/download/$tag/$name.tar.gz.sha256"
(cd "$work" && sha256sum -c "$name.tar.gz.sha256")
tar -xzf "$work/$name.tar.gz" -C "$work"

# The fixtures of every release restore, and the live flip from the release before works.
export PCDS_PREVIOUS_RELEASE="$work/$name"
cd server
# The program finds libsteam_api.so next to itself in a release; in the build tree it is in the
# steamworks crate's output.
cargo build --release --locked
lib=$(dirname "$(ls -t target/release/build/steamworks-sys-*/out/libsteam_api.so | head -1)")
export LD_LIBRARY_PATH="$PWD/$lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
cargo test --release --locked --bin pcds the_snapshot_of_every_release_restores
cargo test --release --locked --test hot_update a_hot_update_from_the_release_before_works -- --ignored --nocapture
