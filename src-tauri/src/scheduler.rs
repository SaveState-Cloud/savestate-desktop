use crate::db::BackupProfile;
use chrono::{DateTime, Duration, Utc};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub const MAX_SCHEDULE_RETRIES: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureClassification {
    pub code: &'static str,
    pub retryable: bool,
}

/// Only sanitized, bounded error codes leave the client. Raw errors may
/// contain a source path and remain solely in the local SQLite database.
pub fn classify_schedule_failure(error: &str) -> FailureClassification {
    let value = error.to_ascii_lowercase();
    if value.contains("source path does not exist") {
        return FailureClassification {
            code: "source_missing",
            retryable: false,
        };
    }
    if value.contains("repository_key_mismatch")
        || value.contains("message authentication failed")
        || value.contains("unable to decrypt content")
    {
        return FailureClassification {
            code: "repository_key_mismatch",
            retryable: false,
        };
    }
    if value.contains("storage limit")
        || value.contains("storage quota")
        || value.contains("insufficient storage")
    {
        return FailureClassification {
            code: "storage_limit_reached",
            retryable: false,
        };
    }
    if value.contains("unauthorized")
        || value.contains("authentication required")
        || value.contains("status 401")
    {
        return FailureClassification {
            code: "authentication_required",
            retryable: false,
        };
    }
    FailureClassification {
        code: "temporary_operation_failure",
        retryable: true,
    }
}

pub fn profile_is_due(profile: &BackupProfile, now: DateTime<Utc>) -> bool {
    if !profile.enabled {
        return false;
    }
    let candidate = if profile.schedule_state == "retrying" {
        profile.retry_at.as_deref()
    } else {
        profile.next_run.as_deref()
    };
    candidate
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc) <= now)
        .unwrap_or(false)
}

/// Return the next retry delay for retry numbers 1..=3. Jitter is stable per
/// profile and attempt, spreads clients over a ±10% window, and requires no
/// additional persisted random state.
pub fn retry_delay(profile_id: &str, retry_number: u32) -> Option<Duration> {
    let base_seconds = match retry_number {
        1 => 5 * 60,
        2 => 30 * 60,
        3 => 2 * 60 * 60,
        _ => return None,
    };
    let mut hasher = DefaultHasher::new();
    profile_id.hash(&mut hasher);
    retry_number.hash(&mut hasher);
    let spread = base_seconds / 5;
    let offset = (hasher.finish() % (spread as u64 + 1)) as i64 - (base_seconds / 10) as i64;
    Some(Duration::seconds(base_seconds as i64 + offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(state: &str, next_run: Option<&str>, retry_at: Option<&str>) -> BackupProfile {
        BackupProfile {
            id: "profile-1".into(),
            owner_account: Some("owner@example.com".into()),
            name: "Test".into(),
            source_path: "C:\\data".into(),
            schedule: Some("daily".into()),
            retention: 7,
            folder: "/".into(),
            enabled: true,
            last_run: None,
            next_run: next_run.map(str::to_string),
            retry_count: 0,
            retry_at: retry_at.map(str::to_string),
            last_error: None,
            last_error_code: None,
            schedule_state: state.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn retries_use_the_retry_deadline_instead_of_the_overdue_regular_slot() {
        let now = DateTime::parse_from_rfc3339("2026-08-19T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let waiting = profile(
            "retrying",
            Some("2026-08-19T10:00:00Z"),
            Some("2026-08-19T12:05:00Z"),
        );
        let due = profile(
            "retrying",
            Some("2026-08-19T10:00:00Z"),
            Some("2026-08-19T11:59:00Z"),
        );
        assert!(!profile_is_due(&waiting, now));
        assert!(profile_is_due(&due, now));
    }

    #[test]
    fn permanent_failures_are_not_retried() {
        assert_eq!(
            classify_schedule_failure("Source path does not exist: C:\\gone").code,
            "source_missing"
        );
        assert!(!classify_schedule_failure("REPOSITORY_KEY_MISMATCH: cannot decrypt").retryable);
        assert!(classify_schedule_failure("request timed out").retryable);
    }

    #[test]
    fn retry_backoff_is_bounded_and_stops_after_three_retries() {
        for (retry, base) in [(1, 300), (2, 1_800), (3, 7_200)] {
            let actual = retry_delay("profile-1", retry).unwrap().num_seconds();
            assert!(actual >= base * 9 / 10 && actual <= base * 11 / 10);
        }
        assert!(retry_delay("profile-1", MAX_SCHEDULE_RETRIES + 1).is_none());
    }
}
