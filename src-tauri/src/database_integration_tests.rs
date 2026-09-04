//! Opt-in native test: fresh MariaDB datadir + ephemeral loopback port + local
//! Kopia repository. Never reads SaveState settings/keyring or installed DB data.
use super::*;
use crate::backup_operations::BackupControl;
use crate::kopia::{
    database_content_object_from_value, run_database_restore_pipeline_commands,
    run_stream_pipeline_commands,
};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    server: Child,
    root: tempfile::TempDir,
    bin: PathBuf,
    kopia: PathBuf,
    port: u16,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only our child; never stop processes by name or alter system services.
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

fn checked(command: Command) -> Output {
    let output =
        crate::subprocess::run(command, crate::subprocess::Limits::default(), &|| false).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

impl Fixture {
    fn start() -> Self {
        let bin = PathBuf::from(
            std::env::var("SAVESTATE_TEST_MYSQL_BIN")
                .expect("Set SAVESTATE_TEST_MYSQL_BIN explicitly"),
        );
        let kopia = PathBuf::from(
            std::env::var("SAVESTATE_TEST_KOPIA_BIN")
                .expect("Set SAVESTATE_TEST_KOPIA_BIN explicitly"),
        );
        let root = tempfile::Builder::new()
            .prefix("savestate-native-audit-")
            .tempdir()
            .unwrap();
        let data = root.path().join("database");
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut init = hidden_command(&bin.join("mysql_install_db.exe"));
        init.arg(format!("--datadir={}", data.display()))
            .arg("--silent");
        checked(init);
        let mut start = hidden_command(&bin.join("mysqld.exe"));
        start.args([
            "--no-defaults".to_string(),
            format!("--datadir={}", data.display()),
            format!("--port={port}"),
            "--bind-address=127.0.0.1".to_string(),
            "--skip-name-resolve".to_string(),
            "--innodb-buffer-pool-size=32M".to_string(),
            format!("--pid-file={}", root.path().join("server.pid").display()),
            format!("--log-error={}", root.path().join("server.log").display()),
            "--console".to_string(),
        ]);
        start
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        drop(listener);
        let server = start.spawn().unwrap();
        let mut fixture = Self {
            server,
            root,
            bin,
            kopia,
            port,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                fixture.server.try_wait().unwrap().is_none(),
                "Disposable DB exited"
            );
            let mut command = fixture.client();
            command.arg("--execute=SELECT 1");
            if crate::subprocess::run(command, crate::subprocess::Limits::default(), &|| false)
                .is_ok_and(|output| output.status.success())
            {
                break;
            }
            assert!(Instant::now() < deadline, "Disposable DB did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut create = fixture.kopia();
        create.args([
            "repository",
            "create",
            "filesystem",
            "--no-check-for-updates",
        ]);
        create.arg(format!(
            "--path={}",
            fixture.root.path().join("repository").display()
        ));
        create.arg(format!(
            "--cache-directory={}",
            fixture.root.path().join("cache").display()
        ));
        checked(create);
        fixture
    }

    fn client(&self) -> Command {
        let mut command = hidden_command(&self.bin.join("mysql.exe"));
        command.args([
            "--no-defaults",
            "--protocol=tcp",
            "--host=127.0.0.1",
            "--user=root",
            "--connect-timeout=1",
            "--batch",
            "--skip-column-names",
        ]);
        command
            .arg(format!("--port={}", self.port))
            .env("MYSQL_PWD", "");
        command
    }

    fn sql(&self, sql: &str) -> String {
        let mut command = self.client();
        command.arg(format!("--execute={sql}"));
        String::from_utf8_lossy(&checked(command).stdout)
            .trim()
            .to_string()
    }

    fn kopia(&self) -> Command {
        let mut command = hidden_command(&self.kopia);
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
        // Public synthetic fixture password, never installed in the keychain.
        command
            .env("KOPIA_PASSWORD", "public-disposable-test-fixture")
            .env("KOPIA_CACHE_DIRECTORY", self.root.path().join("cache"))
            .env("KOPIA_LOG_DIR", self.root.path().join("logs"))
            .env("KOPIA_CHECK_FOR_UPDATES", "false");
        command
    }

    fn profile(&self, mode: &str, databases: &[&str], tables: &[&str]) -> DatabaseProfile {
        let mut profile = super::tests::profile(mode);
        profile.connection_url = format!("mysql://root@127.0.0.1:{}", self.port);
        profile.dump_executable = self
            .bin
            .join("mysqldump.exe")
            .to_string_lossy()
            .into_owned();
        profile.client_executable = self.bin.join("mysql.exe").to_string_lossy().into_owned();
        profile.databases = databases.iter().map(|x| x.to_string()).collect();
        profile.tables = tables.iter().map(|x| x.to_string()).collect();
        profile
    }

    fn snapshot_command(&self) -> Command {
        let mut command = self.kopia();
        command.args([
            "snapshot",
            "create",
            "--stdin-file=database.sql",
            "--json",
            "--no-progress",
            "-",
        ]);
        command
    }

    fn snapshot(&self, profile: &DatabaseProfile) -> String {
        let mut source = build_dump_command(profile, "", &[]).unwrap();
        source.args.insert(0, "--no-defaults".into()); // Ignore machine option files only in this fixture.
        let pipeline = run_stream_pipeline_commands(
            source,
            self.snapshot_command(),
            &BackupControl::fixture(),
        )
        .unwrap();
        assert!(
            pipeline.source_status.success(),
            "{}",
            String::from_utf8_lossy(&pipeline.source_stderr)
        );
        assert!(
            pipeline.kopia_output.status.success(),
            "{}",
            String::from_utf8_lossy(&pipeline.kopia_output.stderr)
        );
        assert!(pipeline.source_bytes > 0);
        let snapshot: serde_json::Value =
            serde_json::from_slice(&pipeline.kopia_output.stdout).unwrap();
        let object = snapshot["rootEntry"]["obj"].as_str().unwrap();
        let mut command = self.kopia();
        command.args(["content", "show", object]);
        let directory: serde_json::Value =
            serde_json::from_slice(&checked(command).stdout).unwrap();
        database_content_object_from_value(&directory).unwrap()
    }

    fn restore(&self, profile: &DatabaseProfile, content: &str) -> Result<u64> {
        let mut target = build_restore_command(profile, "").unwrap();
        target.args.insert(0, "--no-defaults".into());
        let mut command = self.kopia();
        command.args(["content", "show", content]);
        run_database_restore_pipeline_commands(command, target, "disposable-native-restore")
    }
}

#[test]
#[cfg(windows)]
#[ignore = "Requires explicitly configured local MariaDB and Kopia; creates disposable fixtures only"]
fn native_database_kopia_roundtrip_and_failure_paths() {
    let fixture = Fixture::start();
    let files = fixture.root.path().join("source-files");
    std::fs::create_dir_all(files.join("nested")).unwrap();
    let binary: Vec<u8> = (0..8192).map(|n| (n % 256) as u8).collect();
    std::fs::write(files.join("nested").join("payload.bin"), &binary).unwrap();
    std::fs::write(files.join("-leading.txt"), b"disposable file fixture").unwrap();
    let mut snapshot = fixture.kopia();
    snapshot
        .args(["snapshot", "create", "--json", "--no-progress"])
        .arg(&files);
    let metadata: serde_json::Value = serde_json::from_slice(&checked(snapshot).stdout).unwrap();
    let restored_files = fixture.root.path().join("restored-files");
    let mut restore = fixture.kopia();
    restore
        .args(["snapshot", "restore", metadata["id"].as_str().unwrap()])
        .arg(&restored_files);
    checked(restore);
    assert_eq!(
        std::fs::read(restored_files.join("nested").join("payload.bin")).unwrap(),
        binary
    );
    assert_eq!(
        std::fs::read(restored_files.join("-leading.txt")).unwrap(),
        b"disposable file fixture"
    );
    fixture.sql("CREATE DATABASE `--help`; CREATE TABLE `--help`.`--version` (id INT PRIMARY KEY, payload BLOB, label VARCHAR(100)); INSERT INTO `--help`.`--version` VALUES(1, X'0001FF7F', 'original'),(2, X'102030', 'second'); CREATE DATABASE ordinary; CREATE TABLE ordinary.items (id INT PRIMARY KEY); INSERT INTO ordinary.items VALUES (7); CREATE TRIGGER `--help`.audit_insert BEFORE INSERT ON `--help`.`--version` FOR EACH ROW SET NEW.label=CONCAT('trigger-', NEW.label); CREATE PROCEDURE `--help`.row_count() SELECT COUNT(*) FROM `--help`.`--version`; CREATE EVENT `--help`.future_event ON SCHEDULE AT CURRENT_TIMESTAMP + INTERVAL 1 DAY DO INSERT INTO ordinary.items VALUES (99);");
    let digest =
        fixture.sql("SELECT id, HEX(payload), label FROM `--help`.`--version` ORDER BY id");
    let profile = fixture.profile("databases", &["--help", "ordinary"], &[]);
    let content = fixture.snapshot(&profile);
    fixture.sql("DROP DATABASE `--help`; DROP DATABASE ordinary;");
    assert!(fixture.restore(&profile, &content).unwrap() > 0);
    assert_eq!(
        fixture.sql("SELECT id, HEX(payload), label FROM `--help`.`--version` ORDER BY id"),
        digest
    );
    assert_eq!(fixture.sql("SELECT id FROM ordinary.items"), "7");
    assert_eq!(
        fixture
            .sql("SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA='--help'"),
        "1"
    );
    assert_eq!(
        fixture
            .sql("SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA='--help'"),
        "1"
    );
    assert_eq!(
        fixture.sql("SELECT COUNT(*) FROM information_schema.EVENTS WHERE EVENT_SCHEMA='--help'"),
        "1"
    );
    fixture.sql("INSERT INTO `--help`.`--version` VALUES(3, X'FE', 'new')");
    assert_eq!(
        fixture.sql("SELECT label FROM `--help`.`--version` WHERE id=3"),
        "trigger-new"
    );
    let table_profile = fixture.profile("tables", &["--help"], &["--version"]);
    let expected =
        fixture.sql("SELECT id, HEX(payload), label FROM `--help`.`--version` ORDER BY id");
    let table_content = fixture.snapshot(&table_profile);
    fixture.sql("DROP TABLE `--help`.`--version`");
    fixture.restore(&table_profile, &table_content).unwrap();
    assert_eq!(
        fixture.sql("SELECT id, HEX(payload), label FROM `--help`.`--version` ORDER BY id"),
        expected
    );
    let missing = fixture.profile("databases", &["not_present"], &[]);
    let mut source = build_dump_command(&missing, "", &[]).unwrap();
    source.args.insert(0, "--no-defaults".into());
    let failed = run_stream_pipeline_commands(
        source,
        fixture.snapshot_command(),
        &BackupControl::fixture(),
    )
    .unwrap();
    assert!(!failed.source_status.success());
    assert!(fixture.restore(&profile, "invalid-content-object").is_err());
    fixture.sql("DROP DATABASE `--help`");
    let error = fixture.restore(&table_profile, &table_content).unwrap_err();
    assert!(
        error.to_string().contains("DATABASE_RESTORE_FAILED"),
        "{error:#}"
    );
    let root = fixture.root.path().to_path_buf();
    let port = fixture.port;
    drop(fixture);
    assert!(
        !root.exists(),
        "Disposable fixture directory should be removed"
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "Disposable DB listener should be stopped"
    );
}
