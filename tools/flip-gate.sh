#!/usr/bin/env bash
# The flip gate: the last release's world taken over, live, by this build (docs/hot-update.md,
# "The flip gate"). It fetches the last published release that is not this build's version, runs
# it on a world with one of every building and players, and has this build's bank take the
# world over. Run by hand before every tag (`make gate`): it needs the game's tables, which no
# CI has.
#
# A release whose snapshot format, front protocol or tables format differs from this build's
# cannot be hot-updated into it: that is a restart install by design (no legacy support), and
# the gate says so loudly and skips. It tests a flip only between releases of one format.
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

# The program finds libsteam_api.so next to itself in a release; in the build tree it is in the
# steamworks crate's output.
cd server
cargo build --release --locked
lib=$(dirname "$(ls -t target/release/build/steamworks-sys-*/out/libsteam_api.so | head -1)")
export LD_LIBRARY_PATH="$PWD/$lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# What each side says it is: the fields that decide whether the front accepts a flip.
shape() { "$1" bank-info | grep -o '"\(format\|protocol\|tablesFormat\)":[0-9]*' | sort | tr '\n' ' '; }
before=$(shape "$work/$name/pcds")
now=$(shape target/release/pcds)
if [ "$before" != "$now" ]; then
  echo "################################################################"
  echo "flip gate SKIPPED - format changed: $tag is not hot-update compatible with v$version"
  echo "  $tag: $before"
  echo "  v$version: $now"
  echo "Install with 'pcds upgrade --restart' (the world is saved and loads from the save);"
  echo "say so in the release notes. Nothing was tested."
  echo "################################################################"
  exit 0
fi

export PCDS_PREVIOUS_RELEASE="$work/$name"
cargo test --release --locked --test hot_update a_hot_update_from_the_release_before_works -- --ignored --nocapture
