use crate::backup_operations::AccountContext;
use crate::db::{self, DatabaseProfile};
use crate::kopia::StreamSourceCommand;
use crate::state::AppStateWrapper;
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const DATABASE_CREDENTIAL_SERVICE: &str = "SaveState Vault Database";
const SYSTEM_DATABASES: &[&str] = &["information_schema", "mysql", "performance_schema", "sys"];

#[derive(Debug, Clone)]
struct ConnectionTarget {
    username: String,
    host: String,
    port: u16,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseToolBundle {
    id: String,
    label: String,
    vendor: String,
    version: String,
    dump_executable: String,
    client_executable: String,
    supports_user_grants: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseConnectionResult {
    server_version: String,
    databases: Vec<String>,
    host: String,
    port: u16,
}

fn parse_connection_url(raw: &str) -> Result<ConnectionTarget> {
    let url = reqwest::Url::parse(raw.trim())
        .context("Connection string must look like mysql://username@127.0.0.1:3306")?;
    if !matches!(url.scheme(), "mysql" | "mariadb") {
        return Err(anyhow!(
            "Connection string must start with mysql:// or mariadb://"
        ));
    }
    if url.password().is_some() {
        return Err(anyhow!(
            "Keep the password in the separate password field so it is never stored in the connection string"
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(anyhow!(
            "Connection options in the URL are not supported yet; enter host, username and port only"
        ));
    }
    if !matches!(url.path(), "" | "/") {
        return Err(anyhow!(
            "Choose databases after testing the connection instead of placing one in the URL"
        ));
    }
    let username = url.username().trim().to_string();
    if username.is_empty() {
        return Err(anyhow!(
            "Connection string must include a database username"
        ));
    }
    let host = url
        .host_str()
        .filter(|host| !host.trim().is_empty())
        .ok_or_else(|| anyhow!("Connection string must include a database host"))?
        .to_string();
    Ok(ConnectionTarget {
        username,
        host,
        port: url.port().unwrap_or(3306),
    })
}

fn hidden_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

fn run_tool(program: &Path, args: &[OsString], password: Option<&str>) -> Result<Output> {
    if !program.is_file() {
        return Err(anyhow!(
            "DATABASE_TOOL_NOT_FOUND: {} does not exist",
            program.display()
        ));
    }
    let mut command = hidden_command(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(password) = password {
        command.env("MYSQL_PWD", password);
    }
    command
        .output()
        .with_context(|| format!("Could not run database tool at {}", program.display()))
}

fn command_error(output: &Output, fallback: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .last()
        .unwrap_or(fallback)
        .chars()
        .take(600)
        .collect()
}

fn connection_args(target: &ConnectionTarget) -> Vec<OsString> {
    vec![
        format!("--host={}", target.host).into(),
        format!("--port={}", target.port).into(),
        format!("--user={}", target.username).into(),
        "--protocol=tcp".into(),
    ]
}

fn client_connection_args(target: &ConnectionTarget) -> Vec<OsString> {
    let mut args = connection_args(target);
    // `mysql` and `mariadb` accept this option, but XAMPP's MariaDB
    // `mysqldump` rejects it as an unknown variable. Keep client-only
    // connection controls out of dump commands so detected tool bundles work
    // without an adapter executable.
    args.push("--connect-timeout=5".into());
    args
}

fn test_connection(
    connection_url: &str,
    password: &str,
    client_executable: &Path,
) -> Result<DatabaseConnectionResult> {
    let target = parse_connection_url(connection_url)?;
    let mut args = client_connection_args(&target);
    args.extend([
        "--batch".into(),
        "--skip-column-names".into(),
        "--raw".into(),
        "--execute=SELECT VERSION(); SHOW DATABASES;".into(),
    ]);
    let output = run_tool(client_executable, &args, Some(password))?;
    if !output.status.success() {
        let detail = command_error(&output, "The database did not accept the connection");
        let lower = detail.to_ascii_lowercase();
        let code = if lower.contains("access denied") {
            "DATABASE_AUTHENTICATION_FAILED"
        } else if lower.contains("can't connect")
            || lower.contains("cannot connect")
            || lower.contains("timed out")
        {
            "DATABASE_UNREACHABLE"
        } else {
            "DATABASE_CONNECTION_FAILED"
        };
        return Err(anyhow!("{code}: {detail}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let server_version = lines
        .next()
        .ok_or_else(|| anyhow!("Database connection succeeded but returned no server version"))?
        .to_string();
    let mut databases: Vec<String> = lines
        .filter(|name| !SYSTEM_DATABASES.contains(name))
        .map(ToString::to_string)
        .collect();
    databases.sort_by_key(|name| name.to_ascii_lowercase());
    databases.dedup();
    Ok(DatabaseConnectionResult {
        server_version,
        databases,
        host: target.host,
        port: target.port,
    })
}

fn tool_version(path: &Path) -> String {
    run_tool(path, &["--version".into()], None)
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .chars()
                .take(180)
                .collect()
        })
        .filter(|version: &String| !version.is_empty())
        .unwrap_or_else(|| "Version unavailable".to_string())
}

fn dump_supports_user_grants(path: &Path) -> bool {
    run_tool(path, &["--help".into()], None)
        .ok()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("--system=name")
        })
        .unwrap_or(false)
}

fn validate_dump_executable(path: &Path) -> Result<()> {
    let output = run_tool(path, &["--version".into()], None)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "DATABASE_TOOL_INVALID: {}",
            command_error(
                &output,
                "The selected dump executable could not be verified"
            )
        ))
    }
}

fn candidate_tool_directories() -> Vec<PathBuf> {
    let mut directories = BTreeSet::new();
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }
    directories.insert(PathBuf::from(r"C:\xampp\mysql\bin"));
    directories.insert(PathBuf::from(r"C:\xampp\mariadb\bin"));

