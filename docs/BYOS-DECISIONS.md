# BYOS architecture & product decisions

*Locked decisions log · SaveState-Cloud/savestate-desktop · September 22, 2026*

Status key: **Accepted** = locked for implementation. **Provisional** = proceed with documented contract; validate when private repo access is granted.

---

## ADR-001 — Canonical tier copy (managed + unlimited BYOS)

| Field | Value |
|---|---|
| **Date** | 2026-09-22 |
| **Status** | Accepted |
| **Deciders** | Gustav (product), coordinator (implementation) |

**Decision**

- **Pro:** “150 GB managed + unlimited BYOS”
- **Ultra:** “1 TB managed + unlimited BYOS”

Managed SaveState Cloud and user-owned BYOS are **simultaneous**, not either/or. Marketing must never imply that choosing BYOS replaces managed storage on Pro or Ultra.

**Rationale**

Homelab and SMB users expect hybrid storage: a managed vault for convenience plus their own bucket for scale. “Unlimited storage (BYOS)” alone on Pro cards obscures the 150 GB managed cap and caused comparison-table drift. The “managed + unlimited BYOS” framing matches Gustav’s locked Ultra definition and competitive positioning.

**Implementation**

- Website: `savestate-website` → `src/content/site.ts`, `docs/BYOS-PRODUCT-COPY.md`
- Desktop: this document; entitlement checks in planned `vaults.rs` / `api.rs`
- UI vault and plan screens should reflect hybrid managed + BYOS semantics

---

## ADR-002 — Local-only BYOS credentials

| Field | Value |
|---|---|
| **Date** | 2026-09-22 |
| **Status** | Accepted |
| **Deciders** | Gustav (product), coordinator (security) |

**Decision**

BYOS provider credentials are stored **locally only** in Windows Credential Manager (via the existing `keyring` integration). They are never synced to `savestate-api` in MVP. Users **re-enter credentials on a new PC** after reinstall or device migration.

**Rationale**

Preserves the product promise that provider keys never leave the device. Encrypted server backup of credentials would create a high-value cloud secret store, misleading recovery expectations, and additional breach/support surface. Re-entry on new hardware is understandable for homelab users and aligns with key custody expectations.

**Implementation**

- `src-tauri/src/vault_credentials.rs` (planned), Credential Manager targets scoped by vault ID
- Support position: redacted vault manifest export + setup checklist; no secret recovery from cloud
- Optional encrypted credential portability deferred to Phase 4 with separate threat model

---

## ADR-003 — One primary vault per profile; Ultra replication in Phase 3

| Field | Value |
|---|---|
| **Date** | 2026-09-22 |
| **Status** | Accepted |
| **Deciders** | Gustav (product), coordinator (architecture) |

**Decision**

- **MVP:** Each profile binds to **one primary vault** (managed or BYOS).
- **Phase 3 (Ultra):** Optional asynchronous replication to a second vault after primary success — implemented as two independent Kopia repository backups, not repository-object mirroring.
- **Replica failure policy:** **Warning-only** — if the primary backup succeeds, the overall job counts as success; replica failure surfaces a visible warning unless the user opts into “both required.”

**Rationale**

One primary vault keeps retention, restore routing, and failure handling unambiguous at launch. Dual-write at MVP would double egress/time invisibly and blur restore semantics. Independent repository replication avoids corrupting deduplication/maintenance expectations. Warning-only replica failures prevent a secondary destination from blocking primary backup success.

**Implementation**

- `src-tauri/src/db.rs`, `profiles.rs`: profile `vault_id` FK, Phase 3 replication policy on Ultra
- UI: replication labeled explicitly, not as a normal multi-destination picker

---

## ADR-004 — Vault count: no marketing cap; internal soft limit 25

| Field | Value |
|---|---|
| **Date** | 2026-09-22 |
| **Status** | Accepted |
| **Deciders** | Coordinator (product/ops) |

**Decision**

- **No marketing cap** on connected BYOS vaults for Pro or Ultra.
- **Internal soft limit:** 25 vaults per account (raise via support).
- **Never byte-quota BYOS** — SaveState does not meter user-provider bytes.

**Rationale**

Homelab users routinely maintain tiered storage (MinIO + B2 + SFTP). A low marketing cap would weaken the “unlimited BYOS” claim. An internal soft limit prevents abuse/UI overload without constraining normal homelab use. Byte quotas on BYOS would contradict tier contract and billing model.

