# Rootglass — Complete Application Specification and Swarm Build Contract

**Status:** Implementation contract, version 1.1, 2026-11-07  
**Repository:** https://github.com/branning/rootglass  
**Audience:** Coding agents (including small/Haiku-class agents), integration/review agents, human maintainers  
**Scope:** What currently exists, every component needed for a distributable v1 desktop application, later extensions, and executable work packages  
**Authority:** This spec describes the *target*, not a claim that missing features are implemented. Existing code remains authoritative about current behavior; tests are authoritative about what works. If an older plan conflicts with this document, follow this document for product direction and create an issue for incompatible implementation details.

## 0. Working instructions for a coding-agent swarm

1. **Read this file, then the exact implementation files named in your task.** Do not invent APIs, fields, or source files. Inspect actual code and upstream GPUI Kit examples before writing GPUI code.
2. **Do not implement the entire spec in one branch.** Work in the dependency-ordered work packages in section 12. A typical worker owns 1–3 files and one testable outcome.
3. **Do not report success merely because something compiles.** Use the acceptance tests in section 10. Preserve the existing Rust CLI while maintaining the desktop app.
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

## 2. Merged implementation baseline (checked 2026-11-07)

The S00–S11 implementation slices are merged to `main`. This inventory describes the merged feature surface; it does **not** certify every acceptance test or platform combination. Use current CI results and [docs/roadmap.md](roadmap.md) and [docs/desktop-packages.md](desktop-packages.md) for release status.

| Area | Merged implementation | Remaining v1 work |
|---|---|---|
| Manifest and bundle | Typed `.glass` manifest validation with bounded strict parsing; shared bounded signature verification and signing; development directory and ZIP archive paths; canonical signature coverage and archive safety checks | Maintain threat-model regression coverage; document archive/install recovery evidence |
| Signature and trust | DSSE/Ed25519 verification, signed fixture/publishing path, and trust reports consumed by the app | Production publisher-root provisioning, rotation/revocation operations, and release trust policy |
| Runtime and CLI | Shared application service, QEMU supervision, lifecycle events, preflight, CLI validation/planning/boot, and hosted guest boot smoke | Keep A-03/A-04/A-07 covered as runtime changes land; record platform-specific acceleration/performance evidence |
| Import and library | Verified archive import, installed-glass registry/library, and desktop library selection | Validate complete open/import/recovery behavior through GPUI; strengthen limits or migration behavior where needed |
| Guest state | Per-instance persistent state and writable-overlay handling around an immutable bundle root | Close persistence/reset behavior under A-05 on the supported guest path |
| GPUI desktop | GPUI Kit `=0.9.0` native shell integrated with the shared app service, library, trust/details, lifecycle controls, and serial console | Complete A-08 model and GUI smoke coverage; verify responsiveness, event/cancellation behavior, and user-facing failures |
| Platform packages | Linux, macOS, and Windows package builds; `.glass` association metadata and checksums | A-09 clean-install/file-open tests; installed QEMU/runtime inventory; A-10 signing, notarization, SBOM, and provenance |

Hosted CI is the functioning guest-boot test environment: x64 and arm64 TCG jobs materialize the example assets, boot the signed fixture through the CLI, authenticate over serial, run `uname -a`, and shut QEMU down. Desktop package build artifacts are review artifacts; they are unsigned and have not been validated by clean-OS installation and file-open tests.

### 2.1 Current completion rule

Treat a work package as implementation-complete when its code is merged and its required CI checks pass. Treat an acceptance ID as complete only when its stated evidence exists. A merged GPUI shell does not by itself close A-08; a successful package build does not close A-09 or A-10. Do not infer release readiness from merged PR count.

## 3. Versioned delivery scope

### R0 — Trustworthy CLI reference path (implementation merged)

The signed example, `validate`, `plan`, and `boot` workflows are implemented. Hosted x64 and arm64 CI boot the guest, authenticate over serial, run `uname -a`, and shut down deliberately. Keep this path green as a regression gate; see A-01–A-04.

### R1 — Native desktop vertical slice (implementation merged; A-08 qualification open)

The GPUI Kit desktop shell is integrated with the shared Rust application service and includes bundle selection/library, manifest and trust details, preflight, lifecycle controls, and guest console integration. It calls Rust libraries, **not** a CLI subprocess. Complete the model and GUI smoke evidence in A-08 before treating this phase as accepted.

### R2 — User-installable, offline-capable v1 (implementation partial; release gates open)

