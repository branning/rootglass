# Security Policy

Rootglass runs complete guest environments on a user machine, so repository changes should be reviewed with a conservative supply-chain posture.

## GitHub Actions policy

GitHub-authored actions are allowed when they are appropriate for the workflow.

Agents and contributors must not add a non-GitHub-authored GitHub Action directly to a workflow without approval.

Before adding any non-GitHub-authored action, the agent or contributor must:

1. Open a GitHub issue describing the proposed action.
2. Include the action name, publisher, intended version pin, reason for use, and what permissions/network access the workflow needs.
3. Wait for the issue to receive the `action-approved` label.
4. Pin the action to an explicit version or commit according to the recommendation in the approval issue.

This applies to marketplace actions, reusable workflow references, and other third-party automation executed by GitHub Actions.

## Existing third-party actions

The initial CI workflow uses the following third-party actions after manual review:

- `dtolnay/rust-toolchain@stable`
- `Swatinem/rust-cache@v2`

Future changes to replace, add, or materially reconfigure third-party actions should follow the approval process above.

## Issue label

Use the `action-approved` label to mark approval for a specific proposed action. Approval is scoped to the issue and should not be treated as blanket approval for all future uses.

## General guidance

- Prefer official GitHub actions where they are sufficient.
- Prefer small, widely used actions with clear maintenance history.
- Avoid actions that require broad permissions unless the workflow genuinely needs them.
- Keep workflow `permissions` as narrow as practical.
- Prefer explicit version pins over floating branches for non-GitHub-authored actions once the workflow stabilizes.
