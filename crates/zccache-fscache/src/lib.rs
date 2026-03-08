//! File metadata cache for zccache.
//!
//! Provides a fast, concurrent, in-memory cache of filesystem metadata
//! to reduce redundant stat calls during compilation.

pub mod metadata;

pub use metadata::{Confidence, FileId, FileMetadata, MetadataCache};
