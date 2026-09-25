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
The lower-left selector names the active vault, starting with Personal and its plan. The selector lists every custom connector as another vault, plus Add vault and Manage vaults actions. The Backup sources page shows only the selected vault's sources and schedules; restore access stays within that vault.

FORM
The active vault is a persistent app context, not a card inside Personal. The manage view is secondary and contains the inline connect form. Source/schedule editing retains the familiar modal but its vault is fixed by the active context. Managed-only pages disappear while a custom vault is selected.

FINISH
Preserve encryption, local credentials, entitlement, quota, restore, and deletion behavior. Verify empty, loading, expired-account, failed-connection, keyboard, and working backup states at the actual window size.
