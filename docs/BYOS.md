# Customer-owned storage (BYOS)

Implementation and acceptance record for customer-owned vaults. The vault-first
navigation below is a desktop change on this branch; do not describe it as
available in an installed release until a new installer is published.

## Customer flow

1. The lower-left selector is the active vault. **Personal** is the permanent
   SaveState-managed vault and shows its plan there. Open the selector and
   choose **Add vault** to connect another storage destination. **Manage
   vaults** shows the full connector list and disconnect controls.
2. Choose a connector. For S3-compatible cloud storage, create a bucket and
   enter its endpoint, region, bucket name, access key ID and secret. A
   dedicated bucket or prefix is recommended. SaveState generates an
   account-scoped prefix if left blank. For **External drive**, select an
   existing dedicated folder inside the drive; no cloud keys are required.
3. **Connect and test** opens or creates a Kopia repository and runs Kopia's
   storage-provider validation before the vault is saved. A new drive vault
   gets an ID marker in its folder so a later drive-letter collision cannot
   silently initialize a repository on a different disk.
4. Select a vault in the lower-left corner and add one or more file/folder backup sources. Each source
   can have its own local-time schedule and retention or be manual-only. Its
   destination is fixed after creation; add another source to back up the same
   folder to a different vault without moving its existing restore points.
5. Run a source in its vault. Use **Restore points** inside a custom vault to
   browse snapshots directly from its bucket and restore into a new local
   folder. **Browse backups** inside Cloud - Personal opens the managed backup
   browser. Custom vaults have their own dashboard and source list; managed-only
   database and quick-backup pages are not shown while one is selected.
   Disconnecting a custom vault never deletes its bucket or drive data.

## Boundaries and recovery

- Customer-owned repository bytes do not count toward SaveState's managed-storage
  quota. Cloud-provider charges and external-drive ownership are the customer's responsibility. There is no numerical
  cap on custom vaults. The existing automated schedule limit applies across
  all vaults, not separately to each vault.
- File/folder profiles are supported. Quick backups and native database
  profiles still use SaveState-managed storage.
- The S3 key pair stays in Windows Credential Manager on the device. External
  drive vaults use no S3 keys. The API stores only plan entitlement; it never
  receives these keys or customer-owned backup bytes.
  Destination metadata and profile mappings are local SQLite data.
- Kopia encrypts the repository with the account's client-side master key.
  The same account key plus the provider credentials, endpoint, bucket and
  prefix are needed to reconnect on a replacement PC. SaveState cannot recover
  lost provider credentials. Record the displayed prefix before replacing the
  device.
- Existing customer-owned restore points remain browsable after the plan
  lapses, provided the user can sign in and unlock the same master key. The
  encrypted account-key envelope is retained after managed service expiry
  for this purpose; an explicit account-erasure request deletes it. A former
  subscriber can reconnect an existing repository, but cannot create a new
  one or run backups without an active eligible plan. If a replacement
  subscription creates a new service workspace, create a new backup profile;
  the old service-scoped schedule is not silently moved, while the existing
  customer-owned restore points remain available from Vaults.
- The website vault cannot directly browse customer-owned objects because
  SaveState does not hold the customer's storage credentials. Use the desktop
  app's Restore points view for BYOS.
- The initial Windows release accepts HTTPS S3-compatible endpoints, plus
  loopback HTTP for local MinIO testing. Arbitrary remote HTTP is rejected.
- External-drive vaults require an existing dedicated folder on a local drive.
  If that folder disappears or its marker changes, backup and restore stop
  before Kopia writes. A drive-letter change can be repaired with **Find drive
  folder**, which accepts only the original marked folder. A backup source
  cannot contain or sit inside its own repository folder. Keep another copy of
  important data: a drive kept next to the PC is not an off-site backup.

## Verification and release notes

- External-drive support is implemented on the `feat/vault-first-desktop`
  branch, not in the installed production app. The Windows development app
  could not be rebuilt in place while its executable remained open, and app
  control reported access denied. A visual
  in-app backup/restore acceptance pass is still required before release.
- For the drive connector, 67 desktop UI checks, 3 logout checks, and 109 Rust
  checks passed (one optional integration test ignored). Kopia 0.23.1 created
  a disposable local filesystem repository, validated the provider, backed up
  a folder, and restored its file with a matching SHA-256 hash. Native tests
  cover missing/replaced drive markers and
  rejecting backup sources that overlap the repository. This test used a
  disposable folder, not a physical USB drive; unplug/replug behavior remains
  to be accepted on real hardware.
- API unit/runtime tests and desktop Rust/UI tests must pass. On 2026-09-25,
  the desktop checks passed: 62 UI tests, 3 logout-flow tests, and 106 Rust
  tests (one optional database integration test ignored). The API checks passed
  288 unit tests and 6 disposable-D1 runtime tests.
- The development build was opened at the Windows app's 802×632 window size.
  The lower-left selector displayed Personal and two customer-owned vaults.
  Selecting B2 changed the active vault and its source list, hid managed-only
  navigation, showed a B2-specific dashboard, and loaded restore points from
  the private test bucket. Selecting Personal again restored managed navigation
  and its own empty source list. Manage vaults and its connect form opened and
  the form was cancelled without writing backup or provider data. The source
  dialog focused its name field, wrapped Shift+Tab to its final action, and
  Escape returned focus to Add backup source; cancelling Add Vault restored
  keyboard focus to its button. Connector setup was absent from Settings.
- A disposable local S3 round trip passed in the Windows development app:
  connect/validate, create profile, back up a 111-byte file, list the restore
  point, and restore to a separate directory. Original and restored SHA-256
  both equal `07F4C368C2DAEF5B50A34CA4F44368703B04449FBF671D913CED9EE7BD7C2DBA`.
- A real Backblaze B2 EU test used only private bucket
  `savestate-byos-qa-20260925-8` and a 24-hour bucket-scoped application key.
  The Windows development app connected and validated the provider, backed
  up the same 111-byte file, listed the restore point directly from B2, and
  restored into a separate directory with the same SHA-256. No existing B2
  bucket or production backup was touched. Initial connection took roughly
  three minutes; investigate if that is representative for new customers.
- A 11:27 Europe/Copenhagen scheduled profile ran successfully at 11:28:07
  (09:28:07 UTC) and advanced its next run to 2026-09-26 11:27 local. The
  profile screen did not visibly refresh immediately during this observation;
  the database's `last_run`, `next_run`, and `schedule_state` confirmed success.
  The profile screen now also polls while visible and refreshes on focus, so
  a missed native progress event cannot leave those labels stale indefinitely.
- The API key-retention fix and public Terms, Privacy, and DPA distinction
  between managed and customer-owned objects were deployed on 2026-09-25.
  Recheck those live policies and the matching API before publishing BYOS
  availability on the website.
- Not yet proven by this test: sign-out while a BYOS backup is active,
  replacement-PC reconnect using the original prefix and master key, and
  installation/update of the signed production installer. Unit tests cover
  logout cancellation and reconnection key/prefix boundaries, but do not
  replace a second-PC acceptance test. Verify the signed installer and updater
  feed after the release workflow publishes them; do not describe these
  untested paths as fully verified.
