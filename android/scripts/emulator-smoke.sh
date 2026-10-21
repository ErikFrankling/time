#!/usr/bin/env bash
# End to end on a real emulator: install the debug APK, point it at the Time
# server running on the host (10.0.2.2 from inside the emulator), open both
# screens, and pass only if the server receives the phone's heartbeat and no
# activity crashed.
set -euo pipefail
apk=$(ls "${1:-apk}"/*.apk | head -1)
server=${TIME_SERVER:-http://127.0.0.1:7373}
pkg=se.frankling.time.debug

adb install -r "$apk"
adb shell appops set "$pkg" GET_USAGE_STATS allow
adb shell pm grant "$pkg" android.permission.POST_NOTIFICATIONS || true
for p in READ_STEPS READ_DISTANCE READ_HEALTH_DATA_IN_BACKGROUND; do
  adb shell pm grant "$pkg" "android.permission.health.$p" || true
done

# Debug builds are debuggable, so run-as can write the app's preferences:
# the website and the event gateway are both the host's Time server.
cat > /tmp/time-prefs.xml <<XML
<?xml version='1.0' encoding='utf-8' standalone='yes' ?>
<map>
    <string name="server">http://10.0.2.2:7373</string>
    <string name="otlp">http://10.0.2.2:7373/v1/logs</string>
    <string name="device">phone</string>
    <boolean name="askedBatteryExemption" value="true" />
</map>
XML
adb shell "run-as $pkg sh -c 'mkdir -p shared_prefs && cat > shared_prefs/time.xml'" < /tmp/time-prefs.xml
adb logcat -c

adb shell am start -W -n "$pkg/se.frankling.time.MainActivity"
sleep 5
adb shell am start -W -n "$pkg/se.frankling.time.SettingsActivity"
sleep 5
adb shell am start -W -n "$pkg/se.frankling.time.MainActivity"

# Opening the app with no recent sync runs one immediately.
ok=""
for _ in $(seq 1 30); do
  hb=$(curl -sf "$server/api/status" | jq -r '.devices[] | select(.name=="phone") | .last_heartbeat_ms // empty' || true)
  if [ -n "$hb" ]; then ok=1; break; fi
  sleep 5
done

adb logcat -d > logcat.txt || true
if grep -E "FATAL EXCEPTION" logcat.txt; then
  echo "crash:"; grep -A30 "FATAL EXCEPTION" logcat.txt | head -60
  exit 1
fi
if [ -z "$ok" ]; then
  echo "the server never received a heartbeat from the phone"
  tail -40 /tmp/time-server.log 2>/dev/null || true
  curl -s "$server/api/status" | jq . || true
  grep -iE "time|sync" logcat.txt | tail -60 || true
  exit 1
fi
echo "server status as seen after the phone's sync:"
curl -s "$server/api/status" | jq '{devices, channels: [.channels[] | select(.state != "not_set_up")]}'
echo "smoke test passed: the phone reached the server"
