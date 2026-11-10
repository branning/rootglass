#!/usr/bin/env bash
set -euo pipefail

rootglass_binary="$(realpath "${1:?usage: capture-rootglass-library-xvfb.sh ROOTGLASS_BINARY OUTPUT_DIR}")"
artifact_dir="${2:?usage: capture-rootglass-library-xvfb.sh ROOTGLASS_BINARY OUTPUT_DIR}"
mkdir -p "$artifact_dir"
artifact_dir="$(cd "$artifact_dir" && pwd)"
work_dir="$(mktemp -d)"
log_file="${artifact_dir}/desktop-smoke.log"
event_file="${artifact_dir}/desktop-smoke-events.log"
app_pid=""
window_id=""

cleanup() {
  if [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; then
    if [[ -n "$window_id" ]]; then
      xdotool key --clearmodifiers alt+F4 2>/dev/null || true
    fi
    kill -- "-$app_pid" 2>/dev/null || kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
  fi
  chmod -R u+w "$work_dir" 2>/dev/null || true
  rm -rf "$work_dir"
}
trap cleanup EXIT

mkdir -p "$work_dir/home" "$work_dir/config" "$work_dir/data" "$work_dir/runtime"
chmod 700 "$work_dir/runtime"
export HOME="${work_dir}/home"
export XDG_CONFIG_HOME="${work_dir}/config"
export XDG_DATA_HOME="${work_dir}/data"
export XDG_RUNTIME_DIR="${work_dir}/runtime"
export XDG_CURRENT_DESKTOP=GNOME
export GDK_BACKEND=x11
export ROOTGLASS_DESKTOP_SMOKE=1
export ROOTGLASS_DESKTOP_SMOKE_EVENT_LOG="$event_file"

setsid bash -c 'cd "$1"; exec "$2"' _ "$work_dir" "$rootglass_binary" >"$log_file" 2>&1 &
app_pid=$!

for _ in $(seq 1 300); do
  if [[ -f "$event_file" ]] && grep -Fq ROOTGLASS_DESKTOP_WINDOW_READY "$event_file"; then
    break
  fi
  if ! kill -0 "$app_pid" 2>/dev/null; then
    cat "$log_file" "$event_file" 2>/dev/null || true
    echo "Rootglass exited before its window opened" >&2
    exit 1
  fi
  sleep 0.1
done
if [[ ! -f "$event_file" ]] || ! grep -Fq ROOTGLASS_DESKTOP_WINDOW_READY "$event_file"; then
  cat "$log_file" "$event_file" 2>/dev/null || true
  echo "Timed out waiting for the Rootglass window" >&2
  exit 1
fi

for _ in $(seq 1 100); do
  window_id="$(xdotool search --onlyvisible --pid "$app_pid" 2>/dev/null | head -n 1 || true)"
  if [[ -n "$window_id" ]]; then
    break
  fi
  sleep 0.1
done
if [[ -z "$window_id" ]]; then
  cat "$log_file" "$event_file" 2>/dev/null || true
  echo "Could not find the Rootglass GPUI window under Xvfb" >&2
  exit 1
fi

xdotool windowfocus --sync "$window_id"
sleep 1
import -window "$window_id" "${artifact_dir}/00-empty-library.png"
identify -format '%f: %wx%h %m\n' "${artifact_dir}/00-empty-library.png"
xdotool key --clearmodifiers alt+F4
for _ in $(seq 1 100); do
  app_state="$(ps -o stat= -p "$app_pid" 2>/dev/null | tr -d ' ' || true)"
  if [[ -z "$app_state" || "$app_state" == Z* ]]; then
    break
  fi
  sleep 0.1
done
if [[ -n "$app_state" && "$app_state" != Z* ]]; then
  echo "Rootglass did not exit after its Xvfb window closed" >&2
  exit 1
fi
wait "$app_pid"
app_pid=""
cat "$log_file" "$event_file" 2>/dev/null || true
