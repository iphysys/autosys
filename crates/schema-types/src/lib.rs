//! Rust types generated from the canonical JSON Schemas in `schemas/`.
//!
//! `build.rs` generates every type at build time; nothing here is hand-written.
//! Change a type by changing its schema.

#[allow(
    clippy::unwrap_used,
    reason = "typify compiles each schema `pattern` into a regex with `unwrap`; \
              build.rs fails if a pattern is invalid, so the unwrap cannot panic"
)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/schema_types.rs"));
}

pub use generated::*;

/// Every schema file under `schemas/`, as (path relative to `schemas/`, file
/// text), embedded at build time. Lets the harness validate values against the
/// same schemas these types were generated from.
pub const SCHEMA_FILES: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/schema_files.rs"));
