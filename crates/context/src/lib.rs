//! The context manager: what each model call sees, and why (blueprint
//! section 2.3 and 8, ADR-0029).
//!
//! A context is assembled for every call from the task contract, the job's
//! state, the SDR slice and the run's recent events. Nothing accumulates, and
//! nothing is summarised by a model: when a context is too big, the oldest
//! non-authoritative items are dropped and the context says so (the
//! `compaction` module has the order). The design record is never cut; if it does not fit, the
//! build fails with `context_overflow`.
//!
//! - [`ContextBuilder::build`] reads the SDR (through `SdrManager`) and the
//!   event log, assembles, and appends a `context_assembled` event carrying
//!   the context's hash.
//! - [`assemble`] is the pure part: the same inputs give the same context and
//!   the same hash.
//! - Trusted instructions go in the system prompt. Everything else is escaped
//!   and labelled `data` or `untrusted`; tool results are wrapped in an
//!   untrusted envelope ([`untrusted`]).
//! - Every item in the [`Manifest`] carries its provenance.
//! - [`PromptLibrary`] loads the prompts in `prompts/` and records their
//!   versions and hashes.
//! - Sizes are estimates behind [`TokenCounter`]; [`ContextProfiles`] holds
//!   the budgets from `config/context.yaml` within the ceiling of
//!   `config/policies/context.yaml`.

mod compaction;
mod error;
mod hash;
mod prompts;
mod render;
mod selection;
mod token_budget;

mod builder;

pub use builder::{
    AssembledContext, AssemblyInputs, BuildRequest, ContextBuilder, Manifest, SectionManifest,
    Snapshot, UsageComparison, assemble,
};
pub use error::ContextError;
pub use prompts::{Prompt, PromptLibrary};
pub use render::{Omission, Trust, untrusted};
pub use token_budget::{ByteEstimate, ContextProfiles, DEFAULT_PROFILE, TokenCounter};
