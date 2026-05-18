#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GLASS_DIR="${ROOT_DIR}/examples/hello-world.glass"
BOOT_LOG="${ROOT_DIR}/.rootglass/build/hello-world-boot.log"
PLAN_LOG="${ROOT_DIR}/.rootglass/build/hello-world-plan.log"
TIMEOUT_SECONDS="${TIMEOUT_SECONDS:-90}"
EXPECT_QEMU_ACCEL="${EXPECT_QEMU_ACCEL:-}"

cd "${ROOT_DIR}"

mkdir -p "$(dirname "${BOOT_LOG}")"

if ! command -v qemu-system-aarch64 >/dev/null 2>&1; then
  echo "missing qemu-system-aarch64" >&2
  echo "install qemu-system-aarch64 before running this script" >&2
  exit 1
fi

bash scripts/materialize-example-assets.sh

cargo run --locked -p rootglass-cli -- plan "${GLASS_DIR}" >"${PLAN_LOG}" 2>&1
cat "${PLAN_LOG}"

if [[ -n "${EXPECT_QEMU_ACCEL}" ]]; then
  if ! grep -q -- "-accel ${EXPECT_QEMU_ACCEL}" "${PLAN_LOG}"; then
    echo "expected QEMU acceleration not present: -accel ${EXPECT_QEMU_ACCEL}" >&2
    exit 1
  fi
fi

set +e
timeout --foreground "${TIMEOUT_SECONDS}" \
  cargo run --locked -p rootglass-cli -- boot "${GLASS_DIR}" \
  >"${BOOT_LOG}" 2>&1
status=$?
set -e

cat "${BOOT_LOG}"

if grep -Eq "Mounting root: failed|Launching initramfs emergency recovery shell|mount: mounting .* failed|Kernel panic|QEMU exited with status|error: QEMU exited" "${BOOT_LOG}"; then
  echo "boot smoke failed: guest did not mount the intended root filesystem" >&2
  exit 1
fi

if grep -Eq "rootglass-hello login:" "${BOOT_LOG}"; then
  echo "boot smoke succeeded"
  exit 0
fi

if [[ "${status}" -eq 124 ]]; then
  echo "boot smoke timed out before seeing the rootglass login prompt" >&2
else
  echo "boot command exited with status ${status} before seeing the rootglass login prompt" >&2
fi

exit 1
