#!/usr/bin/env bash
set -euo pipefail

binary="${1:?usage: smoke-rootglass-tui-gpui-xvfb.sh BINARY OUTPUT.png}"
screenshot="${2:?usage: smoke-rootglass-tui-gpui-xvfb.sh BINARY OUTPUT.png}"
log_file="$(mktemp)"
app_pid=""

wait_for_marker() {
  local marker="$1"
  for _ in $(seq 1 300); do
    if grep -Fq "$marker" "$log_file"; then
      return 0
    fi
    if ! kill -0 "$app_pid" 2>/dev/null; then
      cat "$log_file"
      echo "Rootglass desktop exited while waiting for $marker" >&2
      exit 1
    fi
    sleep 0.1
  done
  cat "$log_file"
  echo "Timed out waiting for $marker" >&2
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
wait_for_marker ROOTGLASS_EMBEDDED_TUI_READY
wait_for_marker ROOTGLASS_EMBEDDED_TUI_ANSI

window_id=""
for _ in $(seq 1 50); do
  window_id="$(xdotool search --onlyvisible --pid "$app_pid" 2>/dev/null | head -n 1 || true)"
  if [[ -n "$window_id" ]]; then
    break
  fi
  sleep 0.1
done
if [[ -z "$window_id" ]]; then
  echo "Could not find the Rootglass GPUI window under Xvfb" >&2
  exit 1
fi

xdotool windowfocus --sync "$window_id"
for _ in $(seq 1 5); do
  xdotool key --clearmodifiers shift+Right
done
wait_for_marker ROOTGLASS_EMBEDDED_TUI_SELECTION
xdotool key --clearmodifiers ctrl+shift+c
copied_text="$(timeout 5 xclip -selection clipboard -out)"
if [[ -z "$copied_text" ]]; then
  echo "Terminal selection copy left the clipboard empty" >&2
  exit 1
fi

xdotool key --clearmodifiers o
xdotool type --clearmodifiers --delay 10 "headless-key"
wait_for_marker ROOTGLASS_EMBEDDED_TUI_TYPED
xdotool key --clearmodifiers Escape
xdotool key --clearmodifiers o
printf 'headless paste' | xclip -selection clipboard -in
sleep 0.2
xdotool key --clearmodifiers ctrl+v
wait_for_marker ROOTGLASS_EMBEDDED_TUI_PASTE

xdotool windowsize --sync "$window_id" 960 640
wait_for_marker ROOTGLASS_EMBEDDED_TUI_RESIZED
sleep 1
import -window "$window_id" "$screenshot"
test -s "$screenshot"
identify -format 'Captured embedded Rootglass TUI: %wx%h %m\n' "$screenshot"

# Closing the GPUI window requests Ctrl+Q in the private PTY. The TUI then
# stops any owned guest before the session process is reaped.
xdotool windowclose "$window_id"
wait "$app_pid"
app_pid=""
cat "$log_file"
grep -Fq ROOTGLASS_EMBEDDED_TUI_CLEANED "$log_file"
