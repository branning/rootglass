#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <input.dmg> <output-directory> <repo-root>" >&2
  exit 2
fi

input_dmg="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
output_dir="$2"
repo_root="$3"
mkdir -p "$output_dir"

: "${MACOS_CERTIFICATE_P12_BASE64:?protected macOS signing certificate secret is required}"
: "${MACOS_CERTIFICATE_PASSWORD:?protected macOS certificate password secret is required}"
: "${MACOS_DEVELOPER_ID_APPLICATION:?Developer ID Application identity secret is required}"
: "${APPLE_ID:?Apple ID secret is required for notarization}"
: "${APPLE_APP_SPECIFIC_PASSWORD:?Apple app-specific password secret is required}"
: "${APPLE_TEAM_ID:?Apple Team ID secret is required}"

temp_root="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/rootglass-macos-sign.XXXXXX")"
mountpoint="$temp_root/mount"
final_mountpoint="$temp_root/final-mount"
keychain="$temp_root/rootglass-signing.keychain-db"
keychain_password="$(openssl rand -hex 24)"
writable_dmg="$temp_root/rootglass-writable.dmg"
signed_dmg="$output_dir/$(basename "$input_dmg")"
certificate="$temp_root/developer-id.p12"
app_zip="$temp_root/rootglass-app.zip"
app_notary_result="$temp_root/app-notary-result.json"
dmg_notary_result="$temp_root/dmg-notary-result.json"
mounted=false
final_mounted=false
keychain_created=false

cleanup() {
  if [[ "$mounted" == true ]]; then
    hdiutil detach "$mountpoint" -quiet || true
  fi
  if [[ "$final_mounted" == true ]]; then
    hdiutil detach "$final_mountpoint" -quiet || true
  fi
  if [[ "$keychain_created" == true ]]; then
    security delete-keychain "$keychain" >/dev/null 2>&1 || true
  fi
  rm -rf "$temp_root"
}
trap cleanup EXIT

printf '%s' "$MACOS_CERTIFICATE_P12_BASE64" | base64 -D > "$certificate"
security create-keychain -p "$keychain_password" "$keychain"
keychain_created=true
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$certificate" -k "$keychain" -P "$MACOS_CERTIFICATE_PASSWORD" -T /usr/bin/codesign -T /usr/bin/security >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" >/dev/null
security list-keychains -d user -s "$keychain"

hdiutil convert "$input_dmg" -format UDRW -o "$writable_dmg" >/dev/null
mkdir -p "$mountpoint"
hdiutil attach "$writable_dmg" -readwrite -nobrowse -noautoopen -mountpoint "$mountpoint" >/dev/null
mounted=true
mapfile -t apps < <(find "$mountpoint" -maxdepth 3 -type d -name 'Rootglass.app' -print)
if [[ "${#apps[@]}" -ne 1 ]]; then
  echo "Expected exactly one Rootglass.app in the DMG; found ${#apps[@]}" >&2
  find "$mountpoint" -maxdepth 3 -print >&2
  exit 1
fi
app="${apps[0]}"
qemu="$app/Contents/Resources/rootglass/runtime-assets"
mapfile -t qemu_binaries < <(find "$qemu" -type f -name 'qemu-system-aarch64' -print)
if [[ "${#qemu_binaries[@]}" -ne 1 ]]; then
  echo "Expected one bundled QEMU executable; found ${#qemu_binaries[@]}" >&2
  exit 1
fi

# Re-sign every nested Mach-O image before the outer app. QEMU uses TCG's JIT,
# so only that executable receives the hardened-runtime allow-jit entitlement.
while IFS= read -r -d '' image; do
  if [[ "$(file -b "$image")" != *Mach-O* ]]; then
    continue
  fi
  if [[ "$image" == "${qemu_binaries[0]}" ]]; then
    codesign --force --options runtime --timestamp \
      --entitlements "$repo_root/packaging/macos-qemu.entitlements" \
      --sign "$MACOS_DEVELOPER_ID_APPLICATION" "$image"
  else
    codesign --force --options runtime --timestamp \
      --sign "$MACOS_DEVELOPER_ID_APPLICATION" "$image"
  fi
done < <(find "$app/Contents" -type f -print0)
codesign --force --options runtime --timestamp \
  --sign "$MACOS_DEVELOPER_ID_APPLICATION" "$app"
codesign --verify --deep --strict --verbose=2 "$app"
codesign -dv --verbose=4 "$app" 2> "$temp_root/app-signature.txt"
grep -Fqx "TeamIdentifier=$APPLE_TEAM_ID" "$temp_root/app-signature.txt" || {
  echo "The app signature team ID does not match APPLE_TEAM_ID" >&2
  cat "$temp_root/app-signature.txt" >&2
  exit 1
}
grep -Fqx "Authority=$MACOS_DEVELOPER_ID_APPLICATION" "$temp_root/app-signature.txt" || {
  echo "The app signature authority does not match the configured Developer ID" >&2
  cat "$temp_root/app-signature.txt" >&2
  exit 1
}

