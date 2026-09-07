# Organization enrollment verification

## Hardening and isolation — 7 September 2026

Implemented additional safeguards without changing the broad enrollment gate:

- Backup outcomes match the complete account/workspace identity rather than
  comparing `email::service:id` to a plain email. Personal and other service
  backups cannot mark this organization installation protected or unhealthy.
- New connections store the service workspace. Legacy saved connections learn
  it from the authenticated API heartbeat response; deploy the matching Core
  API change first. Older API responses remain readable but cannot migrate a
  legacy connection's missing workspace.
- Credential read/modify/write operations are serialized. A delayed heartbeat
  acknowledgement cannot erase a newer queued outcome, and a revoked response
  from a replaced credential cannot delete its replacement.
- A secure-store failure leaves the active session unchanged and reports the
  required operator recovery. The server may already have consumed the setup
  token: replace the device credential and issue a new recovery token. This is
  explicit recovery, not automatic rollback of server enrollment.
- Metadata/remembered-session write failures now complete the accepted session
  switch, invalidate the old repository cache and return a persistence warning.
- Development/staging builds now isolate their profile database, installation
  ID, Kopia cache, remembered account, organization credential and database
  passwords. Production keeps its existing paths and keyring names unchanged.
  Non-production builds do not automatically register Windows startup. Custom
  environment/API combinations receive a stable hashed namespace.
- Never migrate the old shared production data into a development build. Sign
  in separately and create disposable development profiles instead.

Local regression evidence: 99 Rust tests passed, one existing opt-in database/
Kopia test ignored; 51 UI/source checks and 3 JavaScript tests passed. Fault
tests use an injected secure-store backend and real disposable SQLite databases.
They cover failed persistence, changed session generation, old acknowledgements,
replacement credentials, legacy workspace learning and 100 concurrent queue/ack
races. These are not evidence of a real hosted backup/restore round trip.

Core API tests additionally replace a credential between verification and the
database transaction for connected/succeeded/failed reports. All three reject
the old credential without modifying replacement health or creating a webhook.
The heartbeat response never includes the credential or its hash.

Installed Windows + hosted enrollment/backup/restore acceptance remains open.
The public updater version and rollout flags are unchanged by this work.

## ORG-ENROLL-004 progress — 5 September 2026

The first hardening step is implemented, not a declaration that broad automatic
enrollment is ready. No installer version or rollout flag changes are included.

### Fixed: enrollment must respect active engine work

Both account-first connection and setup-token redemption switch the selected
service token and invalidate the Kopia session cache. Previously these paths
omitted the admission guard already used by ordinary workspace switching.

They now reserve `begin_session_change()` before capturing the account or
contacting the API. The guard stays alive through the API await, secure device
credential persistence, local session switch and remembered-session refresh.
Active backups, restores, maintenance, updates or another session transition
therefore reject connection before a setup token can be consumed. New work
cannot enter midway through enrollment. Errors and dropped futures release the
guard so a failed connection does not permanently block backups or sign-out.

### Verification completed locally

- Rust suite: 87 passed, one opt-in database/Kopia integration test not run.
- Desktop UI/source-contract tests: 50 passed; JavaScript behavior tests: 3 passed.
- Formatting check passed.
- Native admission tests use independent registries and real asynchronous locks.
  They cover active backup/restore/maintenance, a pending API-shaped await,
  concurrent session changes, failure, cancellation and successful retry admission.
- Source-contract tests verify both real enrollment commands acquire the guard
  before the API call and keep it through finalization. These are not a real
  networked Windows enrollment test.
- Core API's existing 24 targeted enrollment/admin/health tests passed against
  disposable in-memory SQLite fixtures: account/tenant scope, approval, expiry,
  replay, concurrent redemption, stable device binding, hash-only credentials,
  reconnection and ordered backup-health transitions.

No real user account, credential-store entry, production backup, email, storage
allocation or infrastructure secret was created or changed by these tests.

## Required before closing ORG-ENROLL-004

1. Run host provisioning and customer approval for a disposable allowlisted
   organization. Verify installation association, quota and tenant isolation.
2. On an isolated installed Windows client, exercise account-first connection and
   setup-token recovery, including restart and remembered-session persistence.
3. Start backup/restore work and attempt each connection method. Confirm rejection
   leaves the server token unused, then retry after the operation finishes.
4. Back up representative files to the assigned hosted encrypted repository,
   restore to a separate directory and compare content hashes. Confirm the
   original personal workspace remains separate.
5. Verify heartbeats distinguish connected from protected, and health/lifecycle
   transitions reach the organization portal without leaking file paths/secrets.
6. Fault-test local finalization after server acceptance: secure-store failures,
   metadata write failures and old in-flight heartbeat responses after reconnect.
   In particular, review the shared credential-entry read/modify/write operations
   and unconditional removal after a revoked-credential response before rollout.
7. Record release build/version, test identities, results and cleanup. Only then
   evaluate widening the enrollment flag with the owner.

Portal-created setup tokens currently last 24 hours; Core API operator recovery
tokens last 30 minutes. These are different issuance paths, not interchangeable
expiry promises. Test each against its actual issuer.
