#!/usr/bin/env bash
# Build the app and install it straight onto the phone over the network.
#
# This is the normal Android development loop, and it is what should be used
# while iterating. The GitHub release path exists for a different job: getting
# a build onto a device that is not on this network, or into someone else's
# hands. Using it to move a build across the room means uploading 25 MB to a
# datacentre so a phone two metres away can download it again.
#
# Pairing is one-time. Wireless debugging then re-announces itself over mDNS on
# every reconnect, so the port changing after a reboot does not matter.
#
# Usage:
#   scripts/push-to-phone.sh pair <host:port> <code>   once, from the phone's screen
#   scripts/push-to-phone.sh                           build and install
#   scripts/push-to-phone.sh --release                 install the CI-signed build
set -euo pipefail

cd "$(dirname "$0")/.."
STATE="${XDG_STATE_HOME:-$HOME/.local/state}/time"
mkdir -p "$STATE"
LAST="$STATE/phone-address"

command -v adb >/dev/null || {
  echo "adb not found. Run inside: nix develop --command scripts/push-to-phone.sh" >&2
  exit 1
}

# mDNS is how a phone announces wireless debugging, and it is the only way to
# find it again after a reboot hands it a new port.
discover() {
  adb mdns services 2>/dev/null \
    | awk '/_adb-tls-connect/ {print $3; exit}'
}

connect() {
  local addr
  addr="$(discover || true)"
  if [ -z "$addr" ] && [ -f "$LAST" ]; then
    addr="$(cat "$LAST")"
  fi
  [ -n "$addr" ] || return 1
  adb connect "$addr" >/dev/null 2>&1 || return 1
  # `adb connect` reports success for a port that merely accepts TCP, so the
  # device list is what actually confirms it.
  adb devices | grep -q "^${addr}[[:space:]]*device$" || return 1
  echo "$addr" > "$LAST"
  echo "$addr"
}

if [ "${1:-}" = "pair" ]; then
  [ $# -eq 3 ] || { echo "usage: $0 pair <host:port> <code>" >&2; exit 2; }
  adb pair "$2" "$3"
  echo "Paired. Finding the debugging port..."
  sleep 2
  if connect >/dev/null; then
    echo "Connected."
  else
    echo "Paired but not connected -- open Wireless debugging again and re-run without arguments." >&2
    exit 1
  fi
  exit 0
fi

RELEASE=""
[ "${1:-}" = "--release" ] && RELEASE=1

ADDR="$(connect || true)"
if [ -z "$ADDR" ]; then
  cat >&2 <<'EOF'
No phone found.

On the phone: Settings → System → Developer options → Wireless debugging → on.
Then "Pair device with pairing code" and run, with the numbers it shows:

  nix develop --command scripts/push-to-phone.sh pair <IP:PORT> <CODE>

The phone must be on the same network. If Developer options is missing, tap
Settings → About phone → Build number seven times.
EOF
  exit 1
fi
echo "Phone: $ADDR"

if [ -n "$RELEASE" ]; then
  APK="$(find android/app/build/outputs/apk/release -name "*.apk" -printf "%T@ %p\\n" 2>/dev/null | sort -rn | head -1 | cut -d" " -f2-)"
  [ -n "$APK" ] || { echo "no release APK built; run gradle assembleRelease" >&2; exit 1; }
else
  ( cd android && gradle --no-daemon assembleDebug -q )
  APK="android/app/build/outputs/apk/debug/app-debug.apk"
fi

echo "Installing $(basename "$APK")..."
# -r reinstalls keeping data; -d permits a lower versionCode, which happens
# whenever a local build follows a CI one, since CI numbers from the run count.
adb -s "$ADDR" install -r -d "$APK"

# Granting appops directly sidesteps the restricted-settings block that a
# sideloaded install would otherwise hit, so a fresh install is usable
# immediately rather than after a detour through three settings screens.
PKG="se.frankling.time"
[ -z "$RELEASE" ] && PKG="se.frankling.time.debug"
adb -s "$ADDR" shell appops set "$PKG" android:get_usage_stats allow 2>/dev/null || true
echo "Installed and usage access granted."
