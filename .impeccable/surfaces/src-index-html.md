---
version: 1
slug: "src-index-html"
primary_target: "src/index.html"
related_targets: ["src/app.js","src/styles.css"]
---

THESIS
The storage destination is the vault. People choose a vault first, then manage its backup sources, schedules, and restore points in that context.

OWN-WORLD
Extend the existing SaveState Windows app: dark flat surfaces, restrained mint for actions, familiar controls, no new visual identity.

STORY
Cloud - Personal is always present and cannot be disconnected. Every connected B2, R2, S3-compatible, or MinIO destination becomes another vault. Existing profiles remain attached to their existing destination.

FIRST VIEWPORT
The Vaults page immediately shows Cloud - Personal and each custom vault with its owner, connector, and count of scheduled sources. Add Vault is adjacent to the heading. Choosing a vault reveals its sources and schedules; restore access stays within that vault.

FORM
One overview and one detail state within the existing app shell, optimized for the narrow Windows window. Connect form is inline. Source/schedule editing retains the familiar modal but its vault is fixed by the selected detail.

FINISH
Preserve encryption, local credentials, entitlement, quota, restore, and deletion behavior. Verify empty, loading, expired-account, failed-connection, keyboard, and working backup states at the actual window size.