    for base in [
        std::env::var_os("ProgramFiles"),
        std::env::var_os("ProgramFiles(x86)"),
    ]
    .into_iter()
    .flatten()
    .map(PathBuf::from)
    {
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if name.starts_with("mariadb") || name.starts_with("mysql") {
                    directories.insert(entry.path().join("bin"));
                }
            }
        }
        for vendor in ["MariaDB", "MySQL"] {
            let vendor_dir = base.join(vendor);
            if let Ok(entries) = std::fs::read_dir(vendor_dir) {
                for entry in entries.flatten() {
                    directories.insert(entry.path().join("bin"));
                }
            }
        }
    }
    directories.into_iter().collect()
}

fn discover_tools() -> Vec<DatabaseToolBundle> {
    let mut bundles = BTreeMap::<String, DatabaseToolBundle>::new();
    for directory in candidate_tool_directories() {
        let pairs = [
            ("mariadb-dump.exe", "mariadb.exe", "MariaDB"),
            ("mysqldump.exe", "mysql.exe", "MySQL / MariaDB"),
            ("mariadb-dump", "mariadb", "MariaDB"),
            ("mysqldump", "mysql", "MySQL / MariaDB"),
        ];
        for (dump_name, client_name, fallback_vendor) in pairs {
            let dump = directory.join(dump_name);
            let client = directory.join(client_name);
            if !dump.is_file() || !client.is_file() {
                continue;
            }
            let canonical_dump = dump.canonicalize().unwrap_or(dump);
            let canonical_client = client.canonicalize().unwrap_or(client);
            let key = canonical_dump.to_string_lossy().to_ascii_lowercase();
            if bundles.contains_key(&key) {
                continue;
            }
            let version = tool_version(&canonical_dump);
            let vendor = if version.to_ascii_lowercase().contains("mariadb") {
                "MariaDB"
            } else if version.to_ascii_lowercase().contains("mysql") {
                "MySQL"
            } else {
                fallback_vendor
            };
            let location = canonical_dump
                .parent()
                .map(|parent| parent.display().to_string())
                .unwrap_or_else(|| "Detected installation".to_string());
            let id = hex::encode(Sha256::digest(key.as_bytes()))[..16].to_string();
            bundles.insert(
                key,
                DatabaseToolBundle {
                    id,
                    label: format!("{vendor} at {location}"),
                    vendor: vendor.to_string(),
                    version,
                    supports_user_grants: dump_supports_user_grants(&canonical_dump),
                    dump_executable: canonical_dump.display().to_string(),
                    client_executable: canonical_client.display().to_string(),
                },
            );
        }
    }
    bundles.into_values().collect()
}

fn credential_username(owner_account: &str, profile_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(owner_account.as_bytes());
    hasher.update([0]);
    hasher.update(profile_id.as_bytes());
    format!("database-{}", hex::encode(hasher.finalize()))
}

fn credential_entry(owner_account: &str, profile_id: &str) -> Result<keyring::v1::Entry> {
    keyring::v1::Entry::new(
        DATABASE_CREDENTIAL_SERVICE,
        &credential_username(owner_account, profile_id),
    )
    .context("Windows Credential Manager is unavailable")
}

fn save_password(owner_account: &str, profile_id: &str, password: &str) -> Result<()> {
    let stored = format!("v1:{password}");
    credential_entry(owner_account, profile_id)?
        .set_secret(stored.as_bytes())
        .context("Could not protect the database password with Windows Credential Manager")
}

