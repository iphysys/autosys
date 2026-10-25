//! Traits and core types of the harness (blueprint section 4.1).
//!
//! Every crate that implements or calls a harness capability depends on this
//! crate. It holds no infrastructure: implementations live in other crates, and
//! `synth-testkit` provides in-memory ones plus a contract-test suite per trait.
//!
//! Traits held as trait objects use `async-trait` (ADR-0015).

pub mod check;
pub mod context;
pub mod error;
pub mod fence;
pub mod gate;
pub mod hooks;
pub mod ids;
pub mod model;
pub mod schema;
pub mod storage;
pub mod tool;

/// Types generated from `schemas/`, re-exported so dependants need not depend
/// on `synth-schema-types` directly.
pub use schema_types as types;

pub use check::{Check, CheckDescriptor, CheckFinding, CheckSubject};
pub use context::{Clock, ExecutionContext, ExecutionContextBuilder, SystemClock};
pub use error::{Error, ErrorCode};
pub use fence::{
    FenceToken, FoundLease, StaleFence, Staleness, StoreWrite, UnfencedReason, WriteAuthority,
    authorize, lease_lost,
};
pub use gate::{Gate, GateOutcome, GateRequest, GateSubject, GateSubjectKind};
pub use hooks::EventSubscriber;
pub use model::{
    ContentBlock, Message, ModelProvider, ModelRequest, ModelResponse, ModelStopReason, Role,
    ToolSpec,
};
pub use schema::SchemaSet;
pub use storage::{
    ArtifactStore, Claimed, EventLog, EvidenceStore, JobStore, NewEvent, NewStep, SdrRepository,
    StepStart, StepStore, WakeReason,
};
pub use tool::{Tool, ToolDescriptor, ToolOutput, ToolRequest};