ditto -c -k --keepParent "$app" "$app_zip"
xcrun notarytool submit "$app_zip" --wait --output-format json \
  --apple-id "$APPLE_ID" \
  --password "$APPLE_APP_SPECIFIC_PASSWORD" \
  --team-id "$APPLE_TEAM_ID" > "$app_notary_result"
app_notary_status="$(/usr/bin/plutil -extract status raw -o - "$app_notary_result")"
app_notary_id="$(/usr/bin/plutil -extract id raw -o - "$app_notary_result")"
[[ "$app_notary_status" == Accepted ]] || {
  echo "Apple notarization did not accept the app (status: $app_notary_status)" >&2
  cat "$app_notary_result" >&2
  exit 1
}
xcrun stapler staple "$app"
xcrun stapler validate "$app"
spctl --assess --type execute --verbose=2 "$app"

hdiutil detach "$mountpoint" -quiet
mounted=false
rm -f "$signed_dmg"
hdiutil convert "$writable_dmg" -format UDZO -o "$signed_dmg" >/dev/null
codesign --force --timestamp --sign "$MACOS_DEVELOPER_ID_APPLICATION" "$signed_dmg"
codesign --verify --strict --verbose=2 "$signed_dmg"
codesign -dv --verbose=4 "$signed_dmg" 2> "$temp_root/dmg-signature.txt"
grep -Fqx "TeamIdentifier=$APPLE_TEAM_ID" "$temp_root/dmg-signature.txt" || {
  echo "The DMG signature team ID does not match APPLE_TEAM_ID" >&2
  cat "$temp_root/dmg-signature.txt" >&2
  exit 1
}
grep -Fqx "Authority=$MACOS_DEVELOPER_ID_APPLICATION" "$temp_root/dmg-signature.txt" || {
  echo "The DMG signature authority does not match the configured Developer ID" >&2
  cat "$temp_root/dmg-signature.txt" >&2
  exit 1
}

xcrun notarytool submit "$signed_dmg" --wait --output-format json \
  --apple-id "$APPLE_ID" \
  --password "$APPLE_APP_SPECIFIC_PASSWORD" \
  --team-id "$APPLE_TEAM_ID" > "$dmg_notary_result"
dmg_notary_status="$(/usr/bin/plutil -extract status raw -o - "$dmg_notary_result")"
dmg_notary_id="$(/usr/bin/plutil -extract id raw -o - "$dmg_notary_result")"
[[ "$dmg_notary_status" == Accepted ]] || {
  echo "Apple notarization did not accept the DMG (status: $dmg_notary_status)" >&2
  cat "$dmg_notary_result" >&2
  exit 1
}
xcrun stapler staple "$signed_dmg"
xcrun stapler validate "$signed_dmg"
spctl --assess --type open --context context:primary-signature --verbose=2 "$signed_dmg"
codesign --verify --deep --strict --verbose=2 "$signed_dmg"

mkdir -p "$final_mountpoint"
hdiutil attach "$signed_dmg" -readonly -nobrowse -noautoopen \
  -mountpoint "$final_mountpoint" >/dev/null
final_mounted=true
mapfile -t packaged_apps < <(find "$final_mountpoint" -maxdepth 3 -type d -name 'Rootglass.app' -print)
if [[ "${#packaged_apps[@]}" -ne 1 ]]; then
  echo "The signed DMG must contain exactly one Rootglass.app" >&2
  exit 1
fi
codesign --verify --deep --strict --verbose=2 "${packaged_apps[0]}"
xcrun stapler validate "${packaged_apps[0]}"
spctl --assess --type execute --verbose=2 "${packaged_apps[0]}"
hdiutil detach "$final_mountpoint" -quiet
final_mounted=false

python3 - "$output_dir/SIGNING-PROVENANCE-macos-arm64.json" \
  "$MACOS_DEVELOPER_ID_APPLICATION" "$APPLE_TEAM_ID" \
  "$app_notary_id" "$app_notary_status" "$dmg_notary_id" "$dmg_notary_status" <<'PY'
import json
import pathlib
import sys

path, identity, team_id, app_submission_id, app_status, dmg_submission_id, dmg_status = sys.argv[1:]
pathlib.Path(path).write_text(
    json.dumps(
        {
            "schema_version": 1,
            "platform": "macOS arm64",
            "application_identity": identity,
            "team_id": team_id,
            "hardened_runtime": True,
            "timestamped": True,
            "qemu_jit_entitlement": "com.apple.security.cs.allow-jit",
            "notarization": {
                "application": {
                    "submission_id": app_submission_id,
                    "status": app_status,
                    "stapled": True,
                },
                "disk_image": {
                    "submission_id": dmg_submission_id,
                    "status": dmg_status,
                    "stapled": True,
                },
            },
            "verification": [
                "codesign --verify --deep --strict",
                "xcrun stapler validate",
                "spctl --assess --type open",
            ],
        },
        indent=2,
    )
    + "\n",
    encoding="utf-8",
)
PY
