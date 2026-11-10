#!/usr/bin/env bash
set -euo pipefail

rootglass_binary="$(realpath "${1:?usage: capture-rootglass-publisher-review-xvfb.sh ROOTGLASS_BINARY OUTPUT_DIR}")"
artifact_dir="${2:?usage: capture-rootglass-publisher-review-xvfb.sh ROOTGLASS_BINARY OUTPUT_DIR}"
repo_root="$(git rev-parse --show-toplevel)"
mkdir -p "$artifact_dir"
artifact_dir="$(cd "$artifact_dir" && pwd)"
work_dir="$(mktemp -d)"
log_file="${artifact_dir}/publisher-review.log"
event_file="${artifact_dir}/publisher-review-events.log"
app_pid=""
window_id=""

cleanup() {
  if [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; then
    if [[ -n "$window_id" ]]; then
      xdotool windowclose "$window_id" 2>/dev/null || true
    fi
    kill -- "-$app_pid" 2>/dev/null || kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
  fi
  chmod -R u+w "$work_dir" 2>/dev/null || true
  rm -rf "$work_dir"
}
trap cleanup EXIT

mkdir -p "$work_dir/home" "$work_dir/config/rootglass" "$work_dir/data" "$work_dir/runtime"
chmod 700 "$work_dir/runtime"
cat >"$work_dir/config/rootglass/trust.json" <<'JSON'
{
  "schemaVersion": 1,
  "trustedPublishers": [],
  "revokedKeyIds": []
}
JSON

archive_file="${work_dir}/untrusted-publisher.glass"
python3 - "${repo_root}/examples/hello-world.glass/rootglass.toml" "$archive_file" <<'PY'
import base64
import json
import sys
import zipfile

manifest_path, archive_path = sys.argv[1:]
with open(manifest_path, "rb") as source:
    manifest = source.read()
payload = base64.urlsafe_b64encode(b"{}").rstrip(b"=").decode("ascii")
envelope = json.dumps({
    "payloadType": "application/vnd.in-toto+json",
    "payload": payload,
    "signatures": [{
        "keyid": "ed25519:" + "9" * 64,
        "sig": "AA",
    }],
}).encode("utf-8")
with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
    archive.writestr("rootglass.toml", manifest)
    archive.writestr("signatures/glass.dsse.json", envelope)
PY

export HOME="${work_dir}/home"
export XDG_CONFIG_HOME="${work_dir}/config"
export XDG_DATA_HOME="${work_dir}/data"
export XDG_RUNTIME_DIR="${work_dir}/runtime"
export XDG_CURRENT_DESKTOP=GNOME
export GDK_BACKEND=x11
export ROOTGLASS_DESKTOP_SMOKE=1
export ROOTGLASS_DESKTOP_SMOKE_EVENT_LOG="$event_file"

setsid bash -c 'cd "$1"; exec "$2" gui "$3"' _ "$work_dir" "$rootglass_binary" "$archive_file" >"$log_file" 2>&1 &
app_pid=$!

for _ in $(seq 1 300); do
  if [[ -f "$event_file" ]] \
    && grep -Fq ROOTGLASS_DESKTOP_WINDOW_READY "$event_file" \
    && grep -Fq ROOTGLASS_DESKTOP_PUBLISHER_REVIEW "$event_file"; then
    break
  fi
  if ! kill -0 "$app_pid" 2>/dev/null; then
    cat "$log_file" "$event_file" 2>/dev/null || true
    echo "Rootglass exited before the publisher review opened" >&2
    exit 1
  fi
  sleep 0.1
done
if [[ ! -f "$event_file" ]] \
  || ! grep -Fq ROOTGLASS_DESKTOP_WINDOW_READY "$event_file" \
  || ! grep -Fq ROOTGLASS_DESKTOP_PUBLISHER_REVIEW "$event_file"; then
  cat "$log_file" "$event_file" 2>/dev/null || true
  echo "Timed out waiting for the untrusted publisher review" >&2
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
import -window "$window_id" "${artifact_dir}/00-publisher-review.png"
identify -format '%f: %wx%h %m\n' "${artifact_dir}/00-publisher-review.png"
grep -F "ROOTGLASS_DESKTOP_PUBLISHER_REVIEW" "$event_file"
cat "$log_file" "$event_file" 2>/dev/null || true
