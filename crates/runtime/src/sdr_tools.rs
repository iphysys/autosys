//! The two design tools the model sees: `sdr_read` and `sdr_propose`
//! (ADR-0030). They are not MCP tools: a tool server may not hold SDR access
//! or harness logic (blueprint sections 3.2, 13.2), so the runtime runs them
//! in process, through `SdrManager`.

pub(crate) const READ: &str = "sdr_read";
pub(crate) const PROPOSE: &str = "sdr_propose";

/// Both names; agent specs and tasks may list them beside the tools of
/// `config/tools/`.
pub(crate) const NAMES: [&str; 2] = [READ, PROPOSE];
