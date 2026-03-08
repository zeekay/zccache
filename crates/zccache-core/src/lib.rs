//! Core types and traits for zccache.
//!
//! This crate contains shared types, error definitions, path utilities,
//! and configuration structures used across all zccache crates.

pub mod config;
pub mod error;
pub mod path;

pub use error::{Error, Result};
