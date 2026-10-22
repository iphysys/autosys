//! Fakes for the non-store traits. Each passes the matching suite in
//! [`crate::contract`].

mod check;
mod gate;
mod model;
mod subscriber;
mod tool;

pub use check::FakeCheck;
pub use gate::FakeGate;
pub use model::{ScriptedModelProvider, text_response, tool_call_response};
pub use subscriber::RecordingSubscriber;
pub use tool::FakeTool;
