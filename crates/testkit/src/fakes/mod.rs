//! Fakes for the non-store traits. Each passes the matching suite in
//! [`crate::contract`], except [`FaultySdrRepository`], which fails on purpose.

mod check;
mod gate;
mod model;
mod recorded;
mod sdr;
mod subscriber;
mod tool;

pub use check::FakeCheck;
pub use gate::FakeGate;
pub use model::{ScriptedModelProvider, text_response, tool_call_response};
pub use recorded::RecordedModelProvider;
pub use sdr::{Fault, FaultySdrRepository, faulty_sdr_storage, faulty_sdr_storage_after_head};
pub use subscriber::RecordingSubscriber;
pub use tool::FakeTool;
