# Rootglass — Complete Application Specification and Swarm Build Contract

**Status:** Implementation contract, version 1.0, 2026-11-05  
**Repository:** https://github.com/branning/rootglass  
**Audience:** Coding agents (including small/Haiku-class agents), integration/review agents, human maintainers  
**Scope:** What currently exists, every component needed for a distributable v1 desktop application, later extensions, and executable work packages  
**Authority:** This spec describes the *target*, not a claim that missing features are implemented. Existing code remains authoritative about current behavior; tests are authoritative about what works. If an older plan conflicts with this document, follow this document for product direction and create an issue for incompatible implementation details.

## 0. Working instructions for a coding-agent swarm

1. **Read this file, then the exact implementation files named in your task.** Do not invent APIs, fields, or source files. Inspect actual code and upstream GPUI Kit examples before writing GPUI code.
2. **Do not implement the entire spec in one branch.** Work in the dependency-ordered work packages in `12. A typical worker owns 1–3 files and one testable outcome.
3. **Do not report success merely because something compiles.** Use the acceptance tests in `10. Preserve the existing Rust CLI while adding the desktop app.
4. **Make one focused branch/PR per work package.** No unrelated refactors. Reserve changes to shared `Cargo.toml`, `Cargo.lock`, and CI workflows for an integrator, or coordinate them explicitly. Never force-push shared branches.
5. **Stop and report a real blocker** when a platform capability, security policy, license, or upstream API differs from this spec; propose the smallest verified adjustment. Do not silently stub a critical feature and call it done.
6. **Treat host-facing security and user data as critical.** Never bypass bundle verification to make a demo pass. No embedded private signing keys or production test-login credentials.
7. **For each PR**, include: objective, changed paths, command(s) run, observed results, known limitations, and associated work-package ID. A reviewer should be able to merge it without reconstructing intent.
8. **Small-agent rule:** Implement the exact assigned contract with targeted tests, avoid changing adjacent subsystems, and hand off any interface change to the integration owner.

## 1. Product definition

**Tagline:** Tiny worlds under glass.

Rootglass is a native desktop application that opens a signed `.glass` artifact containing a self-contained aarch64 embedded Linux application environment, verifies the publisher and artifact, and runs that environment under QEMU as if it were an ordinary desktop app. A nontechnical user should not need to install QEMU, understand rootfs formats, create disks, or type a shell command.

**Developer mode:** A documented Rust CLI supports manifest validation, signature verification, planning, booting, diagnostics, and publishing signed bundles. Both the CLI and desktop shell operate through the same library implementation.

**Primary platforms to qualify:** macOS 15+ (Apple Silicon first), Windows 11, Ubuntu 24.04+ (x86_64 and aarch64 where supported). These are *qualification targets*, not a promise of acceleration or acceptable performance until tested on the relevant hardware.

**First production guest profile:** generic Linux/aarch64 on QEMU's `virt` machine. Kernel and device tree come from a Rootglass-provided runtime profile; guests must not carry arbitrary executable host code. A signed bundle may carry a rootfs image and a state-image template.

**v1 delivery contract:** the user receives a normal installer/application, opens an existing signed `.glass`, sees trusted publisher and runtime requirements, starts it, observes its serial-console application, stops/restarts it safely, and can manage isolated persistent state. The same signed bundle works offline on qualified hosts (subject to explicitly disclosed guest performance constraints). Distribution must not require cloud services.

**Not a goal:** instruction-accurate target board simulation, arbitrary peripheral emulation, a hypervisor replacement, OCI compatibility, HIL, distributed clusters, multi-machine orchestration, guest root access for production users, or a JavaScript/webview frontend.

## 2. Observed repository baseline (read before coding)

The following is a source inventory, **not** a green-CI certification. Check fresh CI on your commit. Names and paths are from `main` when this spec was written.

| Item | Actually present | Missing or incomplete |
|---|---|---|
| `crates/rootglass-manifest` | `Glass`, TOML `Manifest` types, schema validation, aarch64/builtin constraints, manifest-only vs launch-ready checks | Full installed-archive model, robust migration/version policy |
| `crates/rootglass-signature` | DSSE/Ed25519 verification, SHA-256 subjects, directory and ZIP verification, strict ZIP path/duplicate checks, fixture trust store | Signing tool; production trust roots/rotation; secure streamed archive extraction/resource limits; end-to-end signed package creation |
| `crates/rootglass-core` | Lifecycle enum/status/errors, host-preflight model, `LaunchRequest`, verified plan preparation, profile resolution, overlay *selection* | Managed run lifecycle/event stream, import registry, persistent settings, actual process supervisor; currently defaults to fixture-only trust |
| `crates/rootglass-core/src/runtime_profile.rs` | Maps builtin aarch64 references to local Alpine kernel/initramfs; QEMU generates device tree | Installed/bundled asset discovery, versioning, integrity checking |
| `crates/rootglass-core/src/storage.rs` | `OverlayStore` paths, bundled-vs-local selection | Disk creation/cloning, filesystem locking, overlayfs wiring, state durability, reset operations |
| `crates/rootglass-qemu` | QEMU launch-plan construction, accelerator args, child spawning; interactive boot inherits stdio | Capture + writable serial console, QMP control, shutdown, process supervision, output/events, portable bundled-binary lookup |
| `crates/rootglass-cli` | `validate`, `plan`, `boot`; interactive QEMU run and process exit code | Shared supervisor with desktop, archive install/import, sign/package, session diagnostics, guest-success exit semantics |
| `scripts/materialize-example-assets.sh` | Alpine 3.23.4 aarch64 download, ext4 root/state image creation, labels `rootglass-root` / `rootglass-state`, serial getty, trap for mount cleanup | Robust partial-file recovery, existing-image label verification, production-ready guest init/overlayfs, guaranteed boot/login success |
| `scripts/smoke-boot-example.sh` | Bounded QEMU invocation, negative patterns for root mount failure/emergency shell, login-prompt pattern | **Does not log in or run `uname -a`.** Success-on-prompt does not prove usable guest session; QEMU may still run at timeout |
| `.github/workflows/ci.yml` | Format, Clippy, unit tests, CLI smoke, x64 and arm64 hosted boot smoke jobs | Assert login + uname + controlled shutdown; desktop platform build/release matrix; acceleration coverage requires capable runner |
| `examples/hello-world.glass` | Signed development-form manifest + fixture metadata; `root=LABEL=rootglass-root` | Fixture signature/trust synchronization must be reverified; production ZIP including signed images is not generated |
| Desktop UI | GPUI Kit is specified in documentation and issue #25 | **No desktop crate, UI, installer, or application binary has been implemented** |

