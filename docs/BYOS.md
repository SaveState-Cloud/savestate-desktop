# Customer-owned storage (BYOS)

Implementation and acceptance record for the next desktop release. This is
**not** a claim that an older installed production app already has BYOS.

## Customer flow

1. On an eligible active plan, open **Settings → Your storage destinations**.
2. Create a bucket with an S3-compatible provider and enter its endpoint,
   region, bucket name, access key ID and secret. A dedicated bucket or prefix
   is recommended. SaveState generates an account-scoped prefix if left blank.
3. **Connect and test** opens or creates a Kopia repository and runs Kopia's
   storage-provider validation before the destination is saved.
4. Create a file/folder backup profile and choose the connected destination.
   SaveState-managed storage remains the default. A profile's destination is
   fixed after creation; create a second profile to change it without moving or
   deleting existing restore points.
5. Run or schedule the profile normally. Open **Restore points** in Settings to
   browse snapshots directly from the bucket and restore into a new local
   folder. Disconnecting a destination never deletes remote objects.

## Boundaries and recovery

- Customer-owned object bytes do not count toward SaveState's managed-storage
  quota, and provider charges are paid by the customer. The existing automated
  schedule limit still applies to scheduled BYOS profiles.
- File/folder profiles are supported. Quick backups and native database
  profiles still use SaveState-managed storage.
- The S3 key pair stays in Windows Credential Manager on the device. The API
  stores only plan entitlement; it never receives these keys or object bytes.
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
  customer-owned restore points remain available from Settings.
- The website vault cannot directly browse customer-owned objects because
  SaveState does not hold the customer's storage credentials. Use the desktop
  app's Restore points view for BYOS.
- The initial Windows release accepts HTTPS S3-compatible endpoints, plus
  loopback HTTP for local MinIO testing. Arbitrary remote HTTP is rejected.

## Verification and release notes

- API unit/runtime tests and desktop Rust/UI tests must pass. On 2026-09-25,
  the desktop checks passed: 54 UI tests, 3 logout-flow tests, and 106 Rust
  tests (one optional database integration test ignored).
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
