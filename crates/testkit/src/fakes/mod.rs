//! Fakes for the non-store traits. Each passes the matching suite in
//! [`crate::contract`], except [`FaultySdrRepository`], which fails on purpose.

mod check;
mod claude_server;
mod gate;
mod keyed_effect;
mod model;
mod recorded;
mod runs;
mod sdr;
mod subscriber;
mod tool;

pub use check::FakeCheck;
pub use claude_server::{FakeClaudeServer, Responder, message};
pub use gate::FakeGate;
pub use keyed_effect::{KeyedEffects, go_on};
pub use model::{ScriptedModelProvider, text_response, tool_call_response};
pub use recorded::RecordedModelProvider;
pub use runs::{FakeReports, FakeSubmitter};
pub use sdr::{Fault, FaultySdrRepository, faulty_sdr_storage, faulty_sdr_storage_after_head};
pub use subscriber::RecordingSubscriber;
pub use tool::{FakeTool, HookFuture, Slot};
