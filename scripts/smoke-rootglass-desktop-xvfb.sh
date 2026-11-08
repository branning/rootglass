#!/usr/bin/env bash
set -euo pipefail

rootglass_binary="$(realpath "${1:?usage: smoke-rootglass-desktop-xvfb.sh ROOTGLASS_BINARY OUTPUT_DIR}")"
desktop_binary="$rootglass_binary"
cli_binary="$rootglass_binary"
artifact_dir="${2:?usage: smoke-rootglass-desktop-xvfb.sh ROOTGLASS_BINARY OUTPUT_DIR}"
mkdir -p "$artifact_dir"
artifact_dir="$(cd "$artifact_dir" && pwd)"
repo_root="$(git rev-parse --show-toplevel)"
work_dir="$(mktemp -d)"
log_file="${work_dir}/desktop.log"
app_pid=""
window_id=""
sentinel="Password:"
serial_username="root"

wait_for_marker() {
  local marker="$1"
  local timeout_seconds="${2:-45}"
  for _ in $(seq 1 "$((timeout_seconds * 10))"); do
    if grep -Fq "$marker" "$log_file"; then
      return 0
    fi
    if [[ -n "$app_pid" ]] && ! kill -0 "$app_pid" 2>/dev/null; then
      cat "$log_file"
      echo "Rootglass desktop exited while waiting for $marker" >&2
      exit 1
    fi
    sleep 0.1
  done
  cat "$log_file"
  echo "Timed out after ${timeout_seconds}s waiting for $marker" >&2
  exit 1
}

wait_for_count() {
  local marker="$1"
  local expected="$2"
  local timeout_seconds="${3:-90}"
  for _ in $(seq 1 "$((timeout_seconds * 10))"); do
    local count
    count="$(grep -Fc "$marker" "$log_file" || true)"
    if ((count >= expected)); then
      return 0
    fi
    if [[ -n "$app_pid" ]] && ! kill -0 "$app_pid" 2>/dev/null; then
      cat "$log_file"
      echo "Rootglass desktop exited while waiting for $expected occurrences of $marker" >&2
      exit 1
    fi
    sleep 0.1
  done
  cat "$log_file"
  echo "Timed out waiting for $expected occurrences of $marker" >&2
  exit 1
}

