//! The QA surface: everything here exists to be driven by a `BILI_*_TEST`
//! variable from a script, never by a user.
//!
//! Nothing here runs unless its variable is set, and every variable is read
//! through [`env`] (one file, so "what can a QA run change?" has one answer).
//!
//! The `qa` feature is part of `default` so the existing toolchain and every
//! `scripts/*.ps1` recipe keep working. `--no-default-features` builds the same
//! tree with the whole surface inert: [`env::var`] returns `None` without the
//! feature, so no hook can fire. The helper bodies stay in the binary because
//! product paths reference a few of them (the capture utilities and the timer
//! keepers); they are unreachable without a variable.
//!
//! This is the **test face, not the product face**: if a behaviour is only
//! reachable from here, a user cannot reach it.

pub mod covers;
pub mod f196;
pub mod f199;
pub mod f200;
pub mod f201;
pub mod f211;
pub mod f212;
pub mod env;
pub mod hooks;
pub mod mf;
pub mod perf;
pub mod pgc;
pub mod sr;

pub(crate) use covers::*;
pub(crate) use f196::*;
pub(crate) use f199::*;
pub(crate) use f200::*;
pub(crate) use f201::*;
pub(crate) use f212::*;
pub(crate) use hooks::*;
pub(crate) use perf::*;
pub(crate) use pgc::*;
pub(crate) use sr::*;