Verified ZIP import, installed-glass registry, per-instance state, trust reporting, runtime asset resolution, and native package builds are merged. Finish installed-app QEMU/runtime inventory, clean-host install and OS file-open validation, production trust operations, application/installer signing, release provenance, and a complete nontechnical offline open → verify → run → stop flow. See A-05–A-10 and S13–S15.

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
- **F-21 (R2):** Distinguish **second block device** from **actual writable overlay filesystem**. The current reference guest chooses the documented `/state` option: Rootglass keeps the rootfs readonly and mounts the separate state disk by its `rootglass-state` label at `/state`, using BusyBox-compatible `LABEL=` mount syntax. The guest does not have a writable root overlay. Guest applications that need persistence must write under `/state`. Do not claim that attaching a second disk makes `/` writable. OverlayFS upper/work support remains future work requiring a separate implementation and guest test. See [overlays.md](overlays.md).
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

### 5.4 Planned headless TUI and GPUI terminal surface

S16 adds a standalone Ratatui interface available as `rootglass tui`. It is a second frontend over the existing Rootglass service, not a new verification or lifecycle implementation. It must be testable without a display by rendering the same controller through Ratatui's `TestBackend` and a deterministic service fake.

The TUI covers the existing `validate`, `plan`, `boot`, `sign`, and `package` workflows, plus bundle open/import, installed-release management, trust/preflight diagnostics, named state once S12.2 exposes the safe service contract, and interactive guest serial input/output. The current signing API accepts an external secret-file path; agent integration can be added only through a compatible library API. The TUI never accepts pasted key bytes or displays key material. See [tui-architecture.md](tui-architecture.md) for the current service coverage and gaps.

The GPUI Kit `=0.9.0` shell hosts the same TUI session in an embedded terminal backed by libghostty. The TUI session and terminal host share a full-duplex terminal interface; the TUI calls `rootglass-app` for domain actions and the guest serial console, while the GPUI/libghostty surface owns terminal rendering, keyboard input, clipboard and resize. The desktop's `embedded-terminal` feature starts the current desktop executable in a private PTY process running the same `rootglass-tui` library; it does not invoke an arbitrary shell or dispatch individual operations through the CLI. The custom GPUI surface reads libghostty-vt screen state, applies ANSI styles, supports keyboard selection/copy/paste and resizes the PTY with its view. Closing the surface sends the TUI its normal quit key and waits for its guest cleanup before the desktop exits. Build with `GHOSTTY_VT_PREFIX` pointing at a libghostty-vt 1.0 installation and `--features embedded-terminal`. S16.6 pinned the upstream API/ABI, and the maintainer approved Linux x86_64, macOS arm64, and Windows x86_64 as qualification targets. Build/package support and native-host qualification remain separate acceptance work. Upstream labels its full embedding API internal to the macOS app and directs external embedders to `libghostty-vt`; Rootglass does not use the internal API.

## 6. Architecture and precise module boundaries

### 6.1 Dependency direction (avoid cycles)

Current `rootglass-qemu` depends on `rootglass-core::LaunchRequest`. Therefore **do not put code in rootglass-core that imports rootglass-qemu**: that creates a Rust dependency cycle. Keep application orchestration in the existing `rootglass-app` integration crate:

