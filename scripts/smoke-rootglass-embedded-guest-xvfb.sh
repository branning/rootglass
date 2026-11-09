#!/usr/bin/env bash
set -euo pipefail

binary="$(realpath "${1:?usage: smoke-rootglass-embedded-guest-xvfb.sh ROOTGLASS_BINARY OUTPUT_DIR}")"
artifact_dir="${2:?usage: smoke-rootglass-embedded-guest-xvfb.sh ROOTGLASS_BINARY OUTPUT_DIR}"
mkdir -p "$artifact_dir"
artifact_dir="$(cd "$artifact_dir" && pwd)"
repo_root="$(git rev-parse --show-toplevel)"
work_dir="$(mktemp -d)"
log_file="$work_dir/embedded-guest.log"
app_pid=""
window_id=""

wait_for_marker() {
  local marker="$1"
  local timeout_seconds="${2:-90}"
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
  if [[ -n "$window_id" ]]; then
    import -window "$window_id" "$artifact_dir/timeout-${marker}.png" \
      >/dev/null 2>&1 || true
  fi
  cat "$log_file"
  echo "Timed out after ${timeout_seconds}s waiting for $marker" >&2
  exit 1
}

wait_for_trace_marker() {
  local marker="$1"
  local timeout_seconds="${2:-30}"
  for _ in $(seq 1 "$((timeout_seconds * 10))"); do
    if grep -Fq "$marker" "$work_dir/tui-trace.log" 2>/dev/null; then
      return 0
    fi
    if [[ -n "$app_pid" ]] && ! kill -0 "$app_pid" 2>/dev/null; then
      cat "$log_file"
      cat "$work_dir/tui-trace.log" 2>/dev/null || true
      echo "Rootglass desktop exited while waiting for TUI trace: $marker" >&2
      exit 1
    fi
    sleep 0.1
  done
  if [[ -n "$window_id" ]]; then
    import -window "$window_id" "$artifact_dir/timeout-trace-${marker//[^A-Za-z0-9_-]/_}.png" \
      >/dev/null 2>&1 || true
  fi
  cat "$log_file"
  cat "$work_dir/tui-trace.log" 2>/dev/null || true
  echo "Timed out after ${timeout_seconds}s waiting for TUI trace: $marker" >&2
  exit 1
}

wait_for_trace_count() {
  local marker="$1"
  local expected_count="$2"
  local timeout_seconds="${3:-30}"
  local count="0"
  for _ in $(seq 1 "$((timeout_seconds * 10))"); do
    count="$(grep -Fc "$marker" "$work_dir/tui-trace.log" 2>/dev/null || true)"
    count="${count//[[:space:]]/}"
    if (( ${count:-0} >= expected_count )); then
      return 0
    fi
    if [[ -n "$app_pid" ]] && ! kill -0 "$app_pid" 2>/dev/null; then
      cat "$log_file"
      cat "$work_dir/tui-trace.log" 2>/dev/null || true
      echo "Rootglass desktop exited while waiting for TUI trace count: $marker" >&2
      exit 1
    fi
    sleep 0.1
  done
  if [[ -n "$window_id" ]]; then
    import -window "$window_id" "$artifact_dir/timeout-trace-count-${marker//[^A-Za-z0-9_-]/_}.png" \
      >/dev/null 2>&1 || true
  fi
  cat "$log_file"
  cat "$work_dir/tui-trace.log" 2>/dev/null || true
  echo "Timed out after ${timeout_seconds}s waiting for TUI trace count $expected_count: $marker" >&2
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
    cp "$log_file" "$artifact_dir/embedded-guest.log"
  fi
  if [[ -f "$work_dir/tui-trace.log" ]]; then
    cp "$work_dir/tui-trace.log" "$artifact_dir/tui-trace.log"
  fi
  if [[ -d "$work_dir/tmp" ]]; then
    find "$work_dir/tmp" -maxdepth 1 -type f -name 'rootglass-qemu-*.log' \
      -exec cp {} "$artifact_dir/" \; 2>/dev/null || true
  fi
  chmod -R u+w "$work_dir" 2>/dev/null || true
  rm -rf "$work_dir"
}
trap cleanup EXIT

mkdir -p "$work_dir/home" "$work_dir/config/rootglass" "$work_dir/data" \
  "$work_dir/runtime" "$work_dir/tmp"
chmod 700 "$work_dir/runtime"
glass_dir="$work_dir/hello-world.glass"
cp -a "$repo_root/examples/hello-world.glass" "$glass_dir"

# Keep the mostly empty state disk below the importer's compression-ratio limit.
python3 - "$work_dir/state-ratio-guard.bin" <<'PY'
import hashlib
import sys

with open(sys.argv[1], "wb") as payload:
    for counter in range(32768):
        payload.write(hashlib.sha256(b"rootglass embedded guest smoke" + counter.to_bytes(8, "little")).digest())
