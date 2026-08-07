//! Unicode sanitization for user-supplied session tags.
//!
//! Ported from `_sanitize_unicode` in upstream
//! `_internal/session_mutations.py`. Iteratively NFKC-normalizes and strips
//! format/private-use/unassigned characters (plus an explicit range list
//! covering the most commonly abused injection characters) until a fixpoint,
//! matching the CLI's tag-filter compatibility requirements.

use std::sync::LazyLock;

use regex::Regex;
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;

const MAX_ITERATIONS: usize = 10;

// Explicit ranges for dangerous Unicode characters, matching the TS
// fallback paths: zero-width spaces/LTR/RTL marks, directional formatting
// characters, directional isolates, byte order mark, and BMP private use.
static UNICODE_STRIP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("[\u{200b}-\u{200f}\u{202a}-\u{202e}\u{2066}-\u{2069}\u{feff}\u{e000}-\u{f8ff}]").unwrap()
});

fn is_format_private_use_or_unassigned(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::Format | GeneralCategory::PrivateUse | GeneralCategory::Unassigned
    )
}

/// Sanitize a string by removing dangerous Unicode characters.
pub(super) fn sanitize_unicode(value: &str) -> String {
    let mut current = value.to_string();
    for _ in 0..MAX_ITERATIONS {
        let previous = current.clone();
        let normalized: String = current.nfkc().collect();
        let stripped: String = normalized.chars().filter(|c| !is_format_private_use_or_unassigned(*c)).collect();
        current = UNICODE_STRIP_RE.replace_all(&stripped, "").into_owned();
        if current == previous {
            break;
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_zero_width_and_directional_marks() {
        let dirty = "hello\u{200b}world\u{202a}";
        assert_eq!(sanitize_unicode(dirty), "helloworld");
    }

    #[test]
    fn leaves_plain_ascii_untouched() {
        assert_eq!(sanitize_unicode("experiment"), "experiment");
    }

    #[test]
    fn strips_bom() {
        assert_eq!(sanitize_unicode("\u{feff}tag"), "tag");
    }
}