```text
Current frontends:
  rootglass-desktop ─────────────────────────> rootglass-app
  rootglass-cli: validate ───────────────────> rootglass-manifest
  rootglass-cli: plan/boot ──────────────────> rootglass-app
  rootglass-cli: sign/package ───────────────> rootglass-signature

Planned terminal frontends:
  rootglass-cli: `tui` ──────────────────────> rootglass-tui
  GPUI libghostty host <── terminal session ──> same TUI session
                                                   │
                                                   v
rootglass-app (application service / supervisor)
                                               │
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
  rootglass-signature/           # existing; verification, trust, signing and package primitives
  rootglass-core/                # existing; preflight, launch request, profiles, storage model
  rootglass-qemu/                # existing; QEMU adapter and supervised process I/O
  rootglass-app/                 # existing; app service + import, registry, lifecycle, state
  rootglass-cli/                 # existing; preserve CLI, delegate to app service
  rootglass-desktop/             # existing; GPUI Kit binary and views
  rootglass-tui/                 # PLANNED: Ratatui TUI session and TestBackend tests
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

**Storage compatibility policy:** `registry.json` schema version 1 is the only supported registry format. Rootglass rejects unsupported versions, unknown v1 fields, oversized files, duplicate package digests, and malformed rows without rewriting or deleting the original file. There is no automatic in-place migration. A future schema change needs a separately versioned migrator that validates the source, writes and flushes a sibling temporary file, atomically replaces the registry only after success, and preserves a recoverable copy of the source. `settings.json` is not implemented; any future settings file must have an explicit schema version and the same bounded, atomic migration behavior. Current read bounds are 16 MiB/10,000 rows for the registry, 64 KiB for a manifest, and 16 MiB for a signature envelope. Archive verification and import share limits of 256 MiB compressed, 4,096 entries, 256 MiB per file, 512 MiB expanded total, and a 200:1 compression ratio. Path validation rejects traversal, ambiguous separators, portable-name collisions, links and unsupported entry types.

### 6.6 Bundle and cryptographic boundary

The serialized `.glass` remains a ZIP with `rootglass.toml` and `signatures/glass.dsse.json`. Its in-toto-style signed subjects cover every other production archive file. The signed statement binds names to SHA-256 hashes; the DSSE envelope binds the statement to an Ed25519 publisher identity.

**Import transaction:** open source → parse limits and safe paths → validate schema/signature/trusted key → hash every production subject (prefer streaming) → stage extraction into new temporary dir → verify staged file set and sizes → atomic move to digest-addressed immutable store → write registry atomically. Never accept partially verified extraction.

Apply archive, member, aggregate expansion, compression-ratio, path, entry-count and envelope limits in the shared signature verifier as well as the app importer, signer and extractor. Mark the staged tree readonly before publishing its content-addressed path. If registry persistence fails after package publication, the package remains an unreferenced immutable object and is not listed as installed; a later import can verify and reuse it. Never rewrite an existing unsupported or corrupt registry automatically.

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
- The **end-user** app must ship an appropriate QEMU binary/runtime package or a separately installed, user-friendly companion package with fully automated discovery. See [ADR-0002](adr/0002-versioned-desktop-qemu-runtime.md) for the preview distribution decision, target-specific binaries, licensing, native library dependencies, checksums, and update policy.
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

**A-05** With a local state image, write a file under the guest's documented persistent path, restart, and read back the same content; hash the readonly base and bundled state template before and after to assert immutability. The current reference guest uses `/state`; this evidence does not claim OverlayFS behavior.

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

The executable backlog now lives in [`.ergo/backlog.jsonl`](../.ergo/backlog.jsonl), with progress/evidence in [`.ergo/journal.jsonl`](../.ergo/journal.jsonl). Use `ergo list --ready` and `ergo show <id>`; follow [the atomic agent workflow](agent-workflow.md). Ergo owns live task status, dependencies and assignments; this spec retains product requirements, acceptance criteria and the historical package index below. S12–S15 are split into independently reviewable tasks, and R3 is draft work.

For remaining implementation, assign one owner per Ergo task with explicit paths, acceptance IDs and dependencies. The original S00–S11 packages are merged; S12–S15 track acceptance and release qualification. Small agents should not edit shared files outside their owned package.

| ID | Phase | Work package and owned paths | Depends | Acceptance | State |
|---|---|---|---|---|---|
| S00 | R0 | Repair fixture manifest/DSSE/trust pairing and record reproducible verification. Own example fixture + signature fixture tests. Coordinate signing tool changes. | none | A-01, A-02 | Merged |
| S01 | R0 | Implement deterministic sign/package primitives and CLI surface, no production secrets. Own `rootglass-signature` and dedicated signing module/tool. | none | A-02, A-06 unit portion | Merged |
| S02 | R0 | Correct rootfs boot path/getty/test login and implement serial `uname -a` E2E smoke. Own materialize/smoke scripts and fixture guest init. | S00; coordinate S01 | A-03, A-04 | Merged; hosted x64/arm64 boot is the reference CI path |
| S03 | R1 | Add process-safe QEMU supervisor I/O/QMP, deadlines, cancellation, cleanup. Own `rootglass-qemu` + fake child tests. | existing core types | A-07 | Merged |
| S04 | R1 | Implement `rootglass-app` orchestration API/event contract; CLI delegates to it. Own app crate and CLI adapter. | S03 | A-03, A-07 | Merged |
| S05 | R2 | Verified ZIP import, path/size limits, content store, registry transactions. Own `rootglass-app/src/import*`, `registry*`. | S01 + S04 | A-06 | Merged |
| S06 | R2 | Persistent instance state and guest overlayfs or explicitly bounded read-only interim behavior. Own storage/state module and guest init. | S02 + S04 | A-05 | Merged; retain A-05 persistence evidence as a release check |
| S07 | R2 | Trusted publisher stores, unknown-key/revocation behavior, trust UI-facing reports. Own signature trust module + service adapter. | S01 + S04 | A-02, A-06 | Merged; production trust operations remain a release decision |
| S08 | R1 | GPUI Kit `=0.9.0` desktop skeleton, main window, UI model, fake service, preview/preflight; no QEMU spawning in UI. Own desktop crate. | S04 interface contract (mock sufficient) | A-08 | Merged |
| S09 | R1 | GPUI event/serial console, keyboard input, Boot/Stop integration with `rootglass-app`. Own desktop views/controller. | S03 + S04 + S08 | A-07, A-08 | Merged; complete A-08 desktop smoke and interaction evidence |
| S10 | R2 | QEMU bundled/discovered resolver and versioned runtime assets. Own QEMU resolver + asset resolver; issue #20. | S03 | A-03, A-09 | Merged implementation; installed package runtime inventory remains open |
| S11 | R2 | Cross-platform desktop build/installer/file-association/signing workflows and notices. Own release workflow and packaging config. | S08 + S10 | A-09, A-10 | Merged package builds; clean install, signing, and provenance remain open |
| S12 | R2 | Integration and QA: production ZIP on each OS, threat tests, docs, support UX and release gate. Own CI matrices and final docs. | S05–S11 | A-01–A-10 | Open as acceptance/qualification work; see S13–S15 |
| S13 | R1 | GPUI qualification: UI-model tests, GUI smoke for open/verify/Boot/Stop/console, render-thread responsiveness, cancellation and stale-event checks. Own desktop tests and desktop CI. | S08 + S09 | A-07, A-08 | S13.1 merged; S13.2 GUI smoke remains open |
| S14 | R2 | Clean-host install: resolve/package QEMU and versioned assets; install each platform package without developer tools; open via app and OS association; boot and stop. Own runtime inventory and install-test workflows. | S10 + S11 + S13 | A-03, A-09 | After S13; platform evidence required |
| S15 | R2 | Release integrity: app/installer signing, macOS notarization, SBOM/notices, hashes, provenance, and final offline production-archive walkthrough. Own release workflow and evidence docs. | S14 | A-02, A-06, A-10 | Release gate |
| S16 | R1/R2 | Headless Ratatui TUI for all current Rootglass commands and app workflows, with the same TUI hosted in GPUI through public libghostty-vt and a custom GPUI view. Own `rootglass-tui`, CLI adapter, parity docs and desktop terminal host. | S04, S08–S09; S12.1/S12.2/S12.4 for gated workflows; S13.2 for GUI acceptance | Applicable F-01–F-26; A-02/A-05/A-07/A-08 | S16.1–S16.5, S16.9, S16.10 merged; S16.6 PTY prototype and repeatable Linux CI smoke complete; maintainer approved Linux x86_64, macOS arm64, Windows x86_64; native/GPUI qualification, S16.7 host, and S16.8 cross-surface acceptance remain open |


**Shared-file ownership:** One integration agent owns repository-root `Cargo.toml`, `Cargo.lock` and generic CI workflows. Workers needing dependency edits provide the exact proposed diff in their PR and coordinate merge order. No agents concurrently update the same file on `main`.

### 12.1 Copyable worker handoff template

```text
You are Worker <ID> on branning/rootglass.
Read docs/app-spec.md and your assigned source files.
Run ergo where and ergo show <task-id>; inspect prerequisite outcomes.
Implement only the assigned Ergo task within package <ID>.
Use docs/agent-workflow.md for atomic commits and progress recording.
Before writing code, report the existing symbols you will reuse.
Keep public interfaces compatible with the application-service contract in section 6.3.
Implement positive, failure and cleanup-path tests.
Do not edit unrelated paths or weaken verification/CI.
Run relevant cargo fmt, targeted test, clippy and scenario checks.
Open a focused PR with summary, changed paths, test evidence, risks, and acceptance IDs.
If blocked by missing interface, open an issue or provide a tiny compileable contract to the integrator. Do not invent an unrelated API or silently claim completion.
```

### 12.2 Integration-owner checklist

- Keep the dependency DAG/owner map in Ergo and prevent duplicate file ownership; do not maintain a competing Markdown task queue.
- Merge foundation fixes and rerun baseline tests before interpreting GPUI failures.
- Review each PR for false-positive tests, security bypass, missing cleanup and unbounded logging.
- Run workflow tests from the merge commit, not just an agent's branch.
- Validate that the exact GPUI Kit version and lockfile are committed and tested on a real desktop host.
- Advance release phase only on objective acceptance evidence; close issues after functionality and CI exist, not when code has merely been drafted.

## 13. Outstanding product decisions requiring written ADRs

Use the defaults below for remaining implementation unless an ADR changes them.

| Decision | v1 default | Required evidence / owner |
|---|---|---|
| GPUI framework | GPUI Kit exactly 0.9.0 / default backend | Tagged examples build; S08 |
| Embedded terminal | Use libghostty; prove the external API/ABI and platform package before shipping | S16.6 prototype and maintainer platform review; see [tui-architecture.md](tui-architecture.md) |
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