cleanup() {
  if [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; then
    if [[ -n "$window_id" ]]; then
      xdotool windowclose "$window_id" 2>/dev/null || true
    fi
    kill -- "-$app_pid" 2>/dev/null || kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
  fi
  if [[ -f "$log_file" ]]; then
    cp "$log_file" "${artifact_dir}/desktop-smoke.log"
  fi
  chmod -R u+w "$work_dir" 2>/dev/null || true
  rm -rf "$work_dir"
}
trap cleanup EXIT

mkdir -p "$artifact_dir" "${work_dir}/home" "${work_dir}/config/rootglass" \
  "${work_dir}/data" "${work_dir}/runtime"
chmod 700 "${work_dir}/runtime"

verified_dir="${work_dir}/verified.glass"
trusted_key="${work_dir}/trusted-seed"
untrusted_key="${work_dir}/untrusted-seed"
trust_config="${work_dir}/config/rootglass/trust.json"
verified_archive="${work_dir}/verified-package.glass"
untrusted_archive="${work_dir}/untrusted.glass"
tampered_archive="${work_dir}/tampered.glass"

cp -a "${repo_root}/examples/hello-world.glass" "$verified_dir"
# Keep the generated ext4 state disk below the importer's 200:1 archive
# compression-ratio limit. The fixture disk is mostly zero-filled, so add a
# deterministic high-entropy guest file before signing the temporary package.
entropy_file="${work_dir}/state-ratio-guard.bin"
python3 - "$entropy_file" <<'PY'
import hashlib
import sys

with open(sys.argv[1], "wb") as payload:
    for counter in range(32768):
        payload.write(hashlib.sha256(b"rootglass desktop smoke state" + counter.to_bytes(8, "little")).digest())
PY
debugfs -w -R "write ${entropy_file} /rootglass-smoke-ratio-guard.bin" \
  "${verified_dir}/state.img" >/dev/null
python3 - "$trusted_key" "$untrusted_key" "$trust_config" <<'PY'
import base64
import hashlib
import json
import os
import sys

trusted_key, untrusted_key, trust_config = sys.argv[1:]
trusted_seed = hashlib.sha256(
    b"Rootglass public development fixture signing key v1"
).digest()
untrusted_seed = hashlib.sha256(b"Rootglass desktop Xvfb untrusted key v1").digest()
for path, seed in ((trusted_key, trusted_seed), (untrusted_key, untrusted_seed)):
    with open(path, "wb") as key_file:
        key_file.write(seed)
    os.chmod(path, 0o600)

public_key = "2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw"
public_bytes = base64.urlsafe_b64decode(public_key + "=" * (-len(public_key) % 4))
config = {
    "schemaVersion": 1,
    "trustedPublishers": [
        {
            "name": "Rootglass Examples",
            "identity": "rootglass.dev/examples",
            "keyId": "ed25519:" + hashlib.sha256(public_bytes).hexdigest(),
            "publicKeyBase64url": public_key,
        }
    ],
    "revokedKeyIds": [],
}
with open(trust_config, "w", encoding="utf-8") as config_file:
    json.dump(config, config_file, indent=2)
    config_file.write("\n")
PY

"$cli_binary" sign "$verified_dir" --key "$trusted_key" --created-at 2026-11-08T00:00:00Z
"$cli_binary" package "$verified_dir" "$verified_archive" \
  --key "$trusted_key" --created-at 2026-11-08T00:00:00Z
"$cli_binary" package "$verified_dir" "$untrusted_archive" \
  --key "$untrusted_key" --created-at 2026-11-08T00:00:00Z
python3 - "$verified_archive" "$tampered_archive" <<'PY'
from zipfile import ZipFile
import sys

source, destination = sys.argv[1:]
with ZipFile(source) as archive, ZipFile(destination, "w") as output:
    for entry in archive.infolist():
        data = archive.read(entry.filename)
        if entry.filename == "README.md":
            data += b"\nmodified after signing\n"
        output.writestr(entry, data)
PY

export ROOTGLASS_DESKTOP_SMOKE=1
export ROOTGLASS_DESKTOP_SMOKE_SERIAL_SENTINEL="$sentinel"
export HOME="${work_dir}/home"
export XDG_CONFIG_HOME="${work_dir}/config"
export XDG_DATA_HOME="${work_dir}/data"
export XDG_RUNTIME_DIR="${work_dir}/runtime"
export XDG_CURRENT_DESKTOP=GNOME
export GDK_BACKEND=x11

setsid "$desktop_binary" >"$log_file" 2>&1 &
app_pid=$!
wait_for_marker ROOTGLASS_DESKTOP_WINDOW_READY

for _ in $(seq 1 100); do
  window_id="$(xdotool search --onlyvisible --pid "$app_pid" 2>/dev/null | head -n 1 || true)"
  if [[ -n "$window_id" ]]; then
    break
  fi
  sleep 0.1
done
if [[ -z "$window_id" ]]; then
  cat "$log_file"
  echo "Could not find the Rootglass GPUI window under Xvfb" >&2
  exit 1
fi
xdotool windowfocus --sync "$window_id"

# Confirm the real native chooser opens and returns a user-cancel result.
# The toolbar path works consistently across platforms and avoids assuming a
# platform-specific modifier for the app's cmd-o shortcut.
xdotool mousemove --window "$window_id" 170 26 click 1
wait_for_marker ROOTGLASS_DESKTOP_PICKER_OPEN
dialog_window=""
for _ in $(seq 1 100); do
  visible_windows="$(xdotool search --onlyvisible --name '.*' 2>/dev/null || true)"
  while IFS= read -r candidate; do
    [[ -n "$candidate" ]] || continue
    [[ "$candidate" != "$window_id" ]] || continue
    candidate_name="$(xdotool getwindowname "$candidate" 2>/dev/null || true)"
    candidate_pid="$(xdotool getwindowpid "$candidate" 2>/dev/null || true)"
    candidate_geometry="$(xdotool getwindowgeometry --shell "$candidate" 2>/dev/null || true)"
    candidate_width="$(sed -n 's/^WIDTH=//p' <<<"$candidate_geometry")"
    candidate_height="$(sed -n 's/^HEIGHT=//p' <<<"$candidate_geometry")"
    printf 'ROOTGLASS_DESKTOP_VISIBLE_WINDOW id=%s pid=%s name=%s geometry=%s %s\n' \
      "$candidate" "$candidate_pid" "$candidate_name" "$candidate_width" "$candidate_height"
    if [[ "$candidate_name" == *"Open Folder"* || "$candidate_name" == *"Open File"* ]]; then
      dialog_window="$candidate"
      break
    fi
    if [[ "$candidate_width" =~ ^[0-9]+$ && "$candidate_height" =~ ^[0-9]+$ ]] \
      && ((candidate_width >= 320 && candidate_height >= 240)); then
      dialog_window="$candidate"
    fi
  done <<< "$visible_windows"
  if [[ -n "$dialog_window" ]] \
    && { [[ "$(xdotool getwindowname "$dialog_window" 2>/dev/null || true)" == *"Open Folder"* ]] \
      || [[ "$(xdotool getwindowname "$dialog_window" 2>/dev/null || true)" == *"Open File"* ]]; }; then
    break
  fi
  sleep 0.1
done
if [[ -z "$dialog_window" ]]; then
  xwininfo -root -tree || true
  xdotool search --onlyvisible --name '.*' 2>/dev/null || true
  echo "The native file chooser did not create a visible window" >&2
  exit 1
fi
printf 'ROOTGLASS_DESKTOP_DIALOG_WINDOW id=%s name=%s\n' \
  "$dialog_window" "$(xdotool getwindowname "$dialog_window" 2>/dev/null || true)"
xprop -id "$dialog_window" WM_NAME _NET_WM_NAME WM_CLASS _NET_WM_PID _NET_WM_WINDOW_TYPE WM_TRANSIENT_FOR || true
xwininfo -id "$dialog_window" || true
import -window "$dialog_window" "${artifact_dir}/00-native-file-chooser-window.png"
import -window root "${artifact_dir}/00-native-file-chooser.png"
xdotool windowraise "$dialog_window"
xdotool windowactivate --sync "$dialog_window" 2>/dev/null || xdotool windowfocus --sync "$dialog_window"
xdotool windowfocus --sync "$dialog_window"
printf 'ROOTGLASS_DESKTOP_DIALOG_FOCUS id=%s focused=%s\n' \
  "$dialog_window" "$(xdotool getwindowfocus 2>/dev/null || true)"
xdotool key --clearmodifiers --delay 100 Escape
for _ in $(seq 1 30); do
  if grep -Fq ROOTGLASS_DESKTOP_PICKER_CANCELLED "$log_file"; then
    break
  fi
  sleep 0.1
done
if ! grep -Fq ROOTGLASS_DESKTOP_PICKER_CANCELLED "$log_file"; then
  xdotool windowclose "$dialog_window" 2>/dev/null || true
fi
wait_for_marker ROOTGLASS_DESKTOP_PICKER_CANCELLED
xdotool windowfocus --sync "$window_id"

# A directory drop exercises direct inspection; archives exercise verified import.
xdotool mousemove --window "$window_id" 320 120
/usr/bin/python3 "${repo_root}/scripts/xdnd-drop-file.py" "$window_id" "$verified_dir"
wait_for_marker ROOTGLASS_DESKTOP_PREFLIGHT_READY
wait_for_marker ROOTGLASS_DESKTOP_STATE_READY
import -window "$window_id" "${artifact_dir}/01-verified-directory.png"

xdotool mousemove --window "$window_id" 320 120
/usr/bin/python3 "${repo_root}/scripts/xdnd-drop-file.py" "$window_id" "$untrusted_archive"
wait_for_marker ROOTGLASS_DESKTOP_INSPECT_UNTRUSTED
xdotool mousemove --window "$window_id" 320 120
/usr/bin/python3 "${repo_root}/scripts/xdnd-drop-file.py" "$window_id" "$tampered_archive"
wait_for_marker ROOTGLASS_DESKTOP_INSPECT_TAMPERED

xdotool mousemove --window "$window_id" 320 120
/usr/bin/python3 "${repo_root}/scripts/xdnd-drop-file.py" "$window_id" "$verified_archive"
wait_for_count ROOTGLASS_DESKTOP_INSPECT_VERIFIED 2
wait_for_count ROOTGLASS_DESKTOP_STATE_READY 2
import -window "$window_id" "${artifact_dir}/02-verified-import.png"

# Toolbar order is Open, Boot, Restart, Stop. Click Boot after verified preflight.
xdotool mousemove --window "$window_id" 285 26 click 1
wait_for_marker ROOTGLASS_DESKTOP_BOOT_STARTED 120
wait_for_marker ROOTGLASS_DESKTOP_GUEST_READY 120
import -window "$window_id" "${artifact_dir}/03-guest-running.png"

# Submit a username at the guest login prompt and wait for its password request.
window_geometry="$(xdotool getwindowgeometry --shell "$window_id")"
window_height="$(sed -n 's/^HEIGHT=//p' <<<"$window_geometry")"
input_y="$((window_height - 16))"
xdotool mousemove --window "$window_id" 180 "$input_y" click 1
xdotool type --clearmodifiers --delay 20 "$serial_username"
sleep 0.25
import -window "$window_id" "${artifact_dir}/04-guest-username-input.png"
xdotool key Return
wait_for_marker ROOTGLASS_DESKTOP_SERIAL_INPUT_SENT
wait_for_marker ROOTGLASS_DESKTOP_SERIAL_ECHO 45
echo "ROOTGLASS_DESKTOP_SMOKE_SERIAL_RESPONSE"
xdotool mousemove --window "$window_id" 900 "$((window_height - 100))"
xdotool click --repeat 20 --delay 25 5
sleep 0.5
import -window "$window_id" "${artifact_dir}/05-guest-password-prompt.png"

# Restart, wait for a second guest-ready event, then stop through the UI.
xdotool mousemove --window "$window_id" 365 26 click 1
wait_for_marker ROOTGLASS_DESKTOP_RESTART_STARTED 120
wait_for_count ROOTGLASS_DESKTOP_GUEST_READY 2 120
echo "ROOTGLASS_DESKTOP_SMOKE_RESTART_READY"
xdotool mousemove --window "$window_id" 425 26 click 1
wait_for_marker ROOTGLASS_DESKTOP_STOPPED 120
echo "ROOTGLASS_DESKTOP_SMOKE_STOPPED"
xdotool windowsize --sync "$window_id" 1000 650
sleep 1
import -window "$window_id" "${artifact_dir}/06-guest-stopped-resized.png"

xdotool key --clearmodifiers alt+F4
for _ in $(seq 1 300); do
  app_state="$(ps -o stat= -p "$app_pid" 2>/dev/null | tr -d ' ' || true)"
  if [[ -z "$app_state" || "$app_state" == Z* ]]; then
    break
  fi
  sleep 0.1
done
if [[ -n "$app_state" && "$app_state" != Z* ]]; then
  echo "Rootglass desktop did not exit within 30s after its window was closed" >&2
  exit 1
fi
wait "$app_pid"
app_pid=""
echo "ROOTGLASS_DESKTOP_SMOKE_WINDOW_EXITED"
if pgrep -af qemu-system-aarch64 | grep -F "$work_dir"; then
  echo "A QEMU child remained after the desktop closed" >&2
  exit 1
fi
cat "$log_file"
for image in "$artifact_dir"/*.png; do
  identify -format '%f: %wx%h %m\n' "$image"
done
