//! The SDR manager: the only way to change design state (blueprint section 7,
//! ADR-0020).
//!
//! A change moves through typed states. [`SdrManager::propose`] returns a
//! [`ProposedTransition`]; [`SdrManager::validate`] turns it into a
//! [`ValidatedTransition`] or an [`InvalidTransition`]; only a
//! `ValidatedTransition` can be passed to [`SdrManager::commit`] or
//! [`SdrManager::reject`]. The storage behind the manager is crate-private.
//!
//! An unvalidated transition cannot be committed; this does not compile:
//!
//! ```compile_fail,E0308
//! use contracts::ExecutionContext;
//! use sdr::{CommitInfo, ProposedTransition, SdrManager};
//!
//! async fn commit_unvalidated(
//!     manager: &SdrManager,
//!     proposed: ProposedTransition,
//!     info: CommitInfo,
//!     ctx: &ExecutionContext,
//! ) {
//!     let _ = manager.commit(proposed, info, ctx).await;
//! }
//! ```
//!
//! The same code with a validated transition compiles:
//!
//! ```no_run
//! use contracts::ExecutionContext;
//! use sdr::{CommitInfo, ProposedTransition, SdrManager, Validation};
//!
//! async fn commit_validated(
//!     manager: &SdrManager,
//!     proposed: ProposedTransition,
//!     info: CommitInfo,
//!     ctx: &ExecutionContext,
//! ) {
//!     if let Ok(Validation::Valid(validated)) = manager.validate(proposed, ctx).await {
//!         let _ = manager.commit(validated, info, ctx).await;
//!     }
//! }
//! ```

mod apply;
mod manager;
mod migrations;
mod schema;
mod store;
mod transitions;

pub use manager::{CommitInfo, InitialVersion, SdrManager};
pub use transitions::{
    InvalidTransition, Problem, ProblemKind, ProposedTransition, Rejection, ValidatedTransition,
    Validation,
};
