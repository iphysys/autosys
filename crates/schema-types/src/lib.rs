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
