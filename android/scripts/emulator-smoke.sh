#!/usr/bin/env bash
# Install the debug APK on a running emulator, open both screens, grant
# what can be granted from adb, run one sync, and fail on any crash.
set -euo pipefail
apk=$(ls "${1:-apk}"/*.apk | head -1)
pkg=se.frankling.time.debug

adb install -r "$apk"
adb shell appops set "$pkg" GET_USAGE_STATS allow
adb shell pm grant "$pkg" android.permission.POST_NOTIFICATIONS || true
for p in READ_STEPS READ_DISTANCE READ_HEALTH_DATA_IN_BACKGROUND; do
  adb shell pm grant "$pkg" "android.permission.health.$p" || true
done
adb logcat -c

adb shell am start -W -n "$pkg/se.frankling.time.MainActivity"
sleep 10
adb shell am start -W -n "$pkg/se.frankling.time.SettingsActivity"
sleep 10

adb logcat -d > logcat.txt || true
if grep -E "FATAL EXCEPTION|AndroidRuntime: Process: $pkg" logcat.txt; then
  echo "crash:"; grep -A30 "FATAL EXCEPTION" logcat.txt | head -60
  exit 1
fi
adb shell pidof "$pkg" > /dev/null || { echo "app is not running"; tail -100 logcat.txt; exit 1; }
echo "smoke test passed"
