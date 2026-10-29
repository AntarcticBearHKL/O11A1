//! Every environment variable the QA surface reads, in one place.
//!
//! **This is the test face, not the product face.** Nothing here changes how the
//! app behaves for a user: each read is a `BILI_*_TEST`-style switch that a
//! script (or a hand run) sets to drive a scripted walk. A shipped build that
//! sets none of them behaves exactly as if this module did not exist.
//!
//! Keeping the reads here is what makes it possible to answer "what can a QA
//! run change?" by reading one file, instead of grepping 40k lines of UI code
//! for `std::env::var`.

/// The value of a QA variable, trimmed of the trailing newline a `set` from a
/// script tends to leave. `None` when unset (or set to an empty string).
pub(crate) fn var(name: &str) -> Option<String> {
    // Without the feature every switch reads as "not set": the QA surface is
    // compiled out, so nothing can drive it.
    if !cfg!(feature = "qa") {
        return None;
    }
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `BILI_X=1` / `true` / `yes` (case-insensitive).
pub(crate) fn flag(name: &str) -> bool {
    matches!(
        var(name).map(|value| value.to_ascii_lowercase()),
        Some(value) if value == "1" || value == "true" || value == "yes"
    )
}
