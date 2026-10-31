//! Single-account persistence: an `accounts.json` beside the legacy
//! `token.json`.
//!
//! The app holds exactly one logged-in account. This module stores that one
//! [`Account`] as a bare JSON object (no list and no active index), migrates
//! older multi-account files by keeping only the active/last entry, and clears
//! the record on logout.
//!
//! `token.json` is only read here — migration imports it once.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::bilibili::{self, StoredCookie};

/// The persisted account: its identity, its cookies and its login token.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    pub mid: i64,
    pub uname: String,
    pub face: String,
    pub cookies: Vec<StoredCookie>,
    pub refresh_token: String,
    pub added_at: u64,
    /// 大会员 status captured at the last `nav` for this account.
    pub vip: bool,
}

/// Old on-disk shape from the retired multi-account wave. Deserialised only so
/// [`load`] can migrate it down to a single [`Account`].
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyAccounts {
    active_mid: i64,
    accounts: Vec<Account>,
}

/// `accounts.json` lives in the project root regardless of the current dir.
pub fn accounts_path() -> PathBuf {
    crate::paths::root().join("accounts.json")
}

/// Read the saved account. `None` when the file is missing or unparsable; a
/// legacy multi-account file is collapsed to its active entry and rewritten in
/// the single-account shape.
pub fn load() -> Option<Account> {
    let data = std::fs::read_to_string(accounts_path()).ok()?;
    let (account, migrated) = parse_saved(&data)?;
    if migrated {
        let _ = save(&account);
    }
    Some(account)
}

/// Persist the single account, pretty-printed.
pub fn save(account: &Account) -> Result<()> {
    let data = serde_json::to_string_pretty(account)?;
    std::fs::write(accounts_path(), data)?;
    Ok(())
}

/// Delete the saved record (and the legacy token) so the next start shows the
/// login page.
pub fn clear() {
    let _ = std::fs::remove_file(accounts_path());
    let _ = std::fs::remove_file(bilibili::token_path());
}

/// Turn the account back into a login token so a session can be rebuilt.
pub fn to_token(account: &Account) -> bilibili::Token {
    bilibili::Token {
        cookies: account.cookies.clone(),
        refresh_token: account.refresh_token.clone(),
    }
}

/// Parse either the current single-account object or the retired
/// `{ active_mid, accounts: [...] }` shape. The bool is true when the input was
/// the legacy shape and needs rewriting.
fn parse_saved(data: &str) -> Option<(Account, bool)> {
    if let Ok(account) = serde_json::from_str::<Account>(data) {
        if account.mid != 0 || !account.cookies.is_empty() {
            return Some((account, false));
        }
    }
    let legacy = serde_json::from_str::<LegacyAccounts>(data).ok()?;
    let account = legacy
        .accounts
        .iter()
        .find(|account| account.mid == legacy.active_mid)
        .or_else(|| legacy.accounts.last())
        .cloned()?;
    Some((account, true))
}

/// First-run migration: when `accounts.json` is absent but the legacy
/// `token.json` is present, log in with it, read `nav`, and write a
/// single-account `accounts.json`. Returns the freshly written account so the
/// caller can build a single session from it.
pub async fn migrate_from_token() -> Option<Account> {
    if accounts_path().exists() {
        return None;
    }
    let token = bilibili::load_token()?;
    let session = bilibili::Session::from_token(&token).await.ok()?;
    let user = match session.nav().await.ok()? {
        bilibili::NavOutcome::LoggedIn(user) => user,
        bilibili::NavOutcome::NotLoggedIn { .. } => return None,
    };
    let account = Account {
        mid: user.mid,
        uname: user.uname,
        face: user.face,
        cookies: token.cookies,
        refresh_token: token.refresh_token,
        added_at: now_secs(),
        vip: user.vip_status,
    };
    let _ = save(&account);
    Some(account)
}

/// Seconds since the Unix epoch (0 on a pre-epoch clock).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_json() -> String {
        r#"{
            "active_mid": 2,
            "accounts": [
                {"mid": 1, "uname": "one", "cookies": [{"name": "SESSDATA", "value": "a"}]},
                {"mid": 2, "uname": "two", "cookies": [{"name": "SESSDATA", "value": "b"}]}
            ]
        }"#
        .to_string()
    }

    #[test]
    fn legacy_multi_account_keeps_active_entry() {
        let (account, migrated) = match parse_saved(&legacy_json()) {
            Some(pair) => pair,
            None => return assert!(false, "legacy file did not parse"),
        };
        assert!(migrated);
        assert_eq!(account.mid, 2);
        assert_eq!(account.uname, "two");
    }

    #[test]
    fn legacy_multi_account_falls_back_to_last_entry() {
        let json = legacy_json().replace("\"active_mid\": 2", "\"active_mid\": 99");
        let (account, migrated) = match parse_saved(&json) {
            Some(pair) => pair,
            None => return assert!(false, "legacy file did not parse"),
        };
        assert!(migrated);
        assert_eq!(account.mid, 2);
    }

    #[test]
    fn single_account_round_trip_has_no_list() {
        let account = Account {
            mid: 7,
            uname: "seven".to_string(),
            cookies: vec![StoredCookie {
                name: "SESSDATA".to_string(),
                value: "x".to_string(),
            }],
            refresh_token: "r".to_string(),
            added_at: 1,
            vip: true,
            ..Account::default()
        };
        let encoded = match serde_json::to_string_pretty(&account) {
            Ok(text) => text,
            Err(_) => return assert!(false, "account did not serialize"),
        };
        assert!(!encoded.contains("\"accounts\""));
        let (decoded, migrated) = match parse_saved(&encoded) {
            Some(pair) => pair,
            None => return assert!(false, "single account did not parse"),
        };
        assert!(!migrated);
        assert_eq!(decoded.mid, 7);
        assert_eq!(decoded.uname, "seven");
    }
}
