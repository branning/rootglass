#!/usr/bin/env bash
set -euo pipefail

rootglass_binary="$(realpath "${1:?usage: smoke-rootglass-installed-linux-association.sh ROOTGLASS_BINARY GLASS_FILE OUTPUT_DIR}")"
glass_file="$(realpath "${2:?usage: smoke-rootglass-installed-linux-association.sh ROOTGLASS_BINARY GLASS_FILE OUTPUT_DIR}")"
output_dir="${3:?usage: smoke-rootglass-installed-linux-association.sh ROOTGLASS_BINARY GLASS_FILE OUTPUT_DIR}"
mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd)"
if [[ -n "${ROOTGLASS_DESKTOP_SMOKE_TMPDIR:-}" ]]; then
  work_dir="$(mktemp -d "${ROOTGLASS_DESKTOP_SMOKE_TMPDIR%/}/rootglass-association.XXXXXX")"
else
  work_dir="$(mktemp -d)"
fi
event_log="$work_dir/desktop-events.log"
launch_log="$work_dir/xdg-open.log"
app_pid=""
xdg_pid=""

log() {
  printf '[%s] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

cleanup() {
  if [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; then
    kill "$app_pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 "$app_pid" 2>/dev/null || break
      sleep 0.1
    done
  fi
  if [[ -n "$xdg_pid" ]] && kill -0 "$xdg_pid" 2>/dev/null; then
    kill "$xdg_pid" 2>/dev/null || true
  fi
  [[ ! -f "$event_log" ]] || cp "$event_log" "$output_dir/desktop-events.log"
  [[ ! -f "$launch_log" ]] || cp "$launch_log" "$output_dir/xdg-open.log"
  if [[ "${ROOTGLASS_DESKTOP_SMOKE_DEFER_CLEANUP:-0}" == 1 ]]; then
    printf '%s\n' "$work_dir" > "$output_dir/cleanup-path.txt"
    log "deferred temporary portal runtime cleanup until the session bus exits"
  else
    chmod -R u+w "$work_dir" 2>/dev/null || true
    rm -rf "$work_dir"
  fi
}
trap cleanup EXIT

mkdir -p "$work_dir/home" "$work_dir/config/rootglass" "$work_dir/data" "$work_dir/runtime" "$work_dir/bin"
chmod 700 "$work_dir/runtime"
cat > "$work_dir/config/rootglass/trust.json" <<'JSON'
{
  "schemaVersion": 1,
  "trustedPublishers": [
    {
      "name": "Rootglass Examples",
      "identity": "rootglass.dev/examples",
      "keyId": "ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0",
      "publicKeyBase64url": "2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw"
    }
  ],
  "revokedKeyIds": []
}
JSON
chmod 600 "$work_dir/config/rootglass/trust.json"

# These shims make any accidental developer-tool or system-QEMU lookup fail,
# while preserving the installed application and ordinary host utilities.
for tool in cargo rustc qemu-system-aarch64; do
  cat > "$work_dir/bin/$tool" <<EOF
#!/usr/bin/env bash
touch "$work_dir/bin/$tool.used"
echo "unexpected $tool invocation in installed-package smoke" >&2
exit 99
EOF
  chmod 755 "$work_dir/bin/$tool"
done
export PATH="$work_dir/bin:$PATH"
export HOME="$work_dir/home"
export XDG_CONFIG_HOME="$work_dir/config"
export XDG_DATA_HOME="$work_dir/data"
export XDG_RUNTIME_DIR="$work_dir/runtime"
export XDG_CURRENT_DESKTOP=GNOME
export GDK_BACKEND=x11
export WINIT_UNIX_BACKEND=x11
export ROOTGLASS_DESKTOP_SMOKE=1
export ROOTGLASS_DESKTOP_SMOKE_EVENT_LOG="$event_log"

# xdg-open may ask the desktop portal to start the registered application.
# The portal launches it with the D-Bus activation environment, so publish the
# Xvfb, user-home, renderer, and smoke-test settings after creating this
# isolated session and before asking the OS to open the .glass file.
dbus-update-activation-environment \
  DISPLAY XAUTHORITY HOME PATH XDG_CONFIG_HOME XDG_DATA_HOME XDG_RUNTIME_DIR \
  XDG_CURRENT_DESKTOP GDK_BACKEND WINIT_UNIX_BACKEND LIBGL_ALWAYS_SOFTWARE \
  ROOTGLASS_DESKTOP_SMOKE ROOTGLASS_DESKTOP_SMOKE_EVENT_LOG
log "updated the session bus environment for the portal-launched desktop app"

mime_type="application/vnd.rootglass.glass"
mime_package_file="/usr/share/mime/packages/rootglass.xml"
[[ -f "$mime_package_file" ]] || {
  echo "The installed package is missing its shared MIME definition: $mime_package_file" >&2
  exit 1
}
glass_content_type="$(gio info --attributes=standard::content-type -- "$glass_file" \
  | awk '/standard::content-type:/ { print $2; exit }')"
[[ "$glass_content_type" == "$mime_type" ]] || {
  printf 'Expected GIO to identify %s as %s, got %s\n' \
    "$glass_file" "$mime_type" "${glass_content_type:-<none>}" >&2
  exit 1
}
log "looking for installed .glass desktop handler"
mapfile -t matching_desktop_files < <(find /usr/share/applications /usr/local/share/applications \
  -type f -name '*.desktop' -exec grep -Fl "$mime_type" {} + 2>/dev/null | sort)
[[ "${#matching_desktop_files[@]}" -eq 1 ]] || {
  printf 'Expected one installed handler for %s, found %s\n' "$mime_type" "${#matching_desktop_files[@]}" >&2
  printf '%s\n' "${matching_desktop_files[@]}" >&2
  exit 1
}
desktop_file="${matching_desktop_files[0]}"
desktop_id="$(basename "$desktop_file")"
grep -Fq "${mime_type}" "$desktop_file" || {
  echo "$desktop_file does not declare the $mime_type association" >&2
  exit 1
}
xdg-mime default "$desktop_id" "$mime_type"
[[ "$(xdg-mime query default "$mime_type")" == "$desktop_id" ]] || {
  echo "Rootglass did not register as the selected handler for $mime_type" >&2
  exit 1
}
printf 'Registered .glass handler: %s (%s)\n' "$desktop_id" "$desktop_file"

cd "$work_dir"
log "launching .glass through xdg-open"
setsid xdg-open "$glass_file" >"$launch_log" 2>&1 &
xdg_pid=$!

find_associated_app() {
  local proc candidate command_line
  for proc in /proc/[0-9]*; do
    [[ -e "$proc/exe" ]] || continue
    candidate="$(readlink -f "$proc/exe" 2>/dev/null || true)"
    [[ "$candidate" == "$rootglass_binary" ]] || continue
    command_line="$(tr '\0' '\n' <"$proc/cmdline" 2>/dev/null || true)"
    if grep -Fxq "$glass_file" <<< "$command_line"; then
      echo "${proc##*/}"
      return 0
    fi
  done
  return 1
}

wait_for_marker() {
  local marker="$1"
  local timeout_seconds="${2:-60}"
  log "waiting for $marker"
  for _ in $(seq 1 "$((timeout_seconds * 10))"); do
    if [[ -f "$event_log" ]] && grep -Fq "$marker" "$event_log"; then
      log "observed $marker"
      return 0
    fi
    app_pid="$(find_associated_app || true)"
    if [[ -n "$app_pid" ]] && ! kill -0 "$app_pid" 2>/dev/null; then
      cat "$launch_log" >&2 || true
      echo "The associated Rootglass process exited before $marker" >&2
      exit 1
    fi
    sleep 0.1
  done
  cat "$launch_log" >&2 || true
  [[ ! -f "$event_log" ]] || cat "$event_log" >&2
  echo "Timed out waiting for the installed app to emit $marker" >&2
  exit 1
}

wait_for_marker ROOTGLASS_DESKTOP_WINDOW_READY
wait_for_marker ROOTGLASS_DESKTOP_OPEN_PATH_ACCEPTED
wait_for_marker ROOTGLASS_DESKTOP_OPEN_PATH_ARCHIVE
wait_for_marker ROOTGLASS_DESKTOP_INSPECT_VERIFIED
app_pid="$(find_associated_app)"
[[ -n "$app_pid" ]] || { echo "xdg-open did not launch the installed Rootglass binary" >&2; exit 1; }
printf 'Association launched pid=%s executable=%s with argument=%s\n' "$app_pid" "$rootglass_binary" "$glass_file"
window_id="$(xdotool search --onlyvisible --pid "$app_pid" 2>/dev/null | head -n 1 || true)"
[[ -n "$window_id" ]] || { echo "The association-opened Rootglass window is not visible" >&2; exit 1; }
import -window "$window_id" "$output_dir/associated-glass-open.png"

if compgen -G "$work_dir/bin/*.used" >/dev/null; then
  echo "An installed app unexpectedly used cargo, rustc, or system QEMU" >&2
  ls -l "$work_dir/bin" >&2
  exit 1
fi
echo "PASS: installed package opened the signed .glass through the registered OS association"
