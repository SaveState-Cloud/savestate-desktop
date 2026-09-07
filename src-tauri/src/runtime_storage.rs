//! Keep non-production builds away from production sessions, profiles and caches.
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn namespace(environment: &str, endpoint: &str) -> Option<String> {
    match (environment, endpoint.trim_end_matches('/')) {
        ("production", "https://api.savestate.dk") => None,
        ("development", "https://api-dev.savestate.dk") => Some("Development".into()),
        ("staging", "https://api-staging.savestate.dk") => Some("Staging".into()),
        _ => {
            let mut hash = Sha256::new();
            hash.update(environment.as_bytes());
            hash.update([0]);
            hash.update(endpoint.as_bytes());
            Some(format!("Custom-{}", hex::encode(hash.finalize())))
        }
    }
}

fn compiled_namespace() -> Option<String> {
    namespace(
        option_env!("SAVESTATE_ENVIRONMENT").unwrap_or("production"),
        option_env!("SAVESTATE_API_BASE_URL").unwrap_or("https://api.savestate.dk"),
    )
}

pub(crate) fn is_production() -> bool {
    compiled_namespace().is_none()
}

pub(crate) fn credential_service(production_name: &str) -> String {
    match compiled_namespace() {
        None => production_name.into(),
        Some(suffix) => format!("{production_name} {suffix}"),
    }
}

pub(crate) fn data_dir() -> PathBuf {
    let folder = match compiled_namespace() {
        None => "SaveState".into(),
        Some(suffix) => format!("SaveState-{suffix}"),
    };
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_production_environment_and_endpoint_use_legacy_storage() {
        assert_eq!(namespace("production", "https://api.savestate.dk"), None);
        assert_eq!(namespace("production", "https://api.savestate.dk/"), None);
        assert!(namespace("development", "https://api.savestate.dk").is_some());
        assert!(namespace("production", "https://api-dev.savestate.dk").is_some());
    }

    #[test]
    fn environment_storage_is_stable_distinct_and_path_safe() {
        let dev = namespace("development", "https://api-dev.savestate.dk");
        let staging = namespace("staging", "https://api-staging.savestate.dk");
        assert_eq!(dev.as_deref(), Some("Development"));
        assert_eq!(staging.as_deref(), Some("Staging"));
        assert_ne!(dev, staging);
        let custom = namespace("../../custom", "http://localhost:8000").unwrap();
        assert!(!custom.contains(['/', '\\', ':', '.']));
        assert_eq!(
            Some(custom.clone()),
            namespace("../../custom", "http://localhost:8000")
        );
        assert_ne!(
            Some(custom),
            namespace("../../custom", "http://localhost:8001")
        );
    }
}
