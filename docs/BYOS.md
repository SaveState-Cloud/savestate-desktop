# Customer-owned storage (BYOS)

Implementation note for the next desktop release. This is **not** a claim that
the currently installed production app already has BYOS.

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

## Verification before release

- API unit/runtime tests and desktop Rust/UI tests must pass.
- Disposable local S3 test: provider validation, connect/create, tagged
  snapshot, list, restore and file-content comparison. The 2026-09-24 local
  emulator round trip passed. On 2026-09-25, a signed-in Windows development
  app also connected and validated a disposable local S3 bucket. That GUI test
  exposed and fixed an account-scope guard that had rejected every BYOS
  connection; its regression test and the desktop test suites now pass. The
  GUI profile backup and restore, a live Backblaze B2 or Cloudflare R2 bucket,
  scheduled execution, and a signed production installer are still unverified.
- Before production, repeat connect/backup/restore with a disposable provider
  bucket and a signed development app, verify cancellation/sign-out and a
  replacement-PC reconnect before releasing the desktop installer. The API
  key-retention fix and the public Terms, Privacy, and DPA distinction between
  managed and customer-owned objects were deployed on 2026-09-25. Recheck
  those live policies and the matching API before release. Do not advertise
  BYOS as available on the live website until the desktop release gate passes.
