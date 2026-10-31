//! In-process video decode that never leaves the GPU.
//!
//! The player decodes today with an ffmpeg pipe into a CPU frame and uploads
//! 8.3 MB per 1080p BGRA picture (`stage_us` 5-11 ms) before anything can be
//! drawn. This module is the replacement transport: Media Foundation (shipped
//! with Windows, so nothing to bundle) decodes the B 站 DASH stream with the
//! hardware decoder and hands back the picture as an `ID3D11Texture2D` in NV12.
//! The sink can then sample that texture directly, so the per-frame CPU→GPU
//! upload becomes zero bytes.
//!
//! # Scope of f208
//!
//! Only the decoder itself. Nothing calls it from the player yet - wiring it
//! into the sink is f209 - so the module is marked `allow(dead_code)` and its
//! acceptance is the standalone probe (`examples/mf_probe.rs`, driven by
//! `scripts/qa-f208-mf.ps1`) plus the unit tests in `src/qa/mf.rs`.
//!
//! # Failure policy
//!
//! A missing codec (HEVC without the HEVC Video Extensions, for instance) or a
//! machine that refuses the D3D11 texture path returns `Err(String)` with the
//! reason. The caller decides whether to fall back to the existing ffmpeg
//! path; this module never degrades silently to CPU pixels - a CPU buffer from
//! the source reader is an explicit error.

#![allow(dead_code)]

pub(crate) mod mf;
