#!/usr/bin/env bash
set -euo pipefail

binary="${1:?usage: smoke-gpui-host-xvfb.sh BINARY OUTPUT.png}"
screenshot="${2:?usage: smoke-gpui-host-xvfb.sh BINARY OUTPUT.png}"
log_file="$(mktemp)"
app_pid=""

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

for _ in $(seq 1 90); do
  if grep -Fq ROOTGLASS_GPUI_HOST_READY "$log_file"; then
    break
  fi
  if ! kill -0 "$app_pid" 2>/dev/null; then
    cat "$log_file"
    echo "GPUI host exited before its PTY fixture rendered" >&2
    exit 1
  fi
  sleep 0.1
done

if ! grep -Fq ROOTGLASS_GPUI_HOST_READY "$log_file"; then
  cat "$log_file"
  echo "Timed out waiting for the GPUI PTY fixture" >&2
  exit 1
fi

sleep 1
import -window root "$screenshot"
test -s "$screenshot"
identify -format 'Captured GPUI Xvfb screenshot: %wx%h %m\n' "$screenshot"
wait "$app_pid"
app_pid=""
cat "$log_file"
