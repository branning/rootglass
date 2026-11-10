# GPUI Figma evidence

This records the four designed desktop states and the limits of the headless visual check. The implementation adapts Figma to the existing GPUI Kit shell and service data.

## Captured states

| State | Figma | Screenshot evidence | Outcome |
| --- | --- | --- | --- |
| Empty and installed library | [2:24](https://www.figma.com/design/ipUaFKsIiXgLb8qPiAcnSA?node-id=2-24) | [Desktop smoke artifact](https://github.com/branning/rootglass/actions/runs/42887421305/artifacts/13796462700): `00-rootglass-library.png` and `04-verified-import.png` | Empty library renders, then a verified Hello World bundle imports into the library and its selected details are shown. |
| Untrusted publisher review | [2:91](https://www.figma.com/design/ipUaFKsIiXgLb8qPiAcnSA?node-id=2-91) | [Publisher review artifact](https://github.com/branning/rootglass/actions/runs/42888992757/artifacts/13798236345): `00-publisher-review.png` | The publisher and signing key are unverified because the signing key is not trusted; import and boot remain blocked until trust is configured. |
| Running guest console | [2:141](https://www.figma.com/design/ipUaFKsIiXgLb8qPiAcnSA?node-id=2-141) | [Desktop smoke artifact](https://github.com/branning/rootglass/actions/runs/42887421305/artifacts/13796462700): `05-guest-running.png`, `06-guest-username-input.png`, and `07-guest-password-prompt.png` | The example guest reaches its ready signal. The serial console accepts a username and Enter, then shows the password prompt; the run is restarted and stopped. |
| Launch failure and recovery | [2:192](https://www.figma.com/design/ipUaFKsIiXgLb8qPiAcnSA?node-id=2-192) | [Desktop smoke artifact](https://github.com/branning/rootglass/actions/runs/42887421305/artifacts/13796462700): `02-launch-check-failed.png` and `03-launch-checks-retried.png` | A signed fixture requiring KVM reaches the compatibility failure on a runner without `/dev/kvm`. Retry reruns inspection and retains the failure and selected state. |

Each link opens the workflow artifact ZIP; the named PNGs are inside it. The workflow retains artifacts for 14 days.

## Environment and revisions

The full desktop flow ran on GitHub Actions `ubuntu-24.04` (Ubuntu 24.04.5 LTS, image `20261101.360.1`) at `1280x800x24` under Xvfb, with `LIBGL_ALWAYS_SOFTWARE=1`, `dbus-run-session`, and `xdotool`.

The full-flow artifact is from commit `dc0dc0c49ba01543c296f54f880c908af7c28b91`. The publisher-review artifact is from merge commit `92f0e8fdf9dd7631edf091b82a0570a62674b14e`. Both commits have the same source tree, `c61c403d09fe908b289df67225def1278e7be93f`.

Reproduction commands:

```sh
gh workflow run libghostty-vt-smoke.yml \
  --ref codex/figma-launch-recovery \
  -f platform=linux \
  -f desktop_smoke=true

gh workflow run libghostty-vt-smoke.yml \
  --ref main \
  -f platform=linux \
  -f capture_publisher_review_screenshot=true
```

Both were manual Linux-only dispatches. The full guest boot and console flow is kept outside push/synchronize runs; this evidence change adds no Actions jobs.

## Interaction and accessibility coverage

The Xvfb smoke uses pointer input for bundle drops and recovery, restart, and stop buttons. It types into the guest serial console, presses Enter, and observes the password prompt. The script logs that the focused X window is the Rootglass window while entering the username; it does not assert field-level focus.

This does not test screen-reader output, the accessibility tree, keyboard-only focus traversal, or native macOS/Windows interaction. Review those on a native platform before relying on platform accessibility behavior.

## Figma handoff scope

Handoff node [2:239](https://www.figma.com/design/ipUaFKsIiXgLb8qPiAcnSA?node-id=2-239) says saved-data management and the terminal bridge remain future work; this implementation leaves both out of scope. It also notes that the Oxide tokens and button geometry are adapted locally because no linked Oxide Figma library was available, IBM Plex Sans/Mono substitute for unavailable SuisseIntl/GT America Mono, and the example glass and console output are illustrative.
