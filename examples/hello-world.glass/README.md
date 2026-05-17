# Hello World Glass

This directory is a development-form `.glass` fixture. It mirrors the intended packaged bundle layout without committing large binary images to the repository.

## Files

```text
hello-world.glass/
  rootglass.toml              # bundle manifest
  rootfs.img                  # required read-only root filesystem image, not committed
  state.img                   # optional writable overlay partition, not committed
  metadata/
    README.md                 # human-facing notes
  signatures/
    glass.dsse.json           # DSSE-wrapped in-toto-style signature
```

## Binary assets

Large binary guest assets are intentionally not committed yet.

Expected local files once generated or downloaded:

- `rootfs.img` — required read-only root filesystem image.
- `state.img` — optional writable overlay partition.
- `signatures/glass.dsse.json` — DSSE-wrapped signature over the manifest and committed fixture metadata.

Until those assets exist, this fixture is still useful for parser and validation tests that distinguish between structural validation and full launch readiness.

## Validation expectations

A manifest-only test should be able to parse `rootglass.toml` and verify:

- schema version is supported,
- architecture is `aarch64`,
- runtime profile is `virt-linux`,
- kernel and device tree use builtin references,
- rootfs is declared,
- writable overlay is optional and singular,
- publisher/signature metadata is present,
- host constraints are represented.

A launch-readiness test should fail until `rootfs.img` exists.