PY
debugfs -w -R "write $work_dir/state-ratio-guard.bin /.rootglass-embedded-ratio-guard.bin" \
  "$glass_dir/state.img" >/dev/null

trusted_key="$work_dir/trusted-seed"
python3 - "$trusted_key" "$work_dir/config/rootglass/trust.json" <<'PY'
import hashlib
import json
import os
import sys

key_file, trust_file = sys.argv[1:]
seed = hashlib.sha256(b"Rootglass public development fixture signing key v1").digest()
with open(key_file, "wb") as output:
    output.write(seed)
os.chmod(key_file, 0o600)
trust = {
    "schemaVersion": 1,
    "trustedPublishers": [
        {
            "name": "Rootglass Examples",
            "identity": "rootglass.dev/examples",
            "keyId": "ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0",
            "publicKeyBase64url": "2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw",
        }
    ],
    "revokedKeyIds": [],
}
with open(trust_file, "w", encoding="utf-8") as output:
    json.dump(trust, output, indent=2)
    output.write("\n")
PY

sample="$work_dir/hello-world.sample.glass"
"$binary" package "$glass_dir" "$sample" --key "$trusted_key" \
  --created-at 2026-11-09T00:00:00Z

export HOME="$work_dir/home"
export XDG_CONFIG_HOME="$work_dir/config"
export XDG_DATA_HOME="$work_dir/data"
export XDG_RUNTIME_DIR="$work_dir/runtime"
export TMPDIR="$work_dir/tmp"
export XDG_CURRENT_DESKTOP=GNOME
export GDK_BACKEND=x11
export ROOTGLASS_TUI_TRACE_FILE="$work_dir/tui-trace.log"
setsid env ROOTGLASS_GPUI_SMOKE=1 "$binary" >"$log_file" 2>&1 &
app_pid=$!
wait_for_marker ROOTGLASS_EMBEDDED_TUI_READY 60
wait_for_marker ROOTGLASS_EMBEDDED_TUI_ANSI 30

for _ in $(seq 1 100); do
  window_id="$(xdotool search --onlyvisible --pid "$app_pid" 2>/dev/null | head -n 1 || true)"
  [[ -z "$window_id" ]] || break
  sleep 0.1
done
if [[ -z "$window_id" ]]; then
  cat "$log_file"
  echo "Could not find the Rootglass GPUI window under Xvfb" >&2
  exit 1
fi
xdotool windowfocus --sync "$window_id"

# A window-level focus can land before the embedded terminal has received a
# key event. Exercise terminal selection first, matching the host interaction
# smoke, so the first workflow shortcut cannot be swallowed while focus settles.
for _ in $(seq 1 5); do
  xdotool key --clearmodifiers shift+Right
done
wait_for_marker ROOTGLASS_EMBEDDED_TUI_SELECTION 30

# Open the signed sample through the embedded TUI and boot a real guest.
xdotool key --clearmodifiers o
wait_for_marker ROOTGLASS_EMBEDDED_TUI_OPEN_PATH_PROMPT 15
xdotool type --clearmodifiers --delay 5 "$sample"
wait_for_marker ROOTGLASS_EMBEDDED_TUI_OPEN_PATH_TYPED 15
xdotool key --clearmodifiers Return
wait_for_trace_marker "imported bundle inspection completed: outcome=ready" 180
wait_for_marker ROOTGLASS_EMBEDDED_TUI_BUNDLE_READY 30
state_refresh_marker="state completion: operation=refresh, successful=true, applied=true"
wait_for_trace_count "$state_refresh_marker" 1 30
xdotool key --clearmodifiers v
wait_for_trace_marker "validate completion: successful=true, applied=true" 30
wait_for_marker ROOTGLASS_EMBEDDED_TUI_MANIFEST_VALID 30
xdotool key --clearmodifiers p
wait_for_marker ROOTGLASS_EMBEDDED_TUI_LAUNCH_PLAN 60

# Exercise state creation and trust diagnostics in the same TUI session.
state_refreshes_before="$(grep -Fc "$state_refresh_marker" "$work_dir/tui-trace.log" 2>/dev/null || true)"
state_refreshes_before="${state_refreshes_before//[[:space:]]/}"
xdotool key --clearmodifiers t
wait_for_marker ROOTGLASS_EMBEDDED_TUI_STATE_TRUST 30
wait_for_trace_count "$state_refresh_marker" "$(( ${state_refreshes_before:-0} + 1 ))" 30
xdotool key --clearmodifiers c
wait_for_trace_marker "state create prompt opened" 15
xdotool type --clearmodifiers --delay 10 embedded-smoke
xdotool key --clearmodifiers Return
wait_for_marker ROOTGLASS_EMBEDDED_TUI_STATE_CREATED 30
xdotool key --clearmodifiers t

