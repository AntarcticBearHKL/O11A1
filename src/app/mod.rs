//! The application, split by page/host so `main.rs` is only startup.
//!
//! Every module here is a *move* out of the old single-file UI layer: the code
//! is unchanged, the visibility is `pub(crate)`, and each module starts with
//! `use crate::*;` so the shared plumbing resolves exactly as it used to.

pub(crate) mod core;
pub(crate) mod account;
pub(crate) mod feed;
pub(crate) mod search;
pub(crate) mod playlist;
pub(crate) mod player_host;
pub(crate) mod live;
pub(crate) mod pgc;
pub(crate) mod fish_wiring;
pub(crate) mod models_ui;
pub(crate) mod settings_publish;
pub(crate) mod window;
pub(crate) use core::*;
pub(crate) use account::*;
pub(crate) use feed::*;
pub(crate) use search::*;
pub(crate) use playlist::*;
pub(crate) use player_host::*;
pub(crate) use live::*;
pub(crate) use pgc::*;
pub(crate) use fish_wiring::*;
pub(crate) use models_ui::*;
pub(crate) use settings_publish::*;
pub(crate) use window::*;
