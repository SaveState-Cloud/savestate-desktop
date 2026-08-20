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
