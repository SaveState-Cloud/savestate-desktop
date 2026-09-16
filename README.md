# SaveState Desktop

SaveState Desktop is the official open-source Windows client for
[SaveState](https://savestate.dk), an encrypted cloud backup service. The app
configures scheduled backups, creates and restores encrypted Kopia snapshots,
and reports privacy-limited job status to the SaveState service.

## Public and private components

This repository contains only the software that runs on a customer's Windows
device:

- the Tauri desktop shell and user interface;
- local scheduling and profile management;
- client-side key handling and encryption;
- the integration with the open-source Kopia backup engine; and
- the authenticated client for the public SaveState API.

The hosted API, operator Engine, storage infrastructure, billing systems,
website, and future business, reseller, and affiliate panels are separate
services and are not included here.

## Released capabilities

The current Windows client provides:

- reusable folder backup profiles with one or more local run times, custom day
  intervals, vault-folder placement, and latest-N or unlimited retention;
- one-off Quick Backup for selected files or a folder;
- bounded automatic retries for transient scheduled-backup failures, while
  credential, key, tool, and quota failures wait for customer action;
- native MySQL and MariaDB backup profiles that discover compatible vendor and
  XAMPP tools, require a successful connection test, and support all-database,
  selected-database, or selected-table scopes;
- streamed native database dumps into Kopia and streamed restores into the
  native database client, without an intermediate plaintext SQL dump file;
- database-password storage in Windows Credential Manager;
- content-defined deduplication, zstd compression, encrypted Kopia snapshots,
  and latest-N snapshot retention;
- optimized storage accounting based on the encrypted physical repository
  footprint, with separate source-data and optimization-savings statistics;
- whole-snapshot restore into a new destination folder; selective-file restore
  is not currently available;
- explicitly enrolled organization-managed folder policies that remain
  read-only in the local UI and run on the device's local wall clock;
- Windows VSS in `when-available` mode, with normal traversal fallback and
  snapshot failure on file or directory read errors;
- backup, restore, database, retry, maintenance, and quota status in the app
  and privacy-limited operational telemetry for the customer dashboard;
- configurable webhook notifications and a real delivery test, without a
  separate Discord bot;
- a one-time offline vault recovery key distinct from account recovery codes;
  and
- Tauri-signed update artifacts that install only when backup-engine work is
  idle.

Public feature explanations and current commercial facts live at
[savestate.dk/features](https://savestate.dk/features). The compact factual
product sheet is available at
[savestate.dk/ai-facts.txt](https://savestate.dk/ai-facts.txt).

### Product boundaries

- The released app supports Windows 10 and 11 on x64. Linux distribution is
  not currently released.
- SaveState protects selected files, folders, application recovery data, and
  native MySQL or MariaDB logical dumps. It is not a bare-metal system-image
  product.
- VSS provides a stable file-system view; it does not by itself guarantee
  logical consistency for every stateful application.
- Native database integration currently covers MySQL and MariaDB. Other
  applications should use their vendor-supported dump or snapshot workflow.
- Database restores apply SQL directly to the selected server. Cancellation
  stops the import but does not roll back SQL already applied; restore into a
  disposable database first and verify the recovered application.
- A successful job confirms capture and storage. Customers should still test a
  representative restore and validate the recovered application.
- Compression and deduplication savings depend on the workload.

### Organization-managed installations

Connecting an organization installation requires an explicit acknowledgement
in the Windows app. The disclosure names the controls an authorized
organization administrator receives: create, pause, resume, or remove managed
local-folder schedules; select any accessible local folder path and see that
source-path metadata; start or cancel a managed backup; request a whole-snapshot
restore inside SaveState Restores; and delete a listed snapshot. Managed policies are labelled **Managed
by _organization_** and their source, schedule, and retention fields cannot be
edited locally.

Commands are fixed, versioned operations delivered through the authenticated
device credential; the agent does not expose a shell or accept arbitrary
commands. Policies and command receipts use separate local tables from personal
profiles, so organization scheduling does not consume or mutate the personal
profile allowance. Saved schedules continue to be evaluated using the enrolled
PC's local wall clock during temporary network outages. Managed work is dormant
while the owning account is signed out or its vault is locked, and a captured
account/workspace context is rechecked before repository access.

All backup, restore, and snapshot deletion work runs locally through the same
Kopia engine and encrypted repository used by the desktop app. A managed
backup source must be a folder on a local fixed or removable Windows drive;
UNC/network shares, mapped remote drives, and paths traversing junctions or
other reparse points are rejected before execution, so managed commands never
use ambient Windows share credentials. A managed restore command contains only
the exact snapshot identifier: administrators cannot choose a destination path.
The app generates a unique new folder under its per-user, environment-isolated
`SaveState Restores` application-data root. The root and restored descendants
have a protected Windows ACL; local fixed/removable volumes and pinned,
reparse-free ancestry are required. Actual destinations are listed only in local
Settings. Existing paths and in-place overwrite are rejected. Removing an assignment stops its managed
schedule and pending work but does not delete snapshots. Snapshot deletion is a
separate, exact-ID command and is reported as deleted only after the local
repository operation succeeds.

Settings also provides **Disconnect this PC** with a separate destructive
confirmation. The account-authenticated server disconnect must succeed before
the device credential is removed locally. The request uses a durable idempotency
identifier so an uncertain response can be retried safely. Disconnecting
tombstones managed schedules and cancels their queued or running jobs while
preserving snapshots and every personal profile.

## Security model

- Backup data is encrypted on the device before it is uploaded.
- A random per-account master key is wrapped locally in a versioned envelope
  with a password slot and a one-time offline vault-recovery slot. The hosted
  API stores the encrypted envelope and a one-way possession verifier, not the
  plaintext master key.
- When **Remember me** is enabled, the session and decrypted master key are
  stored in Windows Credential Manager. Explicit sign-out removes both; an
  expired or invalidated account session preserves the remembered key so the
  client can safely recover the existing vault after reauthentication.
- Storage access uses short-lived, account-scoped credentials issued by the
  SaveState API.
- On Windows, the client enables Kopia's native Volume Shadow Copy policy in
  `when-available` mode before backups. This gives Kopia a stable view of files
  opened by running services when VSS is available and falls back to normal
  traversal without an elevation prompt when it is not. File and directory
  read errors still fail the snapshot rather than making an incomplete backup
  appear successful.
- The account password is sent to the SaveState API over HTTPS for account
  authentication. For that reason, this project describes its protection as
  **client-side encryption**, not strict zero-knowledge authentication.

See [SECURITY.md](SECURITY.md) for the supported security boundary and private
vulnerability reporting process. Network behavior is documented in
[PRIVACY.md](PRIVACY.md), and the key hierarchy and recovery invariants are
documented in [docs/VAULT_RECOVERY.md](docs/VAULT_RECOVERY.md).
The coordinated service prerequisites, deployment order, rollback policy, and
validation matrix are recorded in
[docs/RECOVERY_RELEASE_HANDOVER_2026-08-20.md](docs/RECOVERY_RELEASE_HANDOVER_2026-08-20.md).

## Technology

- [Tauri v2](https://v2.tauri.app/)
- Rust
- HTML, CSS, and vanilla JavaScript
- [Kopia](https://kopia.io/) for encrypted, deduplicated snapshots

## Build on Windows

Requirements:

- Windows 10 or 11 on x64
- Node.js 22
- the stable Rust toolchain
- the Windows prerequisites required by Tauri v2

```powershell
npm ci
npm run bundle:kopia
cargo test --manifest-path src-tauri/Cargo.toml
npm run build
```

`bundle:kopia` downloads the pinned Kopia release and verifies its SHA-256
checksum before it is included in the application bundle.

Run `npm run test:ui` and `npm run test:js` for frontend tests. Native process,
concurrency, and database/Kopia integration coverage is described in
[docs/NATIVE_DATABASE_TESTING.md](docs/NATIVE_DATABASE_TESTING.md).

`npm run build` creates unsigned local installers. It does not promise that two
independent builds will be bit-for-bit identical. Protected release automation
instead builds once, stages those exact installer bytes for every release
destination, and records their SHA-256 hashes and provenance. It uses
`npm run build:release`, which additionally creates Tauri updater artifacts and
therefore requires the private updater signing key from CI secret storage.

## Code signing

The project intends to use free code signing provided by
[SignPath.io](https://about.signpath.io/), with a certificate from
[SignPath Foundation](https://signpath.org/). Until that application is
approved and a SignPath workflow is connected, public installers may remain
unsigned by Windows Authenticode and SmartScreen may warn. Tauri's separate
updater signature still protects automatic-update artifacts. The signing
policy is documented in
[CODE_SIGNING_POLICY.md](CODE_SIGNING_POLICY.md).

## Contributing

Contributions are welcome through GitHub pull requests. Start with
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

The source code and bundled project assets are licensed under
[GPL-3.0-only](LICENSE). The license does not grant permission to represent a
fork as an official SaveState product; see [TRADEMARKS.md](TRADEMARKS.md).