fn load_password(owner_account: &str, profile_id: &str) -> Result<String> {
    let secret = match credential_entry(owner_account, profile_id)?.get_secret() {
        Ok(secret) => secret,
        Err(primary_error) => {
            let legacy_owner = owner_account
                .split("::service:")
                .next()
                .unwrap_or(owner_account);
            if legacy_owner == owner_account {
                return Err(primary_error).context(
                    "Database credentials are missing; edit the connection and enter the password again",
                );
            }
            let legacy = credential_entry(legacy_owner, profile_id)?
                .get_secret()
                .context(
                    "Database credentials are missing; edit the connection and enter the password again",
                )?;
            credential_entry(owner_account, profile_id)?
                .set_secret(&legacy)
                .context("Could not migrate database credentials into the selected workspace")?;
            legacy
        }
    };
    let stored = String::from_utf8(secret).context("Stored database credentials are invalid")?;
    stored
        .strip_prefix("v1:")
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("Stored database credentials use an unsupported format"))
}

pub(crate) fn delete_profile_password(owner_account: &str, profile_id: &str) {
    if let Ok(entry) = credential_entry(owner_account, profile_id) {
        let _ = entry.delete_credential();
    }
    if let Some(legacy_owner) = owner_account.split("::service:").next() {
        if legacy_owner != owner_account {
            if let Ok(entry) = credential_entry(legacy_owner, profile_id) {
                let _ = entry.delete_credential();
            }
        }
    }
}

fn validate_selection(
    selection_mode: &str,
    databases: &[String],
    tables: &[String],
    include_new_databases: bool,
) -> Result<()> {
    let valid_name = |value: &str| {
        !value.trim().is_empty()
            && value.len() <= 128
            && !value.chars().any(|character| character.is_control())
    };
    if databases.iter().any(|database| !valid_name(database))
        || tables.iter().any(|table| !valid_name(table))
    {
        return Err(anyhow!("Database and table names contain an invalid value"));
    }
    match selection_mode {
        "all" if tables.is_empty() && (include_new_databases || !databases.is_empty()) => {}
        "databases" if !databases.is_empty() && tables.is_empty() => {}
        "tables" if databases.len() == 1 && !tables.is_empty() && !include_new_databases => {}
        _ => {
            return Err(anyhow!(
                "Choose every database, one or more databases, or tables from exactly one database"
            ))
        }
    }
    if include_new_databases && selection_mode != "all" {
        return Err(anyhow!(
            "Automatically include new databases is available with Every database"
        ));
    }
    Ok(())
}

fn build_dump_command(
    profile: &DatabaseProfile,
    password: &str,
    discovered_databases: &[String],
) -> Result<StreamSourceCommand> {
    let target = parse_connection_url(&profile.connection_url)?;
    let dump_path = PathBuf::from(&profile.dump_executable);
    if !dump_path.is_file() {
        return Err(anyhow!(
            "DATABASE_TOOL_NOT_FOUND: The configured dump tool is no longer available at {}",
            dump_path.display()
        ));
    }
    if profile.include_users_and_grants && !dump_supports_user_grants(&dump_path) {
        return Err(anyhow!(
            "DATABASE_GRANTS_UNSUPPORTED: This dump tool cannot export users and grants. Choose a MariaDB tool bundle or turn that option off."
        ));
    }

    let mut args = connection_args(&target);
    args.extend([
        "--default-character-set=utf8mb4".into(),
        "--single-transaction".into(),
        "--quick".into(),
        "--hex-blob".into(),
        "--routines".into(),
        "--events".into(),
        "--triggers".into(),
    ]);
    if profile.include_users_and_grants {
        args.push("--system=users".into());
    }

    match profile.selection_mode.as_str() {
        "all" => {
            let databases = if profile.include_new_databases {
                discovered_databases
            } else {
                &profile.databases
            };
            if databases.is_empty() {
                return Err(anyhow!("No user databases were found to back up"));
            }
            args.push("--databases".into());
            args.extend(databases.iter().map(OsString::from));
        }
        "databases" => {
            args.push("--databases".into());
            args.extend(profile.databases.iter().map(OsString::from));
        }
        "tables" => {
            args.push(OsString::from(&profile.databases[0]));
            args.extend(profile.tables.iter().map(OsString::from));
        }
        _ => return Err(anyhow!("Database selection mode is invalid")),
    }
    if !profile.include_create_statements && profile.selection_mode != "tables" {
        args.push("--no-create-db".into());
    }
    Ok(StreamSourceCommand {
        program: dump_path,
        args,
        env: vec![("MYSQL_PWD".into(), password.into())],
    })
}

