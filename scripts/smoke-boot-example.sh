#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

if ! command -v qemu-system-aarch64 >/dev/null 2>&1; then
  echo "missing qemu-system-aarch64" >&2
  echo "install qemu-system-aarch64 before running this script" >&2
  exit 1
fi

bash scripts/materialize-example-assets.sh
exec python3 scripts/smoke-boot-example.py
