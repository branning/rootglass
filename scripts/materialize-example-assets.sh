#!/usr/bin/env bash
set -euo pipefail

# Materialize local, generated assets for examples/hello-world.glass.
#
# This intentionally does not commit large binary assets. It downloads canned
# Alpine Linux aarch64 material and produces the files that the example glass
# expects locally:
#
#   examples/hello-world.glass/rootfs.img
#   examples/hello-world.glass/state.img
#   runtime-assets/alpine-aarch64/vmlinuz-virt
#   runtime-assets/alpine-aarch64/initramfs-virt
#
# Dependencies:
#   curl, tar, dd, mkfs.ext4, sudo, mount, umount
#
# Linux is currently required for rootfs image creation because this script
# mounts an ext4 image locally. macOS/Windows users can run this inside an
# Ubuntu 24.04 VM/container with loop-device support.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GLASS_DIR="${ROOT_DIR}/examples/hello-world.glass"
ASSET_DIR="${ROOT_DIR}/runtime-assets/alpine-aarch64"
WORK_DIR="${ROOT_DIR}/.rootglass/build/hello-world-assets"

ALPINE_RELEASE="${ALPINE_RELEASE:-latest-stable}"
ALPINE_BASE_URL="${ALPINE_BASE_URL:-https://dl-cdn.alpinelinux.org/alpine/${ALPINE_RELEASE}/releases/aarch64}"
ALPINE_MINIROOTFS="${ALPINE_MINIROOTFS:-alpine-minirootfs-3.22.2-aarch64.tar.gz}"
ROOTFS_SIZE_MB="${ROOTFS_SIZE_MB:-256}"
STATE_SIZE_MB="${STATE_SIZE_MB:-64}"

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

require_cmd curl
require_cmd tar
require_cmd dd
require_cmd mkfs.ext4
require_cmd sudo
require_cmd mount
require_cmd umount

mkdir -p "${GLASS_DIR}" "${ASSET_DIR}" "${WORK_DIR}"

MINIROOTFS_URL="${ALPINE_BASE_URL}/${ALPINE_MINIROOTFS}"
MINIROOTFS_SHA_URL="${MINIROOTFS_URL}.sha256"
KERNEL_URL="${ALPINE_BASE_URL}/netboot/vmlinuz-virt"
INITRAMFS_URL="${ALPINE_BASE_URL}/netboot/initramfs-virt"

MINIROOTFS_TARBALL="${ASSET_DIR}/${ALPINE_MINIROOTFS}"
ROOTFS_IMG="${GLASS_DIR}/rootfs.img"
STATE_IMG="${GLASS_DIR}/state.img"

curl_if_missing() {
  local url="$1"
  local output="$2"
  if [[ -f "${output}" ]]; then
    echo "exists: ${output}"
    return 0
  fi
  echo "download: ${url}"
  curl --fail --location --show-error --output "${output}" "${url}"
}

curl_if_missing "${MINIROOTFS_URL}" "${MINIROOTFS_TARBALL}"
curl_if_missing "${MINIROOTFS_SHA_URL}" "${MINIROOTFS_TARBALL}.sha256"
curl_if_missing "${KERNEL_URL}" "${ASSET_DIR}/vmlinuz-virt"
curl_if_missing "${INITRAMFS_URL}" "${ASSET_DIR}/initramfs-virt"

if command -v sha256sum >/dev/null 2>&1; then
  (
    cd "${ASSET_DIR}"
    sha256sum --check "${ALPINE_MINIROOTFS}.sha256"
  )
else
  echo "sha256sum not available; skipping minirootfs checksum verification" >&2
fi

if [[ ! -f "${ROOTFS_IMG}" ]]; then
  echo "create rootfs image: ${ROOTFS_IMG}"
  dd if=/dev/zero of="${ROOTFS_IMG}" bs=1M count="${ROOTFS_SIZE_MB}" status=progress
  mkfs.ext4 -F "${ROOTFS_IMG}"

  MOUNT_DIR="${WORK_DIR}/mnt-rootfs"
  mkdir -p "${MOUNT_DIR}"
  sudo mount -o loop "${ROOTFS_IMG}" "${MOUNT_DIR}"
  cleanup_mount() {
    if mountpoint -q "${MOUNT_DIR}"; then
      sudo umount "${MOUNT_DIR}"
    fi
  }
  trap cleanup_mount EXIT

  sudo tar -xzf "${MINIROOTFS_TARBALL}" -C "${MOUNT_DIR}"

  # Make the minirootfs slightly friendlier for an interactive serial boot.
  echo 'ttyAMA0::respawn:/sbin/getty -L ttyAMA0 115200 vt100' | sudo tee -a "${MOUNT_DIR}/etc/inittab" >/dev/null
  echo 'rootglass-hello' | sudo tee "${MOUNT_DIR}/etc/hostname" >/dev/null

  cleanup_mount
  trap - EXIT
else
  echo "exists: ${ROOTFS_IMG}"
fi

if [[ ! -f "${STATE_IMG}" ]]; then
  echo "create writable state image: ${STATE_IMG}"
  dd if=/dev/zero of="${STATE_IMG}" bs=1M count="${STATE_SIZE_MB}" status=progress
  mkfs.ext4 -F "${STATE_IMG}"
else
  echo "exists: ${STATE_IMG}"
fi

cat <<EOF

Generated assets:
  ${ROOTFS_IMG}
  ${STATE_IMG}
  ${ASSET_DIR}/vmlinuz-virt
  ${ASSET_DIR}/initramfs-virt

Next:
  cargo run -p rootglass-cli -- validate examples/hello-world.glass
  cargo run -p rootglass-cli -- plan examples/hello-world.glass

Note: QEMU boot still requires runtime-profile asset resolution wiring in Rootglass.
EOF
