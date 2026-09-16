//! Explicitly opted-in local Windows/Kopia acceptance. No SaveState account,
//! hosted repository, installed profile, keyring, or ambient share credential.
use super::*;
use crate::kopia::KopiaSnapshot;
use std::process::Command;

struct LocalKopiaFixture {
    root: tempfile::TempDir,
    binary: PathBuf,
}

impl LocalKopiaFixture {
    fn command(&self) -> Command {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(&self.binary);
        command.creation_flags(0x08000000);
        command.args([
            "--disable-file-logging",
            "--disable-content-log",
            "--no-use-credential-manager",
            "--no-persist-credentials",
        ]);
        command.arg(format!(
            "--config-file={}",
            self.root.path().join("repository.config").display()
        ));
        command
            .env("KOPIA_PASSWORD", "public-managed-native-fixture")
            .env("KOPIA_CACHE_DIRECTORY", self.root.path().join("cache"))
            .env("KOPIA_LOG_DIR", self.root.path().join("logs"))
            .env("KOPIA_CHECK_FOR_UPDATES", "false");
        command
    }

    fn run(&self, args: &[String]) -> Vec<u8> {
        let mut command = self.command();
        command.args(args);
        let output =
            crate::subprocess::run(command, crate::subprocess::Limits::default(), &|| false)
                .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn list_ids(&self) -> Vec<String> {
        let value: Value = serde_json::from_slice(&self.run(&[
            "snapshot".into(),
            "list".into(),
            "--all".into(),
            "--json".into(),
        ]))
        .unwrap();
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|snapshot| snapshot["id"].as_str().unwrap().to_string())
            .collect()
    }
}

#[test]
#[ignore = "Requires an explicitly configured local Kopia executable; creates disposable local fixtures only"]
fn native_managed_folder_kopia_roundtrip_retention_restore_delete() {
    let fixture = LocalKopiaFixture {
        root: tempfile::Builder::new()
            .prefix("savestate-managed-native-")
            .tempdir()
            .unwrap(),
        binary: PathBuf::from(
            std::env::var("SAVESTATE_TEST_KOPIA_BIN")
                .expect("Set SAVESTATE_TEST_KOPIA_BIN explicitly"),
        ),
    };
    assert!(fixture.binary.is_file());
    fixture.run(&[
        "repository".into(),
        "create".into(),
        "filesystem".into(),
        "--no-check-for-updates".into(),
        format!(
            "--path={}",
            fixture.root.path().join("repository").display()
        ),
    ]);
    let source = fixture.root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(source.join("nested")).unwrap();
    let binary: Vec<u8> = (0..8192).map(|index| (index % 256) as u8).collect();
    std::fs::write(source.join("nested").join("payload.bin"), &binary).unwrap();
    std::fs::write(source.join("-leading.txt"), b"managed local roundtrip").unwrap();
    let pinned_source = validate_managed_backup_source(&source.to_string_lossy()).unwrap();
    let mut snapshots = Vec::new();
    for revision in 1..=3 {
        std::fs::write(source.join("version.txt"), revision.to_string()).unwrap();
        let value: Value = serde_json::from_slice(&fixture.run(&[
            "snapshot".into(),
            "create".into(),
            "--json".into(),
            "--no-progress".into(),
            pinned_source.path.to_string_lossy().into_owned(),
        ]))
        .unwrap();
        snapshots.push(KopiaSnapshot {
            id: value["id"].as_str().unwrap().into(),
            source_path: pinned_source.path.to_string_lossy().into_owned(),
            start_time: value["startTime"].as_str().unwrap().into(),
            size: (binary.len() + b"managed local roundtrip".len() + 1) as u64,
            file_count: 3,
            folder: "/managed-native".into(),
            backup_kind: "files".into(),
            database_profile_id: None,
            database_profile_name: None,
            root_object_id: None,
            profile_id: Some("mpa_native".into()),
            profile_name: Some("Native fixture".into()),
            trigger: Some("managed_manual".into()),
            version_number: Some(revision),
        });
    }
    assert_eq!(fixture.list_ids().len(), 3);
    let expired = crate::kopia::expired_profile_snapshot_ids(
        snapshots.clone(),
        "mpa_native",
        "/managed-native",
        2,
    );
    assert_eq!(expired, vec![snapshots[0].id.clone()]);
    for id in &expired {
        fixture.run(&[
            "snapshot".into(),
            "delete".into(),
            id.clone(),
            "--delete".into(),
        ]);
    }
    assert_eq!(fixture.list_ids().len(), 2);
    assert!(!fixture.list_ids().contains(&snapshots[0].id));

    let data = fixture.root.path().join("app-data");
    std::fs::create_dir(&data).unwrap();
    let root = ensure_managed_restore_root_at(&data).unwrap();
    let candidate = managed_restore_destination_candidate();
    let destination =
        validate_generated_managed_restore_destination(&candidate.to_string_lossy(), &root.path)
            .unwrap();
    let staging_path = managed_restore_staging_candidate(&destination, "native-fixture").unwrap();
    let staging = create_managed_restore_staging(&staging_path, &root.path).unwrap();
    let latest_id = &snapshots[2].id;
    let listed: Value = serde_json::from_slice(&fixture.run(&[
        "snapshot".into(),
        "list".into(),
        "--all".into(),
        "--json".into(),
    ]))
    .unwrap();
    let restore_source = crate::kopia::managed_snapshot_restore_root(&listed, latest_id).unwrap();
    let args =
        crate::kopia::snapshot_restore_args(&restore_source, &staging_path.to_string_lossy(), true);
    assert!(args.contains(&"--skip-owners".into()) && args.contains(&"--skip-permissions".into()));
    fixture.run(&args);
    validate_managed_restore_staging(&staging_path, &root.path).unwrap();
    assert!(managed_restore_staging_acl_is_protected(&staging_path).unwrap());
    staging.finalize(&destination).unwrap();
    let restored =
        std::fs::read(destination.join("nested").join("payload.bin")).unwrap_or_else(|error| {
            let names: Vec<_> = std::fs::read_dir(&root.path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            panic!("Native restore bytes missing: {error}; restored root entries: {names:?}");
        });
    assert_eq!(restored, binary);
    assert_eq!(
        std::fs::read(destination.join("-leading.txt")).unwrap(),
        b"managed local roundtrip"
    );
    assert_eq!(
        std::fs::read(destination.join("version.txt")).unwrap(),
        b"3"
    );
    let hash = hex::encode(Sha256::digest(&restored));
    assert_eq!(hash, hex::encode(Sha256::digest(&binary)));
    fixture.run(&[
        "snapshot".into(),
        "delete".into(),
        latest_id.clone(),
        "--delete".into(),
    ]);
    let remaining = fixture.list_ids();
    assert_eq!(remaining, vec![snapshots[1].id.clone()]);
    eprintln!("NATIVE_MANAGED_EVIDENCE backup_count=3 retention_deleted=1 restored_files=3 binary_bytes={} binary_sha256={hash} exact_delete=1 remaining_snapshots={}", restored.len(), remaining.len());
}
