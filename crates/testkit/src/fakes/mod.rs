//! Fakes for the non-store traits. Each passes the matching suite in
//! [`crate::contract`], except [`FaultySdrRepository`], which fails on purpose.

mod check;
mod gate;
mod keyed_effect;
mod model;
mod pliny_server;
mod recorded;
mod runs;
mod sdr;
mod subscriber;
mod tool;

pub use check::FakeCheck;
pub use gate::FakeGate;
pub use keyed_effect::{KeyedEffects, go_on};
pub use model::{ScriptedModelProvider, text_response, tool_call_response};
pub use pliny_server::{FAKE_PLINY_MODEL, FakePlinyServer, Responder, completion, tool_call};
pub use recorded::RecordedModelProvider;
pub use runs::{FakeReports, FakeSdrViews, FakeSubmitter};
pub use sdr::{Fault, FaultySdrRepository, faulty_sdr_storage, faulty_sdr_storage_after_head};
pub use subscriber::RecordingSubscriber;
pub use tool::{FakeTool, HookFuture, Slot};
