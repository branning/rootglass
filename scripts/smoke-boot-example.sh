#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GLASS_DIR="${ROOT_DIR}/examples/hello-world.glass"
BOOT_LOG="${ROOT_DIR}/.rootglass/build/hello-world-boot.log"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-90}"

cd "${ROOT_DIR}"

mkdir -p "$(dirname "${BOOT_LOG}")"

if ! command -v qemu-system-aarch64 >/dev/null 2>&1; then
  echo "missing qemu-system-aarch64" >&2
  echo "install qemu-system-aarch64 before running this script" >&2
  exit 1
fi

bash scripts/materialize-example-assets.sh

set +e
timeout --foreground "${TIMEOUT_SECONDS}" \
  cargo run --locked -p rootglass-cli -- boot "${GLASS_DIR}" \
  >"${BOOT_LOG}" 2>&1
status=$?
set -e

cat "${BOOT_LOG}"

if grep -Eq "rootglass-hello login:|Welcome to Alpine Linux|Alpine Linux" "${BOOT_LOG}"; then
  echo "boot smoke succeeded"
  exit 0
fi

if [[ "${status}" -eq 124 ]]; then
  echo "boot smoke timed out before seeing an expected boot marker" >&2
else
  echo "boot command exited with status ${status} before seeing an expected boot marker" >&2
fi

exit 1
