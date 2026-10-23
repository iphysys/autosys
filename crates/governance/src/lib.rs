//! Gates decide; hooks observe (blueprint section 15).
//!
//! - [`ToolPermissionGate`] lets a tool call through only if a `config/tools/`
//!   overlay lists the tool.
//! - [`SdrCommitGate`] lets a transition commit only when every check
//!   `config/policies/gates.yaml` requires has evidence and none failed.
//! - [`ApprovalGate`] asks a human when `config/policies/autonomy.yaml` says
//!   so.
//! - [`GateRunner`] runs the gates for a subject and writes every decision to
//!   the event log; [`ObservedEventLog`] shows every event to the hooks.
//!
//! Policies arrive typed and validated; nothing here reads files.

mod decisions;
mod gates;
mod hooks;

pub use decisions::{Decision, GateRunner};
pub use gates::{ApprovalGate, SdrCommitGate, ToolPermissionGate};
pub use hooks::ObservedEventLog;