# Sign and package through the TUI using only the external key-file path.
developer_package="$work_dir/embedded-ui-package.glass"
xdotool type --clearmodifiers --delay 5 D
xdotool key --clearmodifiers s
xdotool type --clearmodifiers --delay 5 "$glass_dir"
xdotool key --clearmodifiers Return
xdotool type --clearmodifiers --delay 5 "$trusted_key"
xdotool key --clearmodifiers Return
xdotool key --clearmodifiers Return
wait_for_marker ROOTGLASS_EMBEDDED_TUI_DEVELOPER_SIGNED 60
xdotool key --clearmodifiers Escape
xdotool type --clearmodifiers --delay 5 D
xdotool key --clearmodifiers p
xdotool type --clearmodifiers --delay 5 "$glass_dir"
xdotool key --clearmodifiers Return
xdotool type --clearmodifiers --delay 5 "$trusted_key"
xdotool key --clearmodifiers Return
xdotool key --clearmodifiers Return
xdotool type --clearmodifiers --delay 5 "$developer_package"
xdotool key --clearmodifiers Return
wait_for_marker ROOTGLASS_EMBEDDED_TUI_DEVELOPER_PACKAGED 60
test -s "$developer_package"
xdotool key --clearmodifiers Escape

xdotool key --clearmodifiers b
wait_for_marker ROOTGLASS_EMBEDDED_TUI_GUEST_LOGIN 180
import -window "$window_id" "$artifact_dir/01-guest-login.png"

# Authenticate through the TUI's serial input and run uname in the guest.
xdotool key --clearmodifiers Return
xdotool type --clearmodifiers --delay 25 root
xdotool key --clearmodifiers Return
wait_for_marker ROOTGLASS_EMBEDDED_TUI_GUEST_PASSWORD 30
xdotool key --clearmodifiers Return
xdotool type --clearmodifiers --delay 25 rootglass-ci
xdotool key --clearmodifiers Return
wait_for_marker ROOTGLASS_EMBEDDED_TUI_GUEST_SHELL 30
xdotool key --clearmodifiers Return
xdotool type --clearmodifiers --delay 2 \
  "printf 'ROOTGLASS_EMBEDDED_GUEST_UNAME_BEGIN\\n'; uname -a; printf 'ROOTGLASS_EMBEDDED_GUEST_UNAME_DONE\\n'"
xdotool key --clearmodifiers Return
wait_for_marker ROOTGLASS_EMBEDDED_TUI_GUEST_UNAME 45
import -window "$window_id" "$artifact_dir/02-guest-uname.png"

# Restart through the embedded TUI and wait for its second boot-ready state.
xdotool key --clearmodifiers r
wait_for_marker ROOTGLASS_EMBEDDED_TUI_GUEST_RESTART_STARTED 60
wait_for_marker ROOTGLASS_EMBEDDED_TUI_GUEST_RESTART_RUNNING 120
for _ in $(seq 1 100); do
  qemu_log_count="$(find "$work_dir/tmp" -maxdepth 1 -type f -name 'rootglass-qemu-*.log' | wc -l)"
  ((qemu_log_count >= 2)) && break
  sleep 0.1
done
if ((qemu_log_count < 2)); then
  echo "Restart did not create a second QEMU log" >&2
  exit 1
fi
import -window "$window_id" "$artifact_dir/03-guest-restarted.png"

xdotool key --clearmodifiers s
wait_for_marker ROOTGLASS_EMBEDDED_TUI_GUEST_STOPPED 60
import -window "$window_id" "$artifact_dir/04-guest-stopped.png"

qemu_pids="$(pgrep -f '^/usr/bin/qemu-system-aarch64' || true)"
if [[ -n "$qemu_pids" ]]; then
  while IFS= read -r qemu_pid; do
    [[ -z "$qemu_pid" ]] || ps -fp "$qemu_pid"
  done <<< "$qemu_pids"
  echo "A QEMU child remained after Stop in the embedded TUI" >&2
  exit 1
fi

xdotool key --clearmodifiers ctrl+q || true
wait_for_marker ROOTGLASS_EMBEDDED_TUI_APP_QUIT 30
for _ in $(seq 1 300); do
  app_state="$(ps -o stat= -p "$app_pid" 2>/dev/null | tr -d ' ' || true)"
  [[ -z "$app_state" || "$app_state" == Z* ]] && break
  sleep 0.1
done
if [[ -n "$app_state" && "$app_state" != Z* ]]; then
  cat "$log_file"
  echo "Rootglass did not exit after the embedded TUI quit shortcut" >&2
  exit 1
fi
wait "$app_pid"
app_pid=""
grep -Fq ROOTGLASS_EMBEDDED_TUI_CLEANED "$log_file"
if pgrep -f '^/usr/bin/qemu-system-aarch64'; then
  echo "A QEMU child remained after closing Rootglass" >&2
  exit 1
fi
cat "$log_file"
for image in "$artifact_dir"/*.png; do
  identify -format '%f: %wx%h %m\n' "$image"
done
