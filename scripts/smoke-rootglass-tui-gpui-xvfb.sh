#!/usr/bin/env bash
set -euo pipefail

binary="${1:?usage: smoke-rootglass-tui-gpui-xvfb.sh BINARY OUTPUT.png}"
screenshot="${2:?usage: smoke-rootglass-tui-gpui-xvfb.sh BINARY OUTPUT.png}"
work_dir="$(mktemp -d)"
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
  rm -rf "$work_dir"
}
trap cleanup EXIT

mkdir -p "$(dirname "$screenshot")"
mkdir -p "$work_dir/home" "$work_dir/config/rootglass" "$work_dir/data"
cat >"$work_dir/config/rootglass/trust.json" <<'JSON'
{
  "schemaVersion": 1,
  "trustedPublishers": [],
  "revokedKeyIds": []
}
JSON
export HOME="$work_dir/home"
export XDG_CONFIG_HOME="$work_dir/config"
export XDG_DATA_HOME="$work_dir/data"
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
wait_for_marker ROOTGLASS_EMBEDDED_TUI_COPIED
copied_text="$(timeout 5 xclip -selection clipboard -target UTF8_STRING -out)"
if [[ -z "$copied_text" ]]; then
  echo "Terminal selection copy left the clipboard empty" >&2
  exit 1
fi

xdotool key --clearmodifiers o
xdotool type --clearmodifiers --delay 10 "headless-key"
wait_for_marker ROOTGLASS_EMBEDDED_TUI_TYPED
xdotool key --clearmodifiers --repeat 12 BackSpace
printf 'headless paste' | xclip -selection clipboard -in
sleep 0.2
clipboard_text="$(timeout 5 xclip -selection clipboard -target UTF8_STRING -out)"
if [[ "$clipboard_text" != "headless paste" ]]; then
  echo "Clipboard did not retain the expected paste text: $clipboard_text" >&2
  exit 1
fi
xdotool key --clearmodifiers ctrl+v
wait_for_marker ROOTGLASS_EMBEDDED_TUI_PASTE_KEY
wait_for_marker ROOTGLASS_EMBEDDED_TUI_CLIPBOARD_READ
wait_for_marker ROOTGLASS_EMBEDDED_TUI_PASTE_SENT
wait_for_marker ROOTGLASS_EMBEDDED_TUI_PASTE

xdotool windowsize --sync "$window_id" 960 640
wait_for_marker ROOTGLASS_EMBEDDED_TUI_RESIZED
sleep 1
import -window "$window_id" "$screenshot"
test -s "$screenshot"
identify -format 'Captured embedded Rootglass TUI: %wx%h %m\n' "$screenshot"

# Quit from the embedded terminal surface. GPUI shutdown drops the terminal,
# whose PTY cleanup sends Ctrl+Q to the private TUI and reaps its child.
# GPUI may close X as soon as it handles the quit shortcut, before xdotool
# finishes releasing the key. The app-exit and PTY-cleanup markers below are
# the authoritative result for this interaction.
xdotool key ctrl+q || true
wait_for_marker ROOTGLASS_EMBEDDED_TUI_APP_QUIT
for _ in $(seq 1 450); do
  process_state="$(ps -o stat= -p "$app_pid" 2>/dev/null || true)"
  process_state="${process_state//[[:space:]]/}"
  process_state="${process_state:0:1}"
  if [[ -z "$process_state" || "$process_state" == Z ]]; then
    break
  fi
  sleep 0.1
done
process_state="$(ps -o stat= -p "$app_pid" 2>/dev/null || true)"
process_state="${process_state//[[:space:]]/}"
process_state="${process_state:0:1}"
if [[ -n "$process_state" && "$process_state" != Z ]]; then
  cat "$log_file"
  echo "Rootglass desktop did not shut down within 45 seconds" >&2
  exit 1
fi
if wait "$app_pid"; then
  app_exit_status=0
else
  app_exit_status=$?
fi
app_pid=""
cat "$log_file"
echo "ROOTGLASS_GPUI_APP_EXIT_STATUS $app_exit_status"
if [[ "$app_exit_status" -ne 0 ]]; then
  echo "Rootglass desktop exited with status $app_exit_status after the quit shortcut" >&2
  exit 1
fi
grep -Fq ROOTGLASS_EMBEDDED_TUI_CLEANED "$log_file"
