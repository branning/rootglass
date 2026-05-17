# Writable Overlays

Rootglass models a glass as one required read-only root filesystem plus zero or one selected writable overlay at launch time.

## MVP rules

- A `.glass` has exactly one required read-only rootfs.
- A serialized `.glass` may include at most one bundled writable overlay partition.
- The local Rootglass application may create and track multiple writable overlays for the same glass.
- Only one writable overlay may be selected for a launch.
- A selected local overlay takes precedence over the bundled overlay.

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

The MVP should treat reset as an overlay operation, not as a mutation of the read-only rootfs.

Possible reset operations:

- **Restart** — stop and start QEMU with the same selected overlay.
- **Fresh overlay** — create a new blank local overlay and launch with it.
- **Restore bundled state** — discard the selected local overlay and return to the bundled overlay, if one exists.
- **Delete overlay** — remove a local overlay after confirmation.

The read-only rootfs should not be modified by reset.

## Future ideas

- Multiple read-only overlay packs.
- Snapshot and restore.
- Clone local overlay.
- Export local overlay.
- Diff local overlay against bundled state.
