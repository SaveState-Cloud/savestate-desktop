# Native database and session-safety tests

The ordinary Rust suite does not need a live SaveState account, saved database
profile, credential-store entry, or running database:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml
npm run test:ui
npm run test:js
```

It includes real child-process tests for helper timeout, cancellation, output
limits, nonzero exit codes, descendant cleanup, and cancellation of both sides
of the database backup/restore pipe. Session-barrier tests use independent
in-memory registries and locks, including an API-await-shaped yield.

## Disposable MariaDB and Kopia round trip

An opt-in Windows test uses existing vendor binaries but creates its own
temporary database directory, loopback TCP port, filesystem Kopia repository,
configuration, cache, and test files. It does not connect to the installed
MariaDB server or read SaveState settings. The fixed repository password is
public synthetic test data and is never stored in Credential Manager.

Set the two executable locations explicitly. The MariaDB directory must contain
`mysql_install_db.exe`, `mysqld.exe`, `mysql.exe`, and `mysqldump.exe`:

```powershell
$env:SAVESTATE_TEST_MYSQL_BIN = 'C:\xampp\mysql\bin'
$env:SAVESTATE_TEST_KOPIA_BIN = (Resolve-Path src-tauri/bin/kopia.exe).Path
cargo test --manifest-path src-tauri/Cargo.toml native_database_kopia_roundtrip_and_failure_paths -- --ignored --nocapture --test-threads=1
```

The test runs the production dump/restore command builders and stream pipeline,
with isolated command configuration. It checks option-like database/table
names, binary row fidelity, multiple databases, table-only restore, routines,
triggers, events, source failure, missing encrypted content, destination SQL
errors, and ordinary filesystem backup/restore. It verifies teardown of its
server and temporary directory. No API calls or hosted storage are used.

## Current local hardening behavior

- All dump options precede `--`; database and table identifiers are positional.
- Metadata/helper commands are limited to 30 seconds and 4 MiB per output
  stream. These limits do not truncate SQL backup/restore streams.
- Windows helpers start suspended, enter a private kill-on-close job, and only
  then resume. Fast descendants cannot escape supervision before assignment.
- Scheduled database preflight runs off the asynchronous runtime and observes
  backup cancellation.
- Account/workspace changes reserve both the backup-admission barrier and the
  engine gate across the change. Active restore, deletion, or maintenance
  prevents the change. Logout still cancels tracked backups, but leaves the
  session intact if non-backup engine work is active.
- Already-admitted multi-step operations pass an explicit `EngineLease` to
  nested operations instead of requesting another read lock. A queued
  maintenance writer therefore cannot abort an admitted restore, retention
  pass or deletion; new operations still respect the writer's priority.
- Queued maintenance validates its account and session generation after
  obtaining the engine gate, before using its captured repository context.
- Unreachable legacy archive extraction is removed; current restore uses
  Kopia, while historical encryption-format regression tests remain.

## Limits of this verification

The disposable test does not validate hosted API authorization, production
storage, credentials, installer/updater release flows, or application-specific
Windows VSS recovery. Oracle MySQL versions, newer MariaDB versions, and
users/grants export require their own vendor matrix. A failed or cancelled SQL
import may already have changed its target; successful byte/row recovery is
not a general database transaction rollback guarantee.