fn build_restore_command(
    profile: &DatabaseProfile,
    password: &str,
) -> Result<crate::kopia::StreamRestoreCommand> {
    let target = parse_connection_url(&profile.connection_url)?;
    let client_path = PathBuf::from(&profile.client_executable);
    if !client_path.is_file() {
        return Err(anyhow!(
            "DATABASE_TOOL_NOT_FOUND: The configured restore tool is no longer available at {}",
            client_path.display()
        ));
    }
    let mut args = client_connection_args(&target);
    args.push("--binary-mode=1".into());
    if profile.selection_mode == "tables" {
        let database = profile
            .databases
            .first()
            .ok_or_else(|| anyhow!("The table backup has no destination database"))?;
        args.push(format!("--database={database}").into());
    }
    Ok(crate::kopia::StreamRestoreCommand {
        program: client_path,
        args,
        env: vec![("MYSQL_PWD".into(), password.into())],
    })
}

fn current_owner(state: &AppStateWrapper) -> std::result::Result<String, String> {
    let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
    guard
        .account_scope()
        .ok_or_else(|| "Sign in before managing database backups".to_string())
}

#[tauri::command]
pub async fn cmd_discover_database_tools(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<Vec<DatabaseToolBundle>, String> {
    current_owner(&state)?;
    tokio::task::spawn_blocking(discover_tools)
        .await
        .map_err(|error| format!("Database discovery stopped unexpectedly: {error}"))
}

#[tauri::command]
pub async fn cmd_test_database_connection(
    state: tauri::State<'_, AppStateWrapper>,
    connection_url: String,
    password: Option<String>,
    dump_executable: String,
    client_executable: String,
    profile_id: Option<String>,
) -> std::result::Result<DatabaseConnectionResult, String> {
    let owner_account = current_owner(&state)?;
    let password = match password {
        Some(password) => password,
        None => load_password(
            &owner_account,
            profile_id
                .as_deref()
                .ok_or_else(|| "Enter the database password before testing".to_string())?,
        )
        .map_err(|error| error.to_string())?,
    };
    tokio::task::spawn_blocking(move || {
        validate_dump_executable(Path::new(&dump_executable))?;
        test_connection(&connection_url, &password, Path::new(&client_executable))
    })
    .await
    .map_err(|error| format!("Connection test stopped unexpectedly: {error}"))?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_list_database_tables(
    state: tauri::State<'_, AppStateWrapper>,
    connection_url: String,
    password: Option<String>,
    client_executable: String,
    profile_id: Option<String>,
    database: String,
) -> std::result::Result<Vec<String>, String> {
    let owner_account = current_owner(&state)?;
    if database.trim().is_empty() || database.chars().any(|character| character.is_control()) {
        return Err("Choose a valid database first".to_string());
    }
    let password = match password {
        Some(password) => password,
        None => load_password(
            &owner_account,
            profile_id
                .as_deref()
                .ok_or_else(|| "Enter the database password before loading tables".to_string())?,
        )
        .map_err(|error| error.to_string())?,
    };
    tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        let target = parse_connection_url(&connection_url)?;
        let mut args = client_connection_args(&target);
        args.extend([
            format!("--database={database}").into(),
            "--batch".into(),
            "--skip-column-names".into(),
            "--raw".into(),
            "--execute=SHOW FULL TABLES WHERE Table_type IN ('BASE TABLE','VIEW');".into(),
        ]);
        let output = run_tool(Path::new(&client_executable), &args, Some(&password))?;
        if !output.status.success() {
            return Err(anyhow!(
                "DATABASE_TABLE_LIST_FAILED: {}",
                command_error(&output, "Could not list tables")
            ));
        }
        let mut tables: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split('\t').next())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToString::to_string)
            .collect();
        tables.sort_by_key(|name| name.to_ascii_lowercase());
        tables.dedup();
        Ok(tables)
    })
    .await
    .map_err(|error| format!("Table discovery stopped unexpectedly: {error}"))?
    .map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn cmd_create_database_profile(
    state: tauri::State<'_, AppStateWrapper>,
    name: String,
    connection_url: String,
    password: String,
    dump_executable: String,
    client_executable: String,
    selection_mode: String,
    databases: Vec<String>,
    tables: Vec<String>,
    include_new_databases: bool,
    include_create_statements: bool,
    include_users_and_grants: bool,
    schedule: Option<String>,
) -> std::result::Result<DatabaseProfile, String> {
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 80 {
        return Err("Enter a database backup name up to 80 characters".to_string());
    }
    validate_selection(&selection_mode, &databases, &tables, include_new_databases)
        .map_err(|error| error.to_string())?;
    let owner_account = current_owner(&state)?;
    let connection = {
        let connection_url = connection_url.clone();
        let password = password.clone();
        let dump_executable = dump_executable.clone();
        let client_executable = client_executable.clone();
        tokio::task::spawn_blocking(move || {
            let dump_path = Path::new(&dump_executable);
            validate_dump_executable(dump_path)?;
            if include_users_and_grants && !dump_supports_user_grants(dump_path) {
                return Err(anyhow!(
                    "DATABASE_GRANTS_UNSUPPORTED: This dump tool cannot export users and grants"
                ));
            }
            test_connection(&connection_url, &password, Path::new(&client_executable))
        })
        .await
        .map_err(|error| format!("Connection test stopped unexpectedly: {error}"))?
        .map_err(|error| error.to_string())?
    };
    if (selection_mode != "all" || !include_new_databases)
        && databases
            .iter()
            .any(|database| !connection.databases.contains(database))
    {
        return Err("One or more selected databases are no longer available".to_string());
    }

    let requested_is_automated = schedule
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let api = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
        guard.api.clone()
    };
    let profile_limit = if requested_is_automated {
        Some(
            api.get_entitlements()
                .await
                .map_err(|error| {
                    format!("Could not verify your automated-profile allowance: {error}")
                })?
                .profile_limit
                .unwrap_or(2) as usize,
        )
    } else {
        None
    };
    let mut profile = DatabaseProfile {
        id: uuid::Uuid::new_v4().to_string(),
        owner_account: owner_account.clone(),
        name,
        connection_url,
        dump_executable,
        client_executable,
        selection_mode,
        databases,
        tables,
        include_new_databases,
        include_create_statements,
        include_users_and_grants,
        schedule: schedule.clone(),
        retention: 0,
        folder: "/".to_string(),
        enabled: true,
        last_run: None,
        next_run: crate::profiles::compute_next_run(schedule.as_deref()),
        retry_count: 0,
        retry_at: None,
        last_error: None,
        last_error_code: None,
        schedule_state: "scheduled".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        has_credentials: true,
    };
    {
        let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
        if let Some(limit) = profile_limit {
            let used = db::count_scheduled_file_profiles_for_account(&guard.db, &owner_account)
                .and_then(|files| {
                    db::count_scheduled_database_profiles_for_account(&guard.db, &owner_account)
                        .map(|databases| files + databases)
                })
                .map_err(|error| error.to_string())?;
            if used >= limit {
                return Err(format!(
                    "AUTOMATED_PROFILE_LIMIT_REACHED: Your plan allows up to {limit} enabled scheduled backup profiles. Keep this database manual-only, pause another schedule, or upgrade your plan."
                ));
            }
        }
    }
    save_password(&owner_account, &profile.id, &password).map_err(|error| error.to_string())?;
    profile.folder = match api.ensure_profile_folder(&profile.id, &profile.name).await {
        Ok(folder) => folder,
        Err(error) => {
            delete_profile_password(&owner_account, &profile.id);
            return Err(error.to_string());
        }
    };
    let create_result = state
        .0
        .lock()
        .map_err(|error| format!("Lock: {error}"))
        .and_then(|guard| {
            db::create_database_profile(&guard.db, &profile).map_err(|error| error.to_string())
        });
    if let Err(error) = create_result {
        delete_profile_password(&owner_account, &profile.id);
        let _ = api.detach_profile_folder(&profile.id).await;
        return Err(error);
    }
    crate::profiles::report_schedule_snapshot(&state);
    Ok(profile)
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn cmd_update_database_profile(
    state: tauri::State<'_, AppStateWrapper>,
    id: String,
    name: String,
    connection_url: String,
    password: Option<String>,
    dump_executable: String,
    client_executable: String,
    selection_mode: String,
    databases: Vec<String>,
    tables: Vec<String>,
    include_new_databases: bool,
    include_create_statements: bool,
    include_users_and_grants: bool,
    schedule: Option<String>,
    enabled: bool,
) -> std::result::Result<DatabaseProfile, String> {
    let name = name.trim().to_string();
    if name.is_empty() || name.len() > 80 {
        return Err("Enter a database backup name up to 80 characters".to_string());
    }
    validate_selection(&selection_mode, &databases, &tables, include_new_databases)
        .map_err(|error| error.to_string())?;
    let owner_account = current_owner(&state)?;
    let existing = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
        db::get_database_profile_for_account(&guard.db, &id, &owner_account)
            .map_err(|error| error.to_string())?
    };
    let resolved_password = match password.as_ref() {
        Some(password) => password.clone(),
        None => load_password(&owner_account, &id).map_err(|error| error.to_string())?,
    };
    {
        let connection_url = connection_url.clone();
        let dump_executable = dump_executable.clone();
        let client_executable = client_executable.clone();
        let password = resolved_password.clone();
        let available = tokio::task::spawn_blocking(move || {
            let dump_path = Path::new(&dump_executable);
            validate_dump_executable(dump_path)?;
            if include_users_and_grants && !dump_supports_user_grants(dump_path) {
                return Err(anyhow!(
                    "DATABASE_GRANTS_UNSUPPORTED: This dump tool cannot export users and grants"
                ));
            }
            test_connection(&connection_url, &password, Path::new(&client_executable))
        })
        .await
        .map_err(|error| format!("Connection test stopped unexpectedly: {error}"))?
        .map_err(|error| error.to_string())?;
        if (selection_mode != "all" || !include_new_databases)
            && databases
                .iter()
                .any(|database| !available.databases.contains(database))
        {
            return Err("One or more selected databases are no longer available".to_string());
        }
    }

    let requested_is_automated = enabled
        && schedule
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    let was_automated = existing.enabled
        && existing
            .schedule
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    if requested_is_automated && !was_automated {
        let api = {
            let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
            guard.api.clone()
        };
        let limit = api
            .get_entitlements()
            .await
            .map_err(|error| format!("Could not verify your automated-profile allowance: {error}"))?
            .profile_limit
            .unwrap_or(2) as usize;
        let used = {
            let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
            db::count_scheduled_file_profiles_for_account(&guard.db, &owner_account)
                .and_then(|files| {
                    db::count_scheduled_database_profiles_for_account(&guard.db, &owner_account)
                        .map(|databases| files + databases)
                })
                .map_err(|error| error.to_string())?
        };
        if used >= limit {
            return Err(format!(
                "AUTOMATED_PROFILE_LIMIT_REACHED: Your plan allows up to {limit} enabled scheduled backup profiles."
            ));
        }
    }

    let api = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
        guard.api.clone()
    };
    let managed_folder = api
        .ensure_profile_folder(&id, &name)
        .await
        .map_err(|error| error.to_string())?;
    let mut profile = existing;
    profile.name = name;
    profile.connection_url = connection_url;
    profile.dump_executable = dump_executable;
    profile.client_executable = client_executable;
    profile.selection_mode = selection_mode;
    profile.databases = databases;
    profile.tables = tables;
    profile.include_new_databases = include_new_databases;
    profile.include_create_statements = include_create_statements;
    profile.include_users_and_grants = include_users_and_grants;
    profile.schedule = schedule.clone();
    profile.folder = managed_folder;
    profile.enabled = enabled;
    profile.next_run = if enabled {
        crate::profiles::compute_next_run(schedule.as_deref())
    } else {
        None
    };
    profile.retry_count = 0;
    profile.retry_at = None;
    profile.last_error = None;
    profile.last_error_code = None;
    profile.schedule_state = if enabled { "scheduled" } else { "disabled" }.to_string();
    if password.is_some() {
        save_password(&owner_account, &id, &resolved_password)
            .map_err(|error| error.to_string())?;
    }
    {
        let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
        db::update_database_profile(&guard.db, &profile).map_err(|error| error.to_string())?;
    }
    crate::profiles::report_schedule_snapshot(&state);
    Ok(profile)
}

