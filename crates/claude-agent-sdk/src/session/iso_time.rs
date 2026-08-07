//! Shared timestamp formatting for mutation-appended entries.
//!
//! Ported from `_iso_now` in upstream `_internal/session_mutations.py`.

/// Current UTC time as an ISO-8601 string with a `Z` suffix (millisecond
/// precision), matching upstream's
/// `datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")`.
pub(super) fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