**Existing contract references:** `docs/architecture.md`, `docs/bundle-format.md`, `docs/signatures.md`, `docs/overlays.md`, `docs/milestone-1.md`, and issues [#11](https://github.com/branning/rootglass/issues/11), [#18](https://github.com/branning/rootglass/issues/18), [#19](https://github.com/branning/rootglass/issues/19), [#20](https://github.com/branning/rootglass/issues/20), [#25](https://github.com/branning/rootglass/issues/25).

### 2.1 Immediate integration blockers to recheck (P0)

- The example manifest changed from `root=/dev/vda` to `root=LABEL=rootglass-root`; prior test/CLI failures showed a DSSE subject digest mismatch. The example envelope was subsequently changed, but the source inventory still shows the original fixture public key in `TrustStore::example_fixture`. **Do not assume this pairing verifies.** First run the CLI and signature suite and repair via a reproducible signing tool.
- Prior x64/arm64 boot jobs entered Alpine initramfs recovery because the state image was `/dev/vda` while rootfs was `/dev/vdb`. The label-based boot is intended to remove ordering dependence; prove it on both runners.
- The smoke harness detects a login prompt but does **not** type a username, obtain a shell, or run `uname -a`. Replace this with an end-to-end serial interaction; never restore the permissive "Alpine mentioned" predicate.
- Existing host acceleration checks are incomplete: macOS/Windows capability cannot be inferred merely from host OS, and aarch64 KVM is not automatically available on GitHub-hosted ARM runners.
- `justfile` calls scripts as executables; some checked-in script modes may not be executable. Either commit executable bits correctly or call `bash scripts/...` consistently.

No desktop work package should claim an end-to-end completed product until these are resolved. Independent UI skeleton work can proceed concurrently.

## 3. Versioned delivery scope

### R0 — Restore a trustworthy CLI reference path (blocking foundation)

Commands `validate`, `plan` and `boot` must work consistently for the signed example. Both CI boot matrix legs must boot the correct rootfs, reach a real guest login, execute `uname -a`, capture its kernel identity, shut down QEMU deliberately, and return a meaningful smoke-test status. A signing tool must reissue signatures when a signed file changes.

### R1 — Native desktop vertical slice

Build the GPUI Kit desktop shell with open/select, manifest and trust display, preflight, start, live serial/log view, stop/restart, and truthful errors. The desktop calls Rust libraries, **not** a CLI subprocess. Ship developer-runnable binaries on qualifying hosts. Rootfs and runtime assets may still be developer-provisioned for this phase.

### R2 — User-installable, offline-capable v1

Implement verified ZIP `.glass` import, persistent installed-glass registry, isolated writable state, bundled/discovered QEMU and runtime assets, trusted publisher configuration, native installers, signed releases, platform validation, and a complete nontechnical open → verify → run → stop flow.

### R3 — Explicitly deferred after v1

Rich guest framebuffer, embedding guest HTTP UI, guest agent/RPC, auto-updating guest packages, cloud registry/catalog/JFrog sync, sophisticated snapshot/diff, multi-world concurrency, enterprise trust deployment, automatic guest-service discovery, and non-generic hardware. These require separate design and acceptance decisions.

## 4. Functional requirements (tests use these IDs)

### 4.1 Opening and installing bundles

- **F-01 (R1):** A file picker and drag/drop open a directory-form `.glass/` for developers. The app displays product name, identifier, version, description, selected architecture/profile, publisher name/identity, and readable validation status.
- **F-02 (R2):** Double-click a serialized `.glass` opens Rootglass via OS file association; opening from the in-app picker is equally supported.
- **F-03 (R2):** Before install or run, validate the manifest, verify DSSE signature and publisher trust, and verify *all signed files*. Tampering, unsigned extra ZIP entries, missing subjects, unsafe paths, invalid signature, unsupported schema, and unknown keys block launch. Do not allow "Run anyway" for invalid/untrusted bundles in production mode.
- **F-04 (R2):** Import a verified ZIP into an app-managed immutable content store using an atomic staging/rename transaction. Do not execute, trust, or present extracted files as installed before the complete integrity check passes.
- **F-05 (R2):** Installed library persists name/version/publisher/content digest, original source, import time, runtime readiness, and last selected state. Support list, search, open, remove (with confirmation), and reimport/update.
- **F-06 (R2):** An update of the same bundle identifier with a different version/digest creates a separate immutable release; it never mutates an installed release or silently overwrites user state.

### 4.2 Preparing a world

- **F-07 (R1):** Preflight checks signature/trust, host architecture, QEMU presence/compatibility, required kernel/initramfs/rootfs/state files, memory/CPU limits, local disk space and selected acceleration. Display actionable failures before enabling Boot.
- **F-08 (R1):** Display a human-readable launch summary (guest architecture, kernel/profile, CPU/RAM, rootfs, state, network mode, accelerator). Advanced diagnostics may reveal the exact QEMU command, with paths and arguments safely quoted; never shell-evaluate that string.
- **F-09 (R2):** Runtime asset resolver locates versioned app-provided kernels/initramfs and bundled QEMU independently of the current working directory; verify their integrity before use. Optional system QEMU override is an explicit developer setting.
- **F-10 (R2):** Network defaults to `none` unless the signed manifest explicitly requests `user`. No inbound host port forwarding or bridged/tap networking in v1 without an explicit, scoped future policy.
- **F-11 (R2):** Acceleration selection is based on real CPU/guest architecture compatibility, OS capability and accessible hypervisor APIs. If required but unavailable, fail preflight; if optional, use and disclose TCG fallback. Do not claim KVM on generic x64 hosts for an aarch64 guest.

### 4.3 Running, monitoring and stopping

- **F-12 (R1):** Start one guest at a time and expose lifecycle states `Stopped → Validating → Verifying → Ready → Starting → Booting → Running → Stopping → Stopped`, with `Failed` on any error. At most one transition owner controls each run.
- **F-13 (R1):** A bounded, asynchronously drained serial stream renders in the UI with timestamps optional, pause/clear/copy and scrollback. Raw output also goes to a bounded on-disk diagnostic log.
- **F-14 (R1):** Support guest-console input: keyboard text and Enter from the UI are written to the actual guest serial channel. The UI must retain focus and distinguish logs from an interactive terminal.
- **F-15 (R1):** Boot is considered *Running* only after an explicit boot-ready signal defined for the selected profile (the initial example: its real login prompt). QEMU process spawn is **not** proof of boot success.
- **F-16 (R1):** Stop sends a guest-friendly shutdown request where available, waits a bounded period, escalates to QEMU quit/terminate, and reaps the child. Closing the app also cleans up owned QEMU children. Repeated Stop and cancellation are idempotent.
- **F-17 (R1):** Unexpected QEMU exit, startup timeout, kernel panic, root mount failure, or initramfs emergency shell surfaces `Failed` with preserved details. Exit code 0 alone is not proof of successful guest boot.
- **F-18 (R2):** Restart stops and restarts the selected release with the same persistent state. Changing state while running is prohibited; changing it is allowed only while stopped.

### 4.4 State / isolation

- **F-19 (R1):** Keep the immutable base rootfs attached read-only. A bundled state image is a *template*, never the mutable installed copy. The application copies or creates local instance state under its own data directory.
- **F-20 (R2):** An instance has exactly one selected writable state image and exclusive ownership while running. Multiple named local states per glass are allowed, one active at a time. Preserve state across reboot; reset-to-pristine requires confirmation.
- **F-21 (R2):** Distinguish **second block device** from **actual writable overlay filesystem**. Today the runtime only models/attaches a state disk; that does *not* magically make a readonly rootfs writable. For v1 either (a) implement and test guest initramfs `overlayfs` over readonly rootfs with state as upper/work, or (b) explicitly document read-only-root behavior and only support guest apps that persist under the separately mounted state disk. **Preferred v1:** (a), with upper and work on the same state filesystem; mount by stable LABEL/UUID, never by `/dev/vdX` enumeration.
- **F-22 (R2):** Never modify the signed bundle/rootfs when resetting, booting, installing, or removing an instance. Protect active state from deletion; clone/copy atomically where needed.

### 4.5 Trust, signing and publishing

- **F-23 (R0):** A documented `rootglass sign` development/release command deterministically enumerates the file set, writes the DSSE in-toto-style statement, signs using Ed25519 and verifies the result. Private keys are supplied by a secret path/agent, never committed. Signing changes to fixtures are atomic with dependent manifest changes.
- **F-24 (R2):** A configurable local trust store identifies trusted publisher key IDs and public keys; separate compiled-in sample fixture keys from production trust. Unknown/revoked keys fail closed. Key rotation and offline revocation policy are documented; online JFrog/Sigstore evidence remains optional.
- **F-25 (R2):** A `rootglass package` command creates a canonical production ZIP with every file signed except the DSSE envelope. Its output verifies with the exact runtime verifier and installs/runs from disk, with no access to the source directory.
- **F-26 (R2):** Signing, parsing, installation and running are memory/disk bounded. Refuse suspicious ZIP compression ratios, excessively large manifest/envelope, archive traversal/symlink escapes, duplicate normalized paths, and any extraction path outside the destination.

## 5. Desktop interaction and design specification

**Framework is fixed:** [Longbridge GPUI Kit](https://github.com/longbridge/gpui-kit), exact dependency `gpui-kit = "=0.9.0"`, default `gpui-pre` snapshot (0.3.11 with Kit 0.9.0). Use Kit's reexported GPUI API and its component library; do **not** use Tauri, direct Zed `main`, optional `gpui-fast`, or a JavaScript shell. Pin/commit `Cargo.lock`. Use the upstream [GPUI Kit getting started](https://gpui-kit.com/docs/getting-started/), [installation guide](https://gpui-kit.com/docs/installation/), and actual tagged examples; do not guess component method names. Kit's published macOS minimum is 15; test Windows/Linux rather than assuming support means build succeeds.

### 5.1 Window structure

Single primary window, native appearance, keyboard-accessible controls, and sensible narrow resizing:

```text
┌─────────────────────────────────────────────────────────────────────┐
│ Rootglass                                     Import…   Settings    │
├───────────────────┬─────────────────────────────────────────────────┤
│ Library / Recent  │ Hello World  0.1.0                               │
│ • Hello World     │ Signed by Rootglass Examples  [Verified]       │
│                   │ aarch64 / virt-linux  ·  512 MiB  ·  1 CPU       │
│                   │ Readiness: Ready / Missing assets / Blocked     │
│                   │ State: default ▼          [Plan] [Boot]         │
│                   ├─────────────────────────────────────────────────┤
│                   │ Status: Stopped / Booting / Running / Failed    │
│                   │ [Stop] [Restart] [Reset state…]                 │
│                   ├─────────────────────────────────────────────────┤
│                   │ Console / Logs             Copy  Clear  Export  │
│                   │                                                 │
│                   │ rootglass-hello login:                          │
│                   │                                                 │
│                   ├─────────────────────────────────────────────────┤
│                   │ Console input: [_______________________] [Send] │
└───────────────────┴─────────────────────────────────────────────────┘
```

### 5.2 Screens / components

1. **Empty library:** "Open a .glass bundle" primary action; drag/drop target; no fabricated sample installation.
2. **Verification/preview:** name/version/description/publisher and signature status. Distinguish invalid manifest, signed-but-untrusted, tampered and verified/trusted. A trust badge is only green after full integrity + trust verification.
3. **Preflight:** CPU/RAM, guest architecture, QEMU path/version, acceleration mode, runtime assets and disk requirements. Disabled Boot explains the exact blocker.
4. **Running world:** status, elapsed time, serial console view with bounded scrollback, input, Stop, Restart, diagnostic details.
5. **Library/state:** named instances, selected persistent state, new/clone/reset/delete; destructive operations require explicit confirmation and are unavailable while running.
6. **Settings/diagnostics:** app/runtime/QEMU versions, asset paths, trust-store summary, exported logs, optional developer-only system-QEMU override. Never ask users to paste a private key into UI.

### 5.3 UI behavioral rules

- No synchronous filesystem scanning, signature hashing, ZIP extraction or child-process wait on the GPUI render thread.
- Model `Loading`, `Ready`, `Blocked`, `Failed`, `Running` explicitly; never show a success state on an unverified asynchronous call.
- Validate on each open/import and again immediately before run, in case files changed.
- Disable actions during transitions; allow user cancellation where safe.
- Console text must remain selectable/copyable, preserve line breaks, and avoid unbounded memory growth. Avoid sending millions of character updates one UI event at a time; batch.
- Keyboard: Open (`Cmd/Ctrl+O`), Stop (`Cmd/Ctrl+.`), switch focus between console/input, accessible focus rings, correct screen-reader labels. No blanket global keystroke capture that steals guest input from native shortcuts.
- Prefer system-native window/dialog behavior. Do not reimplement custom frameless chrome before the basic app works.

## 6. Architecture and precise module boundaries

### 6.1 Dependency direction (avoid cycles)

Current `rootglass-qemu` depends on `rootglass-core::LaunchRequest`. Therefore **do not put code in rootglass-core that imports rootglass-qemu**: that creates a Rust dependency cycle. Add a small application-service/integration crate:

```text
rootglass-desktop (GPUI Kit binary) ──┐
                                     ├──> rootglass-app (application service / supervisor)
rootglass-cli (CLI binary) ──────────┘          │
                                               ├──> rootglass-core (validation, plan, types)
                                               ├──> rootglass-qemu (QEMU adapter → core types)
                                               ├──> rootglass-signature (DSSE, trust)
                                               └──> rootglass-manifest (TOML schema)
```

The `rootglass-app` crate owns installed-glass records, import transactions, runtime orchestration, cancellation, lifecycle event streaming, state instances, session locks, and cleanup. It has **no GPUI imports**. Keep `rootglass-core` as UI-independent policy/preflight types; keep `rootglass-qemu` as typed command/process adapter. This fulfills "desktop uses the core library directly rather than shelling out to CLI" via the shared app service.

### 6.2 Proposed repository layout

```text
crates/
  rootglass-manifest/            # existing; schema
  rootglass-signature/           # existing; verify + planned signing primitives
  rootglass-core/                # existing; preflight, launch request, profiles, storage model
  rootglass-qemu/                # existing; QEMU adapter, add supervised process + QMP
  rootglass-app/                 # NEW: app service + import, registry, lifecycle, state
  rootglass-cli/                 # existing; preserve CLI, delegate to app service
  rootglass-desktop/             # NEW: GPUI Kit binary and views
scripts/
  materialize-example-assets.sh  # existing, harden
  smoke-boot-example.sh          # existing, replace prompt-only criterion
  ...                           # signing/package/release scripts where justified
docs/
  app-spec.md                    # THIS spec (authoritative target)
  architecture.md                # existing, align when interfaces change
  bundle-format.md               # existing, align archive decisions
  signatures.md                  # existing, trust/release format
  overlays.md                   # existing, specify real guest overlay behavior
  example-assets.md              # existing, fix stale release example
.github/workflows/
  ci.yml                         # existing core checks and boot smoke
  desktop.yml                    # NEW, platform builds with explicit dependencies
  release.yml                    # NEW, tagged release artifacts, provenance and notices
```

### 6.3 Typed application-service contract

Design equivalent Rust types (exact API signatures can change with tests; **semantics cannot**):

```rust
struct GlassId(String);                 // identifier + version + content digest
struct RunId(String);                   // unique per process session
struct InstalledGlass { /* id, name, version, publisher, immutable path */ }
struct PreflightReport { /* verified trust, resolved assets, host checks, warnings */ }
struct RunOptions { /* selected state ID, optional safe overrides */ }

enum RuntimeEvent {
    StateChanged { run: RunId, state: LifecycleState },
    SerialChunk { run: RunId, bytes: Vec<u8> },
    LogLine { run: RunId, level: LogLevel, message: String },
    GuestReady { run: RunId, evidence: String },
    Exited { run: RunId, exit_code: Option<i32>, expected: bool },
    Failed { run: RunId, error: AppError },
}

trait AppService {
    fn inspect(&self, source: &Path) -> Result<GlassSummary, AppError>;
    fn import(&self, archive: &Path) -> Result<InstalledGlass, AppError>;
    fn list_installed(&self) -> Result<Vec<InstalledGlass>, AppError>;
    fn preflight(&self, glass: &GlassId, state: Option<&str>)
        -> Result<PreflightReport, AppError>;
    fn start(&self, glass: &GlassId, options: RunOptions) -> Result<RunId, AppError>;
    fn stop(&self, run: &RunId) -> Result<(), AppError>;
    // plus a bounded subscription/event-receiver API and serial input method
}
```

A supervising worker holds the child process, serial write handle, QMP control handle, ring buffers and cancellation token. The GPUI thread owns presentation state only. Use explicit channel ownership, bounded queues, and generation/run IDs so stale events from an old session cannot mutate a newer view.

### 6.4 QEMU process contract

- Construct `Command` arguments as an array, not a shell command; record an escaped string for diagnostics only.
- For desktop/CI use independent serial I/O and monitor/QMP, not `-serial mon:stdio` with inherited terminal. Linux/macOS can use pipes/PTY; Windows use supported pipes or named endpoints. Give a tested implementation for each qualified host.
- Rootfs drive is explicitly readonly; writable state drive is an independent mutable file under the app data root.
- Guest root identification uses `root=LABEL=rootglass-root` or stable filesystem UUID; state uses `rootglass-state` or UUID. Device-order-dependent paths such as `/dev/vda` are forbidden in the example's production boot contract.
- Expose and test the mapped rootfs drive, state drive and QEMU acceleration in a typed launch plan. Fix disk-order ambiguity rather than adjusting expected logs.
- QMP used for shutdown and process status. On Windows use a tested locally bound control endpoint; do not assume Unix-domain sockets exist everywhere.
- State machine advances to Running only on a real guest-ready condition; timeout/failure propagates to a nonzero test exit.
- A process killed by test timeout is not "successful boot" even if an earlier matching string appeared.

### 6.5 Filesystem and application data

Use OS-managed per-user directories, not relative `.rootglass/` in the desktop release:

```text
<app-data>/rootglass/
  registry.json                    # schema-versioned, atomically written
  packages/<content-sha256>/       # immutable verified package extraction
  instances/<glass-id>/<state-id>/ # writable state and instance metadata
  sessions/<run-id>/               # QEMU logs, control sockets, pid metadata
  runtime/<profile-version>/       # pinned/verified kernel and initramfs
  trust/                           # trusted publisher records + revocations
  settings.json                    # explicitly versioned
```

Use established platform directory-location APIs. Directory form in the source repository remains developer-only and must not be silently rewritten by a production run. Use a scoped lock (per state image) to prevent two writers, even across multiple Rootglass processes. Avoid following attacker-controlled symlinks during import or state deletion.

### 6.6 Bundle and cryptographic boundary

The serialized `.glass` remains a ZIP with `rootglass.toml` and `signatures/glass.dsse.json`. Its in-toto-style signed subjects cover every other production archive file. The signed statement binds names to SHA-256 hashes; the DSSE envelope binds the statement to an Ed25519 publisher identity.

**Import transaction:** open source → parse limits and safe paths → validate schema/signature/trusted key → hash every production subject (prefer streaming) → stage extraction into new temporary dir → verify staged file set and sizes → atomic move to digest-addressed immutable store → write registry atomically. Never accept partially verified extraction.

**Explicit unresolved decision:** sample fixture `.img` files are excluded from development-directory verification; production archives must include them as signed subjects. Never carry this dev exception into production ZIP verification.

**Fixture reproducibility:** keep fixture public key in source for tests, generate test key from a deterministic *nonsecret* fixture seed or use ephemeral test keys and checked-in signed examples through a documented fixture-generation tool. No production secret belongs in Git.

## 7. Detailed guest boot and state contract

### 7.1 Reference Linux guest

- Use a pinned Alpine Linux aarch64 kernel/initramfs/minirootfs set, not a mutable/latest-stable combination without checksum verification.
- Format rootfs ext4 `-L rootglass-root` and state ext4 `-L rootglass-state`; verify labels and contents when images already exist.
- Ensure kernel/initramfs contain modules required for QEMU virtio-mmio, virtio block, ext4, root LABEL lookup, and (for preferred writable implementation) overlayfs.
- Provide valid Alpine init and a serial getty on `ttyAMA0`. A locked root account or no getty is not an acceptable smoke-test fixture.
- Production release must not ship the test-only open root login credentials. The local smoke fixture may use a deliberate disposable CI-only credential or scripted test authorization, with networking disabled.

### 7.2 Writable-root behavior

Do not conflate existing `writable_overlay = "state.img"` with actual Linux OverlayFS. QEMU currently attaches a separate block device. Choose and implement one of these explicitly:

- **Preferred v1:** initramfs mounts immutable rootfs as lower; mounts a writable state filesystem with separate `upper`/`work` directories; mounts overlayfs merged root and `switch_root`s. Confirm file written in guest survives restart while lower image SHA-256 is unchanged. Ensure initramfs integration is versioned, tested and does not mutate signed assets.
- **Allowed temporary R1:** mount immutable rootfs read-only and persist user data only under an explicitly mounted `/state` directory. Disable claims that root writes persist. Do not advertise reset/snapshots as working before implementation.

Avoid silently changing signed `kernel_args` during runtime; runtime-required root arguments must have an explicit, documented merge policy.

## 8. Platform, dependencies and packaging

### 8.1 Framework constraints

- `gpui-kit = "=0.9.0"`; default `gpui-pre` backend; no direct additional incompatible GPUI versions.
- One GPUI Kit dependency for the UI crate, default component/assets features initially.
- Keep `rootglass-desktop` excluded from `cargo test --workspace` on headless Ubuntu until proper desktop toolchains/packages are installed in dedicated build jobs (e.g. with `default-members` or explicitly selected core packages). Make desktop build job explicit, not silently skipped forever.
- GPUI source/tests/examples for the **tagged Kit version** are the source of truth for API shapes. Do not invent methods based on an older Zed blog post.
- Rust stable, locked dependencies and reviewed updates.

### 8.2 Host/acceleration qualification

| Host | Initial target behavior | Mandatory verification |
|---|---|---|
| macOS 15+, Apple Silicon | GPUI Kit GUI + aarch64 QEMU; HVF if genuinely available | Build, app launch, boot, stop, host API checks, installer signing |
| macOS Intel | GPUI may run but aarch64 guest generally needs TCG | Explicit slow fallback and usability decision; no false HVF claim |
| Ubuntu 24.04 aarch64 | GPUI Kit + QEMU; KVM only if device and guest architecture permit | Build, boot; KVM-specific tests on a KVM-capable runner |
| Ubuntu 24.04 x86_64 | GPUI Kit + QEMU TCG | Build, boot, sane timeout and diagnostics |
| Windows 11 (x86_64 / ARM64 separately) | GPUI Kit + packaged QEMU; WHPX only on compatible CPU/guest path | Build, installer, guest console, stop, QMP endpoints and no orphaned process |

Do not conflate a GitHub `ubuntu-24.04-arm` runner with KVM-enabled hardware. Hosted boot matrix can validate TCG on two host architectures; real acceleration tests require a qualifying self-hosted/larger runner or actual supported hardware.

### 8.3 QEMU and release packaging

- The **developer** CLI may use a system QEMU executable found on PATH.
- The **end-user** app must ship an appropriate QEMU binary/runtime package or a separately installed, user-friendly companion package with fully automated discovery. Decide this in issue #20, documenting target-specific binaries, licensing, native library dependencies, checksums, vulnerabilities and updates.
- Ship every required runtime asset offline for a sample v1 package. No surprise Alpine CDN download on first launch.
- Native app signing/notarization where applicable; include MIT project license and all bundled upstream license/notice obligations (including QEMU/GPUI Kit and Alpine).
- Keep signed app distribution separate from signed `.glass` publisher trust; the two are independent trust decisions.
- Before declaring v1, a clean supported host must install, open a signed ZIP, boot and stop without Rust, Cargo, QEMU, or root privileges installed separately.

## 9. Failure, privacy, and performance policy

- Offline use is the default. No telemetry, accounts, cloud registry or upload of local artifacts/logs in v1.
- No unsigned/untrusted production bundle runs; no executable code from archive metadata runs on the host. Do not shell out with manifest-provided arguments.
- Manifest resources have safe minimums/maximums; show host memory pressure and insufficient disk space before boot.
- Use bounded in-memory output (e.g. last 20,000 lines or a byte cap), rotating file logs and a maximum log retention policy; report truncation rather than freezing.
- Handle child stdout/stderr backpressure, serial binary/ANSI escape content, CRLF, and UTF-8 decode errors. Sanitize terminal escape sequences before rendering in the GUI.
- Application exit/restart must not leave writable images locked or orphan QEMU processes. Distinguish clean user stop, unexpected process crash and guest boot failure.
- Security boundaries: ZIP extraction outside destination, symlink traversal, zip bombs, untrusted publisher, modified signed bytes, tampered installed package, state naming collisions, child process injection, nonlocal QMP access.
- A runtime exception must include code, phase, user-facing summary, technical detail and a suggested correction; never replace it with "boot failed" alone.

## 10. Acceptance tests and executable definition of done

### 10.1 Required automated checks

**A-01** `cargo fmt --all -- --check`, locked core Clippy and tests pass. Desktop build/check has its own correctly provisioned jobs.

**A-02** Sign a fixture deterministically, verify it, modify one signed byte, and assert verification fails with the correct reason. An unknown publisher must fail before QEMU spawn.

**A-03** Missing rootfs/kernel/state or incompatible architecture produces preflight failure without spawning QEMU.

**A-04** On both x64 hosted TCG and arm64 hosted TCG, launch a *freshly materialized* example with a bounded PTY/serial harness. The test must prove:
1. Correct rootfs mounted (no initramfs emergency shell).
2. `rootglass-hello login:` appears.
3. Test harness sends the fixture-specific login credentials over serial and receives a real shell prompt.
4. Harness sends `uname -a`, and **observes a response beginning with `Linux rootglass-hello`**, containing the expected aarch64 machine token. Check a command-completion sentinel as well; do not pass on a sent command echoed back.
5. Harness requests controlled shutdown, waits for QEMU process exit, and checks that no child remains.
6. Every timeout, kernel panic, root mount error, failed login, missing uname response, unexpected exit or forced kill returns nonzero. Capture console logs as CI artifacts on failure.

**A-05** With a local state image, write a file inside the guest, restart, and read back the same content; hash the readonly base before and after to assert immutability (after the preferred OverlayFS path is implemented).

**A-06** Build a ZIP `.glass` through the package/sign tool, then import from outside source checkout, verify, boot and stop. Reject unsigned files, traversal, symlinks, corrupted bytes and zip bombs.

**A-07** Supervisor tests with a fake child process cover spawn failure, timeout, cancellation, stop idempotence, EOF, output flood, unexpected exit, and stale events from a previous RunId. All children are reaped.

**A-08** Headless GPUI model tests validate the exact state of enabled actions and errors. GUI smoke checks exercise file-open, verification view, Boot/Stop and log/input where supported.

**A-09** On each shipping OS, app installer and file association work on a clean system; no preinstalled developer tools required.

**A-10** Release inventory includes bundled binary hashes, SBOM/notices, signing provenance, exact runtime versions, and platform test results.

### 10.2 CI commands and job requirements

Keep the current CLI contract:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo run --locked -p rootglass-cli -- validate examples/hello-world.glass
cargo run --locked -p rootglass-cli -- plan examples/hello-world.glass
bash scripts/smoke-boot-example.sh
```

Adapt workspace selection for the new desktop crate explicitly, while retaining equivalent checks for all supported packages. Provide targeted commands for each new crate and a no-QEMU fake supervisor test path. Do not mask an error with `|| true` or a success-grep that ignores subprocess exit status.

### 10.3 Release definition of done

R2/v1 is complete only when all of the following hold simultaneously:

- A clean qualified user machine installs and launches the signed Rootglass app.
- A genuine production-structured, signed ZIP `.glass` can be opened, trusted, imported, planned, booted and stopped offline.
- UI reflects real child, guest and signature state; serial console accepts input.
- Persistent state survives restart, reset is safe, and the signed immutable source remains unchanged.
- Tests A-01 through A-10 pass or documented platform limitations narrow the advertised supported matrix.
- Documentation, notices, licensing, error messages, recovery and uninstaller data-retention behavior are checked.

## 11. Development and release policy

- Keep core crates and CLI as a usable standalone developer product throughout development.
- Prefer isolated integration tests over GUI automation for expensive state-machine behavior.
- Every domain error has a negative-path test. Security tests must prove refusal before starting QEMU.
- A refactor that changes the `rootglass.toml` signed bytes must regenerate/reverify fixture DSSE in the **same PR**.
- Never resolve a CI failure by weakening signature verification or by changing success regex to unrelated boot text.
- Run `cargo fmt`, targeted `cargo test`, relevant Clippy, and a fixture validation on each code PR; run platform-specific tests for process/GUI changes.
- Update this spec's "observed baseline" via a follow-up change when features ship; do not mark planned features as existing just because an issue is closed.

## 12. Swarm work packages (assign one owner per package)

Each agent should receive exactly one package ID, explicit path ownership, acceptance IDs and dependencies. Names below are a **task DAG**; agents can execute independent branches concurrently but the integrator must merge them in order. Small agents should not edit shared files outside their owned package.

| ID | Phase | Work package and owned paths | Depends | Acceptance |
|---|---|---|---|---|
| S00 | R0 | Repair fixture manifest/DSSE/trust pairing and record reproducible verification. Own example fixture + signature fixture tests. Coordinate signing tool changes. | none | A-01, A-02 |
| S01 | R0 | Implement deterministic sign/package primitives and CLI surface, no production secrets. Own `rootglass-signature` and dedicated signing module/tool. | none | A-02, A-06 unit portion |
| S02 | R0 | Correct rootfs boot path/getty/test login and implement serial `uname -a` E2E smoke. Own materialize/smoke scripts and fixture guest init. | S00; coordinate S01 | A-03, A-04 |
| S03 | R1 | Add process-safe QEMU supervisor I/O/QMP, deadlines, cancellation, cleanup. Own `rootglass-qemu` + fake child tests. | existing core types | A-07 |
| S04 | R1 | Implement `rootglass-app` orchestration API/event contract; CLI delegates to it. Own new app crate and CLI adapter. | S03 | A-03, A-07 |
| S05 | R2 | Verified ZIP import, path/size limits, content store, registry transactions. Own `rootglass-app/src/import*`, `registry*`. | S01 + S04 | A-06 |
| S06 | R2 | Persistent instance state and guest overlayfs or explicitly bounded read-only interim behavior. Own storage/state module and guest init. | S02 + S04 | A-05 |
| S07 | R2 | Trusted publisher stores, unknown-key/revocation behavior, trust UI-facing reports. Own signature trust module + service adapter. | S01 + S04 | A-02, A-06 |
| S08 | R1 | GPUI Kit `=0.9.0` desktop skeleton, main window, UI model, fake service, preview/preflight; no QEMU spawning in UI. Own desktop crate. | S04 interface contract (mock sufficient) | A-08 |
| S09 | R1 | GPUI event/serial console, keyboard input, Boot/Stop integration with `rootglass-app`. Own desktop views/controller. | S03 + S04 + S08 | A-07, A-08 |
| S10 | R2 | QEMU bundled/discovered resolver and versioned runtime assets. Own QEMU resolver + asset resolver; issue #20. | S03 | A-03, A-09 |
| S11 | R2 | Cross-platform desktop build/installer/file-association/signing workflows and notices. Own release workflow and packaging config. | S08 + S10 | A-09, A-10 |
| S12 | R2 | Integration and QA: production ZIP on each OS, threat tests, docs, support UX and release gate. Own CI matrices and final docs. | S05–S11 | A-01–A-10 |

**Parallelization:** Start S00, S01, S03 and S08 (mocked service contract) concurrently. S02 follows fixture stabilization; S04 follows supervisor API stabilization; S05/S06/S07/S10 can work after their prerequisites with disjoint paths. S09 integrates desktop after service exists. S11 begins packaging once minimal desktop and resolver work. S12 is the final integration owner.

**Shared-file ownership:** One integration agent owns repository-root `Cargo.toml`, `Cargo.lock` and generic CI workflows. Workers needing dependency edits provide the exact proposed diff in their PR and coordinate merge order. No agents concurrently update the same file on `main`.

### 12.1 Copyable worker handoff template

```text
You are Worker <ID> on branning/rootglass.
Read docs/app-spec.md and your assigned source files.
Implement only package <ID> (paths and prerequisites in `12).
Before writing code, report the existing symbols you will reuse.
Keep public interfaces compatible with the application-service contract (`6.3).
Implement positive, failure and cleanup-path tests.
Do not edit unrelated paths or weaken verification/CI.
Run relevant cargo fmt, targeted test, clippy and scenario checks.
Open a focused PR with summary, changed paths, test evidence, risks, and acceptance IDs.
If blocked by missing interface, open an issue or provide a tiny compileable contract to the integrator. Do not invent an unrelated API or silently claim completion.
```

### 12.2 Integration-owner checklist

- Keep a single dependency DAG/owner map and prevent duplicate file ownership.
- Merge foundation fixes and rerun baseline tests before interpreting GPUI failures.
- Review each PR for false-positive tests, security bypass, missing cleanup and unbounded logging.
- Run workflow tests from the merge commit, not just an agent's branch.
- Validate that the exact GPUI Kit version and lockfile are committed and tested on a real desktop host.
- Advance release phase only on objective acceptance evidence; close issues after functionality and CI exist, not when code has merely been drafted.

## 13. Outstanding product decisions requiring written ADRs

These are not permission to stall S00–S09; use the defaults below until an ADR changes them.

| Decision | v1 default | Required evidence / owner |
|---|---|---|
| GPUI framework | GPUI Kit exactly 0.9.0 / default backend | Tagged examples build; S08 |
| Guest UI | Serial console first; no embedded web UI | S09; web-forward after v1 |
| Execution | One world at a time per desktop process | S04; multiworld after v1 |
| Bundle contents | One read-only rootfs, optional state template; canonical ZIP | S01/S05 |
| State behavior | Prefer OverlayFS upper/work on local state disk; signed root immutable | S06 |
| Publisher trust | Explicit offline trusted keys, default deny unknown | S07 / issue #18 |
| QEMU distribution | Bundled when legally/operationally feasible; system override for developers | S10 / issue #20 |
| Guest networking | None unless manifest explicitly requests user networking; no unsolicited inbound | S04/S10 |
| Host arch acceleration | Real capability-tested HVF/KVM/WHPX, otherwise disclosed TCG | S03/S10 |
| Platform support | Advertise only OS + CPU combinations verified by installer and guest tests | S11/S12 |
| Data deletion | Uninstall policy distinguishes immutable packages from user state; destructive delete confirms | S05/S06 |
| Release signing | Separate Rootglass app/installer signing and .glass publisher signing | S11 |

## 14. Source of truth and issue map

- Repository: https://github.com/branning/rootglass
- Existing architecture: [docs/architecture.md](architecture.md)
- Signed bundle format: [docs/bundle-format.md](bundle-format.md)
- DSSE trust format: [docs/signatures.md](signatures.md)
- State/overlay model: [docs/overlays.md](overlays.md)
- First developer milestone: [docs/milestone-1.md](milestone-1.md)
- GPUI Kit desktop issue: [#25](https://github.com/branning/rootglass/issues/25)
- Signing tool: [#19](https://github.com/branning/rootglass/issues/19)
- Publisher trust: [#18](https://github.com/branning/rootglass/issues/18)
- QEMU packaging: [#20](https://github.com/branning/rootglass/issues/20)

**Final instruction to the swarm:** The deliverable is a working, verified, native Rootglass application, not a collection of unconnected crates, screenshots, or passing greps. Every milestone must leave a runnable and honestly tested vertical slice.
