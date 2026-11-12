//! Bilibili web endpoints used for QR-code login and account info.
//!
//! One [`Session`] wraps a single `reqwest::Client` backed by an explicit
//! shared cookie [`Jar`], a desktop Chrome `User-Agent` and a
//! `Referer: https://www.bilibili.com/` default header.


use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{
    HeaderMap, HeaderValue, CONTENT_ENCODING, CONTENT_TYPE, RANGE, REFERER, SET_COOKIE, USER_AGENT,
};
use reqwest::Url;

mod account;
mod feed;
mod live;
mod pgc;
mod search;
mod wbi;

pub use account::*;
pub use feed::*;
pub use live::*;
pub use pgc::*;
pub use search::*;
pub use wbi::*;