#[tauri::command]
pub async fn cmd_list_database_profiles(
    state: tauri::State<'_, AppStateWrapper>,
) -> std::result::Result<Vec<DatabaseProfile>, String> {
    let owner_account = current_owner(&state)?;
    let (mut profiles, api) = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
        (
            db::list_database_profiles_for_account(&guard.db, &owner_account)
                .map_err(|error| error.to_string())?,
            guard.api.clone(),
        )
    };
    for profile in &mut profiles {
        if let Ok(folder) = api.ensure_profile_folder(&profile.id, &profile.name).await {
            if folder != profile.folder {
                profile.folder = folder;
                let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
                db::update_database_profile(&guard.db, profile)
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(profiles)
}

#[tauri::command]
pub async fn cmd_delete_database_profile(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    id: String,
    delete_backups: Option<bool>,
) -> std::result::Result<(), String> {
    let owner_account = current_owner(&state)?;
    let (profile, api) = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
        let profile = db::get_database_profile_for_account(&guard.db, &id, &owner_account)
            .map_err(|error| error.to_string())?;
        (profile, guard.api.clone())
    };
    if delete_backups.unwrap_or(false) {
        crate::backup::delete_snapshots_in_folder(&app, state.inner(), &profile.folder)
            .await
            .map_err(|error| error.to_string())?;
        api.delete_folder(&profile.folder)
            .await
            .map_err(|error| error.to_string())?;
    } else {
        api.detach_profile_folder(&profile.id)
            .await
            .map_err(|error| error.to_string())?;
    }
    {
        let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
        db::delete_database_profile_for_account(&guard.db, &id, &owner_account)
            .map_err(|error| error.to_string())?;
    }
    delete_profile_password(&owner_account, &id);
    crate::profiles::report_schedule_snapshot(&state);
    Ok(())
}

#[tauri::command]
pub async fn cmd_run_database_backup(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    profile_id: String,
) -> std::result::Result<String, String> {
    let (api, profile_name) = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
        let owner_account = guard
            .account_scope()
            .ok_or_else(|| "Sign in before running a database backup".to_string())?;
        let profile = db::get_database_profile_for_account(&guard.db, &profile_id, &owner_account)
            .map_err(|error| error.to_string())?;
        (guard.api.clone(), profile.name)
    };
    let result = run_database_backup_inner(app, &state, &profile_id, "database_manual").await;
    match &result {
        Ok(backup_id) => {
            crate::notifications::send_backup_notification(
                &api,
                "backup_success",
                &profile_name,
                &format!("Manual database backup completed. ID: {backup_id}"),
            )
            .await;
        }
        Err(error) if error.to_string().contains("BACKUP_CANCELLED") => {}
        Err(_) => {
            crate::notifications::send_backup_notification(
                &api,
                "backup_failure",
                &profile_name,
                "Manual database backup failed. Open SaveState Vault for details.",
            )
            .await;
        }
    }
    result.map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn cmd_restore_database_backup(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppStateWrapper>,
    profile_id: String,
    snapshot_id: String,
) -> std::result::Result<(), String> {
    let (profile, password) = {
        let guard = state.0.lock().map_err(|error| format!("Lock: {error}"))?;
        let owner_account = guard
            .account_scope()
            .ok_or_else(|| "Sign in before restoring a database".to_string())?;
        let profile = db::get_database_profile_for_account(&guard.db, &profile_id, &owner_account)
            .map_err(|error| error.to_string())?;
        let password =
            load_password(&owner_account, &profile_id).map_err(|error| error.to_string())?;
        (profile, password)
    };
    test_connection(
        &profile.connection_url,
        &password,
        Path::new(&profile.client_executable),
    )
    .map_err(|error| error.to_string())?;
    let target = build_restore_command(&profile, &password).map_err(|error| error.to_string())?;
    crate::kopia::restore_database_snapshot_to_command(
        &app,
        state.inner(),
        &snapshot_id,
        &profile_id,
        target,
    )
    .await
    .map_err(|error| error.to_string())
}

pub async fn run_database_backup_inner(
    app: tauri::AppHandle,
    state: &AppStateWrapper,
    profile_id: &str,
    trigger: &'static str,
) -> Result<String> {
    let context = {
        let guard = state.0.lock().map_err(|error| anyhow!("Lock: {error}"))?;
        AccountContext::capture(&guard)?
    };
    run_database_backup_with_context(app, state, profile_id, trigger, context).await
}

pub async fn run_database_backup_with_context(
    app: tauri::AppHandle,
    state: &AppStateWrapper,
    profile_id: &str,
    trigger: &'static str,
    context: AccountContext,
) -> Result<String> {
    let operation =
        crate::backup_operations::begin_with_context(state, context, "Database backup")?;
    crate::kopia::prepare_repository_for_backup(&app, &operation).await?;
    let result: Result<String> = async {
        let mut profile = {
            let guard = state.0.lock().map_err(|error| anyhow!("Lock: {error}"))?;
            db::get_database_profile_for_account(&guard.db, profile_id, operation.account_scope())?
        };
        operation.set_name(profile.name.clone());
        let managed_folder = operation
            .api()
            .ensure_profile_folder(&profile.id, &profile.name)
            .await?;
        if managed_folder != profile.folder {
            profile.folder = managed_folder;
            let guard = state.0.lock().map_err(|error| anyhow!("Lock: {error}"))?;
            db::update_database_profile(&guard.db, &profile)?;
        }
        if profile.retention > 0 {
            crate::kopia::prune_profile_snapshots_with_operation(
                &app,
                &operation,
                &profile.id,
                &profile.folder,
                profile.retention as usize,
            )
            .await?;
        }
        let password = load_password(operation.account_scope(), profile_id)?;
        let connection = test_connection(
            &profile.connection_url,
            &password,
            Path::new(&profile.client_executable),
        )?;
        let source = build_dump_command(&profile, &password, &connection.databases)?;
        let filename = format!("savestate-database/{profile_id}/database.sql");
        let backup_id = crate::kopia::backup_stream_with_operation(
            &app,
            &operation,
            source,
            filename,
            profile_id,
            &profile.name,
            trigger,
            &profile.folder,
            (profile.retention > 0).then_some(profile.retention as usize),
        )
        .await?;
        if profile.retention > 0 {
            crate::kopia::prune_profile_snapshots_with_operation(
                &app,
                &operation,
                &profile.id,
                &profile.folder,
                profile.retention as usize,
            )
            .await?;
        }
        let now = chrono::Utc::now().to_rfc3339();
        let next = crate::profiles::compute_next_run(profile.schedule.as_deref());
        let guard = state.0.lock().map_err(|error| anyhow!("Lock: {error}"))?;
        db::update_database_profile_run_times(
            &guard.db,
            profile_id,
            operation.account_scope(),
            &now,
            next.as_deref(),
        )?;
        Ok(backup_id)
    }
    .await;
    operation.finish_tracking().await;
    result
}

pub fn database_profile_is_due(
    profile: &DatabaseProfile,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if !profile.enabled {
        return false;
    }
    let candidate = if profile.schedule_state == "retrying" {
        profile.retry_at.as_deref()
    } else {
        profile.next_run.as_deref()
    };
    candidate
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&chrono::Utc) <= now)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        build_dump_command, client_connection_args, credential_username, parse_connection_url,
        validate_selection,
    };
    use crate::db::DatabaseProfile;

    fn profile(mode: &str) -> DatabaseProfile {
        DatabaseProfile {
            id: "db-profile".into(),
            owner_account: "account".into(),
            name: "Database".into(),
            connection_url: "mysql://root@127.0.0.1:3306".into(),
            // The command builder validates that the executable still exists. Use the
            // current test binary so this unit test does not depend on XAMPP being
            // installed on the CI runner.
            dump_executable: std::env::current_exe()
                .expect("test executable path")
                .to_string_lossy()
                .into_owned(),
            client_executable: String::new(),
            selection_mode: mode.into(),
            databases: vec!["app".into()],
            tables: Vec::new(),
            include_new_databases: false,
            include_create_statements: true,
            include_users_and_grants: false,
            schedule: None,
            retention: 0,
            folder: "/".into(),
            enabled: true,
            last_run: None,
            next_run: None,
            retry_count: 0,
            retry_at: None,
            last_error: None,
            last_error_code: None,
            schedule_state: "scheduled".into(),
            created_at: "2026-08-24T00:00:00Z".into(),
            has_credentials: true,
        }
    }

    #[test]
    fn connection_url_keeps_password_out_of_persisted_configuration() {
        let parsed = parse_connection_url("mysql://root@localhost:3307").unwrap();
        assert_eq!(parsed.username, "root");
        assert_eq!(parsed.host, "localhost");
        assert_eq!(parsed.port, 3307);
        assert!(parse_connection_url("mysql://root:secret@localhost:3306").is_err());
    }

    #[test]
    fn selection_contract_covers_all_databases_multiple_databases_and_tables() {
        assert!(validate_selection("all", &[], &[], true).is_ok());
        assert!(validate_selection("all", &["one".into(), "two".into()], &[], false).is_ok());
        assert!(validate_selection("all", &[], &[], false).is_err());
        assert!(validate_selection("databases", &["one".into(), "two".into()], &[], false).is_ok());
        assert!(validate_selection("tables", &["one".into()], &["users".into()], false).is_ok());
        assert!(validate_selection(
            "tables",
            &["one".into(), "two".into()],
            &["users".into()],
            false
        )
        .is_err());
    }

    #[test]
    fn credential_lookup_is_account_scoped_and_stable() {
        assert_eq!(
            credential_username("a", "id"),
            credential_username("a", "id")
        );
        assert_ne!(
            credential_username("a", "id"),
            credential_username("b", "id")
        );
    }

    #[test]
    #[cfg(windows)]
    fn dump_command_uses_arguments_and_environment_instead_of_password_arguments() {
        let command =
            build_dump_command(&profile("databases"), "top-secret", &["app".into()]).unwrap();
        assert!(command
            .args
            .iter()
            .all(|arg| !arg.to_string_lossy().contains("top-secret")));
        assert!(command
            .env
            .iter()
            .any(|(key, value)| key == "MYSQL_PWD" && value == "top-secret"));
        assert!(command.args.iter().any(|arg| arg == "--routines"));
        assert!(command.args.iter().any(|arg| arg == "--events"));
        assert!(command.args.iter().any(|arg| arg == "--triggers"));
        assert!(command
            .args
            .iter()
            .all(|arg| !arg.to_string_lossy().starts_with("--connect-timeout=")));

        let fixed = build_dump_command(
            &profile("all"),
            "top-secret",
            &["app".into(), "new_database".into()],
        )
        .unwrap();
        assert!(fixed.args.iter().any(|arg| arg == "app"));
        assert!(!fixed.args.iter().any(|arg| arg == "new_database"));

        let mut automatic = profile("all");
        automatic.include_new_databases = true;
        automatic.databases.clear();
        let dynamic = build_dump_command(
            &automatic,
            "top-secret",
            &["app".into(), "new_database".into()],
        )
        .unwrap();
        assert!(dynamic.args.iter().any(|arg| arg == "new_database"));
    }

    #[test]
    fn connection_timeout_is_kept_on_mysql_client_commands() {
        let target = parse_connection_url("mysql://root@127.0.0.1:3306").unwrap();
        let args = client_connection_args(&target);
        assert!(args.iter().any(|arg| arg == "--connect-timeout=5"));
    }
}
