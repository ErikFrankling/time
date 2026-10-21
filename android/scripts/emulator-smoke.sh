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
# Edge to edge: with a tall display cutout, the native bar must sit below
# the status bar, and Phone settings must open. Screenshots are uploaded.
mkdir -p shots
adb shell cmd overlay enable com.android.internal.display.cutout.emulation.tall || true
sleep 3
adb shell am force-stop "$pkg"
adb shell am start -W -n "$pkg/se.frankling.time.MainActivity"
sleep 8
adb exec-out screencap -p > shots/1-main.png
adb shell uiautomator dump /sdcard/ui.xml >/dev/null && adb pull /sdcard/ui.xml shots/1-main.xml >/dev/null
bounds=$(grep -o 'content-desc="Phone settings"[^>]*bounds="\[[0-9]*,[0-9]*\]\[[0-9]*,[0-9]*\]"' shots/1-main.xml | grep -o '\[[0-9]*,[0-9]*\]\[[0-9]*,[0-9]*\]' | head -1)
echo "Phone settings bounds: $bounds"
top=$(echo "$bounds" | sed -E 's/\[([0-9]+),([0-9]+)\].*/\2/')
sb=$(adb shell dumpsys window | grep -m1 -E "type=statusBars" | grep -o 'frame=\[[0-9]*,[0-9]*\]\[[0-9]*,[0-9]*\]' | sed -E 's/.*\]\[[0-9]+,([0-9]+)\]/\1/')
echo "status bar bottom: ${sb:-unknown}"
[ -n "$top" ] || { echo "Phone settings button not found"; exit 1; }
if [ -n "$sb" ] && [ "$top" -lt "$sb" ]; then echo "FAIL: Phone settings (top $top) is under the status bar (bottom $sb)"; exit 1; fi
x=$(echo "$bounds" | sed -E 's/\[([0-9]+),([0-9]+)\]\[([0-9]+),([0-9]+)\]/\1 \3/' | awk '{print int(($1+$2)/2)}')
y=$(echo "$bounds" | sed -E 's/\[([0-9]+),([0-9]+)\]\[([0-9]+),([0-9]+)\]/\2 \4/' | awk '{print int(($1+$2)/2)}')
adb shell input tap "$x" "$y"
sleep 4
adb exec-out screencap -p > shots/2-settings.png
adb shell uiautomator dump /sdcard/ui.xml >/dev/null && adb pull /sdcard/ui.xml shots/2-settings.xml >/dev/null
grep -q 'text="Settings"' shots/2-settings.xml || { echo "FAIL: Phone settings did not open"; exit 1; }
echo "edge-to-edge check passed: header below the status bar, settings open"

echo "server status as seen after the phone's sync:"
curl -s "$server/api/status" | jq '{devices, channels: [.channels[] | select(.state != "not_set_up")]}'
echo "smoke test passed: the phone reached the server"
