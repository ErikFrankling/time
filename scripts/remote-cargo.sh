#!/usr/bin/env bash
# Run cargo on the homelab build server instead of the pc.
#
# The pc is the machine Erik works at and runs every agent session; a
# full-parallel Rust build there competes with all of it for memory. The
# build server has ~36 GB free and nobody sitting at it.
#
#   scripts/remote-cargo.sh test --workspace
#   scripts/remote-cargo.sh clippy --workspace --all-targets
#
# The tree is copied fresh each run (tracked and untracked-but-not-ignored
# files); the target directory lives beside it, so builds stay incremental.
set -euo pipefail
host=${TIME_BUILD_HOST:-nix-builder}
dir=${TIME_BUILD_DIR:-/root/time-v2}
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)

git -C "$root" ls-files -co --exclude-standard -z \
  | tar -C "$root" --null -T - -czf - \
  | ssh "$host" "rm -rf $dir/src && mkdir -p $dir/src && tar -xzf - -C $dir/src"

# Bring the lockfile back whatever cargo did, so the resolved versions are
# committed from the machine that actually built them.
trap 'ssh "$host" "cat $dir/src/Cargo.lock" > "$root/Cargo.lock.tmp" 2>/dev/null && mv "$root/Cargo.lock.tmp" "$root/Cargo.lock" || rm -f "$root/Cargo.lock.tmp"' EXIT

# shellcheck disable=SC2029  # the arguments are meant to expand remotely
ssh "$host" "cd $dir/src && CARGO_TARGET_DIR=$dir/target nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc nixpkgs#clippy nixpkgs#rustfmt --command cargo $*"
