set shell := ["bash", "-cu"]

glass := "examples/hello-world.glass"

# List available commands.
default:
    @just --list

# Run the same core checks used by CI.
check: fmt clippy test cli-smoke

# Verify formatting without rewriting files.
fmt:
    cargo fmt --all -- --check

# Format the workspace in place.
fmt-fix:
    cargo fmt --all

# Run clippy with the committed lockfile.
clippy:
    cargo clippy --locked --workspace --all-targets -- -D warnings

# Run all workspace tests with the committed lockfile.
test:
    cargo test --locked --workspace --all-targets

# Exercise the CLI commands that do not require generated binary assets.
cli-smoke: validate plan boot-preflight-missing-assets

# Validate the default example glass manifest.
validate:
    cargo run --locked -p rootglass-cli -- validate {{glass}}

# Print the QEMU launch plan for the default example glass.
plan:
    cargo run --locked -p rootglass-cli -- plan {{glass}}

# Prove boot fails before QEMU when generated assets are absent.
boot-preflight-missing-assets:
    if [[ -f {{glass}}/rootfs.img || -f runtime-assets/alpine-aarch64/vmlinuz-virt ]]; then \
      echo "generated assets exist; skipping missing-asset preflight check"; \
    else \
      cargo run --locked -p rootglass-cli -- boot {{glass}} 2>&1 | tee /tmp/rootglass-boot-preflight.log; \
      grep -q "missing kernel asset" /tmp/rootglass-boot-preflight.log; \
    fi

# Download/generate local example assets. Requires Linux, sudo, curl, tar, dd, mkfs.ext4, mount, and umount.
materialize-assets:
    bash scripts/materialize-example-assets.sh

# Print a bootable plan after local assets have been materialized.
plan-with-assets: materialize-assets
    cargo run --locked -p rootglass-cli -- plan {{glass}}

# Boot the default example glass interactively after local assets have been materialized.
# Exit QEMU with Ctrl-a x when using -nographic/mon:stdio.
boot: materialize-assets
    cargo run --locked -p rootglass-cli -- boot {{glass}}

# Run a bounded automated boot smoke test and look for the Alpine login marker.
boot-smoke:
    bash scripts/smoke-boot-example.sh

# Clean generated example assets and local Rootglass build state.
clean-assets:
    rm -f {{glass}}/rootfs.img {{glass}}/state.img
    rm -rf runtime-assets .rootglass/build