**Implementation**

- API entitlements: `maxVaults` high default (25) with support override path
- Desktop: enforce before vault save in `vaults.rs`; clear message when limit reached

---

## ADR-005 — Dashboard metadata sync (redacted)

| Field | Value |
|---|---|
| **Date** | 2026-09-22 |
| **Status** | Accepted |
| **Deciders** | Coordinator (product/privacy) |

**Decision**

Sync **redacted** vault and profile metadata to the customer dashboard:

- **Include:** local vault ID, label, provider family, health status, last backup timestamp
- **Never sync:** endpoint, bucket, path, secrets, or credential material
- **Default:** on when dashboard metadata sync ships; account-level disable in settings later

**Rationale**

Operators want cross-device visibility into backup health without exposing storage topology or secrets. Redacted metadata supports support and fleet awareness while preserving BYOS key custody. Opt-out respects privacy-sensitive accounts.

**Implementation**

- `src-tauri/src/api.rs`: redacted telemetry/metadata upload
- Redaction helpers before any cloud upload; never include secrets in SQLite or events

---

## ADR-006 — S3 Object Lock: Phase 2 advanced opt-in only

| Field | Value |
|---|---|
| **Date** | 2026-09-22 |
| **Status** | Accepted |
| **Deciders** | Coordinator (product/security) |

**Decision**

- **Not in MVP.** Object Lock UI and enablement ship in **Phase 2** as an **advanced opt-in**.
- **Mandatory disclosure wizard** before enable (immutable retention, billing, deletion constraints).
- **S3-compatible providers only** (use S3 route for B2 Object Lock per Kopia guidance).

**Rationale**

Object Lock is valuable for ransomware resistance but has irreversible provider semantics and cost implications. Default-off with explicit disclosure avoids surprise lock-in and support incidents. S3-compatible scope matches Kopia’s supported Object Lock path.

**Implementation**

- Phase 2: `vault_providers/s3.rs` capability validation, wizard copy, `VAULT_OBJECT_LOCK_CONFLICT` error family
- B2 preset uses S3 endpoint, not native B2 transport

---

## ADR-007 — savestate-api: provisional entitlement/telemetry contract

| Field | Value |
|---|---|
| **Date** | 2026-09-22 |
| **Status** | Provisional |
| **Deciders** | Coordinator (architecture) |

**Decision**

Proceed with the **inferred entitlement and telemetry contract** from the desktop client analysis. Desktop Phase 1 may ship behind feature flags. API route and schema names remain **provisional** until private `savestate-api` access validates them.

**Provisional API surface (from desktop contract inference)**

| Need | Provisional route / change |
|---|---|
| Entitlements | Extend account response: `tier`, `managedQuotaBytes`, `managedUsedBytes`, `profileLimit`, `byosEnabled`, `maxVaults` (25 default), feature flags (`sftpEnabled`, `webdavEnabled`, `rcloneBetaEnabled`) |
| Managed sessions | Keep `POST /repo/session` managed-only; document as managed session, not generic BYOS credential service |
| BYOS telemetry | `POST /vault-events` (or extend engine-job telemetry): `vaultId`, `vaultKind`, provider family, operation, outcome, duration, redacted error code, bytes processed — no host/path/bucket/secrets |
| Metadata parity | Optional redacted vault/profile metadata sync (ADR-005) |
| Billing | Stripe Pro/Ultra → `byosEnabled=true`; BYOS activity never creates storage overage |
| Account lifecycle | Cancellation never deletes BYOS objects; may delete redacted telemetry per policy |

**Rationale**

Desktop architecture can proceed without blocking on private repo access. Documenting provisional routes prevents silent drift and gives API implementers a concrete checklist. Feature flags limit blast radius until reconciliation.

**Implementation**

- Reconcile against private `savestate-api` in Phase 0 before production API deploy
- `src-tauri/src/api.rs`: feature-flagged calls until API validated

---

## Related documents

- Marketing copy: `savestate-website` → `docs/BYOS-PRODUCT-COPY.md`
- Integration plan: SaveState-Cloud project store → `docs/tauri-byos-integration-plan.md`
- Ultra tier plan: SaveState-Cloud project store → `docs/ultra-byos-app-plan.md`
