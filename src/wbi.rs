//! Bilibili "WBI" request signing.
//!
//! Signed requests append `wts` and `w_rid` to the query string. The signature
//! is `md5(sorted_query + mixin_key)`, where `mixin_key` is a 32-byte
//! permutation of `img_key ++ sub_key` driven by a fixed 64-entry table.
//!
//! The percent-encoding MUST follow `encodeURIComponent` semantics: the eight
//! characters `- _ . ! ~ * ' ( )` stay literal and a space becomes `%20`
//! (never `+`). `url::form_urlencoded` is deliberately not used here.

use md5::{Digest, Md5};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use std::time::{SystemTime, UNIX_EPOCH};

/// Fixed 64-entry permutation used to derive the mixin key.
const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

/// Alphanumerics are never escaped; these eight punctuation marks are also
/// preserved, matching JavaScript's `encodeURIComponent`.
const ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// Derive the 32-byte mixin key from `img_key ++ sub_key`.
///
/// The table indexes are byte offsets into the (ASCII) concatenation.
pub fn get_mixin_key(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(32);
    for &index in MIXIN_KEY_ENC_TAB.iter().take(32) {
        if let Some(&byte) = bytes.get(index) {
            out.push(byte as char);
        }
    }
    out
}

/// Percent-encode `input` with `encodeURIComponent` semantics.
fn enc(input: &str) -> String {
    utf8_percent_encode(input, ENCODE_SET).to_string()
}

/// Sign `params` and return the full query string (`...&w_rid=<hex>`).
///
/// `wts` (seconds, integer) is added and participates in the sort. The value of
/// each parameter has the characters `!'()*` removed before encoding.
pub fn enc_wbi(params: Vec<(String, String)>, img_key: &str, sub_key: &str) -> String {
    let mixin_key = get_mixin_key(&format!("{img_key}{sub_key}"));

    let mut params = params;
    let wts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    params.push(("wts".to_string(), wts.to_string()));

    // Ascending byte order on the key; `wts` is part of this sort.
    params.sort_by(|a, b| a.0.cmp(&b.0));

    let query = params
        .iter()
        .map(|(key, value)| {
            let filtered: String = value.chars().filter(|c| !"!'()*".contains(*c)).collect();
            format!("{}={}", enc(key), enc(&filtered))
        })
        .collect::<Vec<_>>()
        .join("&");

    // Direct concatenation: query string immediately followed by the key.
    let w_rid = format!("{:x}", Md5::digest(format!("{query}{mixin_key}").as_bytes()));
    format!("{query}&w_rid={w_rid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixin_key_matches_reference_vector() {
        let raw = "7cd084941338484aae1ad9425b84077c4932caff0ff746eab6f01bf08b70ac45";
        assert_eq!(get_mixin_key(raw), "ea1db124af3c7062474693fa704f4ff8");
    }

    #[test]
    fn encoding_preserves_uricomponent_unreserved() {
        // Space -> %20 (not '+'); the eight punctuation marks stay literal.
        assert_eq!(enc("a b"), "a%20b");
        assert_eq!(enc("-_.!~*'()"), "-_.!~*'()");
        assert_eq!(enc("中"), "%E4%B8%AD");
    }
}
