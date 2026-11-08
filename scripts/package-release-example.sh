#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ $# -ne 1 ]]; then
  echo "usage: $0 <output-directory>" >&2
  exit 2
fi

output_dir="$1"
mkdir -p "$output_dir"
output_dir="$(cd "$output_dir" && pwd)"
glass_dir="$repo_root/examples/hello-world.glass"
state_image="$glass_dir/state.img"
package="$output_dir/rootglass-hello-world-0.1.0.glass"
seed_file="$(mktemp "${TMPDIR:-/tmp}/rootglass-example-signing-seed.XXXXXX")"
ratio_guard="$(mktemp "${TMPDIR:-/tmp}/rootglass-example-ratio-guard.XXXXXX")"
trap 'rm -f "$seed_file" "$ratio_guard"' EXIT

if [[ ! -f "$state_image" ]]; then
  echo "materialize the example assets before packaging: $state_image is missing" >&2
  exit 1
fi

# Keep the mostly empty ext4 state image below the archive importer's 200:1
# compression-ratio limit. The file is hidden in the disposable sample state.
python3 - "$ratio_guard" <<'PY'
import hashlib
import sys

with open(sys.argv[1], "wb") as payload:
    for counter in range(32768):
        payload.write(hashlib.sha256(b"rootglass release sample state" + counter.to_bytes(8, "little")).digest())
PY
debugfs -w -R "write $ratio_guard /.rootglass-release-ratio-guard.bin" "$state_image" >/dev/null

# This deterministic public fixture key is intentionally not a production
# secret. The signing library generates the package signature from this seed.
python3 - "$seed_file" <<'PY'
import hashlib
import os
import sys

seed = hashlib.sha256(b"Rootglass public development fixture signing key v1").digest()
with open(sys.argv[1], "wb") as output:
    output.write(seed)
os.chmod(sys.argv[1], 0o600)
PY

cd "$repo_root"
cargo run --locked --manifest-path crates/rootglass-desktop/Cargo.toml --no-default-features -- \
  package examples/hello-world.glass "$package" --key "$seed_file" \
  --created-at 2026-11-08T00:00:00Z

# Exercise the serialized archive through the same bounded import and planning
# path users run. Planning an archive imports it into Rootglass's local package
# library before printing a launch plan.
cargo run --locked --manifest-path crates/rootglass-desktop/Cargo.toml --no-default-features -- \
  plan "$package"

(
  cd "$output_dir"
  sha256sum "$(basename "$package")" > SHA256SUMS-example.txt
  sha256sum --check SHA256SUMS-example.txt
)
