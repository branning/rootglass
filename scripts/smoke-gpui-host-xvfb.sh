#!/usr/bin/env bash
set -euo pipefail

binary="${1:?usage: smoke-gpui-host-xvfb.sh BINARY OUTPUT.png}"
screenshot="${2:?usage: smoke-gpui-host-xvfb.sh BINARY OUTPUT.png}"
log_file="$(mktemp)"
app_pid=""

wait_for_marker() {
  local marker="$1"
  for _ in $(seq 1 100); do
    if grep -Fq "$marker" "$log_file"; then
      return 0
    fi
    if ! kill -0 "$app_pid" 2>/dev/null; then
      cat "$log_file"
      echo "GPUI host exited while waiting for $marker" >&2
      exit 1
    fi
    sleep 0.1
  done
  cat "$log_file"
  echo "Timed out waiting for $marker" >&2
  exit 1
}

wait_for_new_marker() {
  local marker="$1"
  local previous_count="$2"
  for _ in $(seq 1 100); do
    local marker_count
    marker_count="$(grep -Fc "$marker" "$log_file" || true)"
    if [[ "$marker_count" =~ ^[0-9]+$ ]] && (( marker_count > previous_count )); then
      return 0
    fi
    if ! kill -0 "$app_pid" 2>/dev/null; then
      cat "$log_file"
      echo "GPUI host exited while waiting for a new $marker" >&2
      exit 1
    fi
    sleep 0.1
  done
  cat "$log_file"
  echo "Timed out waiting for a new $marker" >&2
  exit 1
}

cleanup() {
  if [[ -n "$app_pid" ]]; then
    if kill -0 "$app_pid" 2>/dev/null; then
      kill -- "-$app_pid" 2>/dev/null || kill "$app_pid" 2>/dev/null || true
    fi
    wait "$app_pid" 2>/dev/null || true
  fi
  rm -f "$log_file"
}
trap cleanup EXIT

mkdir -p "$(dirname "$screenshot")"
setsid env ROOTGLASS_GPUI_SMOKE=1 "$binary" >"$log_file" 2>&1 &
app_pid=$!

wait_for_marker ROOTGLASS_GPUI_HOST_READY

window_id=""
for _ in $(seq 1 50); do
  window_id="$(xdotool search --onlyvisible --pid "$app_pid" 2>/dev/null | head -n 1 || true)"
  if [[ -n "$window_id" ]]; then
    break
  fi
  sleep 0.1
done
if [[ -z "$window_id" ]]; then
  echo "Could not find the GPUI host window under Xvfb" >&2
  exit 1
fi

xdotool windowfocus --sync "$window_id"
xdotool key --clearmodifiers Left
xdotool type --clearmodifiers --delay 10 "headless-key"
wait_for_marker ROOTGLASS_GPUI_TYPED_READY

xdotool key --clearmodifiers ctrl+shift+c
copied_text="$(timeout 5 xclip -selection clipboard -out)"
if [[ "$copied_text" != *"INPUT:headless-key"* ]]; then
  printf 'Clipboard copy did not contain the visible terminal screen: %s\n' "$copied_text" >&2
  exit 1
fi

printf 'headless paste' | xclip -selection clipboard -in
sleep 0.2
xdotool key --clearmodifiers ctrl+v
wait_for_marker ROOTGLASS_GPUI_PASTE_READY

resize_markers_before="$(grep -Fc ROOTGLASS_GPUI_RESIZE_READY "$log_file" || true)"
resize_markers_before="${resize_markers_before:-0}"
xdotool windowsize --sync "$window_id" 960 640
wait_for_new_marker ROOTGLASS_GPUI_RESIZE_READY "$resize_markers_before"

sleep 1
import -window root "$screenshot"
test -s "$screenshot"
identify -format 'Captured GPUI Xvfb screenshot: %wx%h %m\n' "$screenshot"
wait "$app_pid"
app_pid=""
cat "$log_file"
grep -Fq ROOTGLASS_GPUI_PTY_CLEANED "$log_file"
