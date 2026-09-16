# Desktop client privacy

This document describes network behavior in the open-source SaveState Desktop
client. It complements the service privacy policy presented on the SaveState
website.

## Services contacted by the client

The application communicates with `api.savestate.dk` over HTTPS to:

- authenticate the account and maintain a session;
- retrieve account, plan, usage, and backup metadata;
- obtain an account-scoped encrypted-repository session;
- report backup, restore, deletion, maintenance, and scheduled-job status;
- synchronize privacy-limited schedule timing metadata;
- poll for and acknowledge explicitly authorized organization-managed backup
  commands when this PC is enrolled;
- manage folders, retention, notification settings, and subscription actions;
  and
- check for application updates.

Backup contents and Kopia repository objects are encrypted on the user's device
before upload. The current hosted service proxies that encrypted repository
traffic through SaveState's ciphertext-only API gateway to Backblaze B2 EU
Central, where the backup objects are stored. Restore operations follow the
reverse route and are decrypted only on the user's device. This EU-residency
statement applies to stored backup objects; it does not claim that every
transient gateway processing or network-routing location is in the EU.

To provide the dashboard, scheduling, and job history, the SaveState API
separately processes limited readable operational metadata. Depending on the
operation, that can include a snapshot identifier, source path, timestamp,
size, file count, folder, schedule timing, and job status. This operational
metadata is not part of the encrypted Kopia repository, and it does not contain
backup file contents or the decrypted vault master key.

## Organization-managed device control

Organization control is off until the user explicitly connects an assigned
installation or redeems a setup token and acknowledges the control disclosure.
After enrollment, an authorized organization administrator can create, update,
pause, resume, or remove managed folder schedules; start or cancel a managed
backup; request a whole-snapshot restore; and request deletion of a listed
snapshot. Administrators may select any accessible local folder path and see
that path as operational metadata, but never receive its plaintext contents or
the plaintext vault key. The local UI identifies each assigned policy as **Managed by
_organization_** and presents its fields as read-only.

The readable operational data used for this feature can include the managed
source path, device-local schedule times, retention count, assignment and policy
identifiers and revisions, job and snapshot identifiers, timestamps, outcome
codes, logical byte counts, and file counts. Per-snapshot physical stored bytes
are omitted when they cannot be attributed safely. Command and event payloads
are bounded and typed; they do not carry file contents, filenames, repository
passwords, plaintext encryption keys, or decrypted backup data. Restore
destinations are neither accepted in organization commands nor included in
device event telemetry. The app chooses the destination and shows its actual
path only in local Settings.

Policy scheduling, Kopia execution, encryption, restore, and deletion occur on
the enrolled PC. Managed work is suspended while the owning SaveState account
is signed out or its vault is locked. Managed backup sources are restricted to
local fixed or removable Windows drives; UNC shares, mapped remote drives, and
paths traversing junctions or other reparse points are rejected, and the agent
does not use ambient Windows share credentials for organization commands.
Restore commands carry only an exact snapshot identifier. The app generates a
unique new folder under its protected, environment-isolated per-user
`SaveState Restores` application-data root. It requires a local fixed/removable
volume and reparse-free, pinned ancestry, applies a protected Windows ACL to
the root and restored descendants, and rejects existing paths and overwrite.
Administrators cannot choose Startup, plugin, network, or other arbitrary
destination paths. Removing an
assignment disables and removes only its local managed scheduling state and
does not delete its existing snapshots. Snapshot deletion requires a separate
exact snapshot identifier and is reported only after the repository deletion
has actually succeeded.

The user can disconnect the PC from Settings after a separate warning that
managed schedules will stop and managed work will be cancelled while snapshots
remain. The client first sends an account-authenticated, replay-safe disconnect
request. After the service confirms the disconnect (including an idempotent
already-disconnected response), it tombstones managed schedules, cancels their
work without changing personal profiles, and removes the local device
credential. A revoked credential observed later by the control poller or
periodic heartbeat triggers the same local cleanup.

The gateway authorizes the repository request and forwards encrypted objects;
it does not receive the repository password, decrypted master key, or plaintext
backup contents. As described in `SECURITY.md`, account authentication and key
delivery still depend on the hosted SaveState API, so this is client-side
encryption rather than a claim of strict zero-knowledge authentication.

If a user configures Discord notifications, the destination is stored by the
SaveState API and delivery is performed by the hosted service. The desktop
client does not contact Discord directly.

## Account recovery and multi-factor authentication

For accounts that enable TOTP or use recovery, the hosted service processes the
account email address, an encrypted TOTP authenticator seed, and hashed
account-recovery tokens or recovery codes. It also retains limited
security-audit metadata and a one-way request fingerprint to rate-limit abuse
and investigate recovery activity. Recovery tokens and codes are not stored in
plaintext, and the TOTP seed is encrypted at rest.

Email access, a TOTP authenticator, and recovery codes authenticate control of
the SaveState account. They are not the vault master key and cannot themselves
decrypt a backup. Access to existing encrypted backups still depends on the
separately protected, client-owned master-key envelope and one of its local
unlock factors: the previous vault password, the one-time offline vault
recovery key, or a matching remembered-device key. The API stores the encrypted
envelope, key identifier, and a one-way verifier used to prove possession during
rotation; it does not receive a plaintext copy of the vault master key. Account
recovery does not create a replacement key or make old backup ciphertext
decryptable with an email, TOTP value, or account recovery code.

When **Remember me** is selected, the bearer session and decrypted vault master
key are stored in Windows Credential Manager. Explicit sign-out removes that
remembered material after any active backups have been stopped or the user has
cancelled sign-out. By contrast, an `auth_version` or unauthorized-session
invalidation clears in-memory authentication and repository caches without
deleting the remembered master key, so a legitimate password-recovery flow can
still prove and unlock the existing vault.

## Information intentionally excluded from job telemetry

Engine job events identify the authenticated installation and operation, but
the client is designed not to send source-folder paths, restored destination
paths, profile names, filenames, file contents, repository passwords, or the
decrypted master key in those job-event payloads. This job-telemetry exclusion
does not apply to the limited dashboard and schedule metadata described above;
for example, a snapshot source path may be processed separately for display in
the dashboard.

## Third-party requests

The runtime UI uses operating-system fonts and does not load web fonts or
analytics scripts. The Kopia download in `scripts/bundle-kopia.mjs` occurs only
during a developer or CI build and is verified against a pinned SHA-256 hash.

## User control

Users can sign out to remove the remembered session and remembered vault master
key, disable schedules, remove notification settings, and request account
management through the SaveState service. If a backup is active, sign-out asks
before stopping and cleaning up that uncommitted snapshot. Network access is
required for cloud backup, restore, account, usage, and update features.
