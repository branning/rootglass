# Writable Overlays

Rootglass models a glass as one required read-only root filesystem plus zero or one selected writable overlay at launch time.

## MVP rules

- A `.glass` has exactly one required read-only rootfs.
- A serialized `.glass` may include at most one bundled writable overlay partition.
- The local Rootglass application may create and track multiple writable overlays for the same glass.
- Only one writable overlay may be selected for a launch.
- A selected local overlay takes precedence over the bundled overlay.

## Current guest behavior

The reference Alpine guest uses the separate-state-disk option in app-spec F-21. The rootfs remains read-only and the state disk is mounted by its `rootglass-state` filesystem label at `/state`, using BusyBox-compatible `LABEL=` mount syntax. This is **not** a writable rootfs and does not use OverlayFS. Guest applications that need persistence must write under `/state`; paths such as `/etc` and `/var` remain on the read-only base.

Before boot, Rootglass copies the bundled state image into its per-user state store and attaches the local copy. The bundled image is a template and must remain unchanged. The guest mount script requires `/state` when a state disk is attached and emits a boot failure marker if the label cannot be mounted.

## Naming

Local overlays are named by the user or by the application. The storage layer sanitizes names before using them as filenames.

Example:

```text
Rootglass overlay store root:
  .rootglass/overlays/

Glass identifier:
  io.rootglass.examples.hello-world

Local overlay name:
  experiment-1

Resolved local overlay path:
  .rootglass/overlays/io.rootglass.examples.hello-world/experiment-1.img
```

## Selection behavior

Given a glass with:

```toml
[boot]
rootfs = "rootfs.img"
writable_overlay = "state.img"
```

Launch with no local overlay selected:

```text
rootfs:  hello-world.glass/rootfs.img
state:   hello-world.glass/state.img
```

Launch with local overlay `experiment-1` selected:

```text
rootfs:  hello-world.glass/rootfs.img
state:   .rootglass/overlays/io.rootglass.examples.hello-world/experiment-1.img
```

## Reset behavior

The current reset operation replaces the selected local state image with a fresh copy of the bundled template. It does not modify the read-only rootfs or the bundled state image.

Possible reset operations:

- **Restart** — stop and start QEMU with the same selected state image.
- **Fresh state** — create a new local copy of the bundled state template and select it.
- **Restore bundled state** — reset a selected local state image from the bundled template.
- **Delete state** — remove a local state image after confirmation.

The read-only rootfs should not be modified by reset.

## Future ideas

- Multiple read-only overlay packs.
- Snapshot and restore.
- Clone local overlay.
- Export local overlay.
- Diff local overlay against bundled state.
