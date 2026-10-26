//! The event view of `run watch` (ADR-0036): one line per event, bursts of
//! checks grouped into one line, and the `synth run approve` commands when a
//! run asks for approval. `Renderer` is a pure state machine; printing is
//! the caller's.

use std::io::Write;

use api_client::types::{
    Actor, Event, EventPayload, GateDecision, StopReason, ToolCallOutcome, TransitionOutcome,
    Verdict,
};

use super::{MAX_WIDTH, Tone, Zone, sanitize, state_name};

/// One line of the view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Line {
    /// `HH:MM:SS`, or empty for a line that stands alone.
    pub(crate) time: String,
    pub(crate) kind: &'static str,
    pub(crate) tone: Tone,
    /// Sanitized.
    pub(crate) text: String,
}

impl Line {
    fn new(time: &str, kind: &'static str, tone: Tone, text: &str) -> Self {
        Self {
            time: time.to_owned(),
            kind,
            tone,
            text: sanitize(text, MAX_WIDTH),
        }
    }

    /// A line under the one before, with no kind of its own.
    fn more(time: &str, tone: Tone, text: &str) -> Self {
        Self::new(time, "", tone, &format!("  {text}"))
    }

    /// A line with no time and no kind.
    fn bare(tone: Tone, text: &str) -> Self {
        Self::new("", "", tone, text)
    }

    /// The line without colour, as `write` prints it to a stream that
    /// strips styles.
    #[cfg(test)]
    pub(crate) fn plain(&self) -> String {
        if self.time.is_empty() {
            return self.text.clone();
        }
        format!("{}  {:<9}{}", self.time, self.kind, self.text)
    }

    /// The line with colour; the stream decides whether it shows.
    pub(crate) fn write(&self, out: &mut impl Write) -> std::io::Result<()> {
        let style = self.tone.style();
        if self.time.is_empty() {
            return writeln!(out, "{style}{}{style:#}", self.text);
        }
        let muted = Tone::Muted.style();
        write!(out, "{muted}{}{muted:#}  ", self.time)?;
        if self.kind.is_empty() {
            writeln!(out, "{:<9}{style}{}{style:#}", "", self.text)
        } else {
            writeln!(out, "{style}{:<9}{style:#}{}", self.kind, self.text)
        }
    }
}

struct Check {
    time: String,
    id: String,
    verdict: Verdict,
    evidence: String,
}

/// Turns events into lines, in order.
pub(crate) struct Renderer {
    zone: Zone,
    /// Checks arrive in bursts of a dozen or more; they are printed as one
    /// line when anything else arrives.
    checks: Vec<Check>,
    /// The last gate's reasons, which a rejected transition repeats.
    gate_reasons: Vec<String>,
}

impl Renderer {
    pub(crate) fn new(zone: Zone) -> Self {
        Self {
            zone,
            checks: Vec::new(),
            gate_reasons: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, event: &Event) -> Vec<Line> {
        let time = self.zone.format(&event.occurred_at, "%H:%M:%S");
        let mut lines = Vec::new();
        if let EventPayload::CheckCompleted {
            check_id,
            evidence_id,
            verdict,
        } = &event.payload
        {
            self.checks.push(Check {
                time,
                id: check_id.as_str().to_owned(),
                verdict: *verdict,
                evidence: evidence_id.as_str().to_owned(),
            });
            return lines;
        }
        self.flush_checks(&mut lines);
        let t = time.as_str();
        match &event.payload {
            EventPayload::JobStateChanged {
                from, reason, to, ..
            } => {
                let mut text = match from {
                    Some(from) => format!("{from} -> {}", state_name(to)),
                    None => state_name(to).to_owned(),
                };
                if let Some(reason) = reason {
                    text.push_str(&format!("  ({})", reason.as_str()));
                }
                lines.push(Line::new(t, "job", Tone::Job, &text));
            }
            EventPayload::RunStarted { agent_id, .. } => {
                lines.push(Line::new(
                    t,
                    "run",
                    Tone::Info,
                    &format!("started, agent {}", agent_id.as_str()),
                ));
            }
            EventPayload::ContextAssembled {
                total_tokens,
                sdr_version,
                ..
            } => lines.push(Line::new(
                t,
                "context",
                Tone::Muted,
                &format!(
                    "assembled for the next model call ({total_tokens} tokens, design at version {sdr_version})"
                ),
            )),
            EventPayload::ModelCallCompleted {
                model_id, usage, ..
            } => lines.push(Line::new(
                t,
                "model",
                Tone::Model,
                &format!(
                    "{} answered  ({} tokens in, {} out)",
                    model_id.as_str(),
                    usage.input_tokens,
                    usage.output_tokens
                ),
            )),
            EventPayload::ToolCallStarted { step_id, tool, .. } => lines.push(Line::new(
                t,
                "tool",
                Tone::Tool,
                &format!("-> {}  (step {})", tool.as_str(), step_id.as_str()),
            )),
            EventPayload::ToolCallCompleted {
                duration_ms,
                error,
                outcome,
                recovered,
                tool,
                ..
            } => {
                let mut text = format!("<- {} {outcome} in {duration_ms} ms", tool.as_str());
                if let Some(error) = error {
                    text.push_str(&format!(
                        "  {}: {}",
                        error.code.as_str(),
                        error.message.as_str()
                    ));
                }
                if *recovered == Some(true) {
                    text.push_str("  (recovered after a crash, not called again)");
                }
                let tone = match outcome {
                    ToolCallOutcome::Ok => Tone::Tool,
                    ToolCallOutcome::Error => Tone::Bad,
                };
                lines.push(Line::new(t, "tool", tone, &text));
            }
            EventPayload::GateDecided {
                decision,
                gate,
                reasons,
                ..
            } => {
                let tone = match decision {
                    GateDecision::Allow => Tone::Good,
                    GateDecision::Ask => Tone::Warn,
                    GateDecision::Deny | GateDecision::Retry | GateDecision::RejectTransition => {
                        Tone::Bad
                    }
                };
                lines.push(Line::new(
                    t,
                    "gate",
                    tone,
                    &format!("{}: {decision}", gate.as_str()),
                ));
                for reason in reasons {
                    lines.push(Line::more(t, tone, reason));
                    self.gate_reasons.push(reason.as_str().to_owned());
                }
            }
            EventPayload::SdrTransition {
                base_version,
                new_version,
                outcome,
                reasons,
                ..
            } => {
                match outcome {
                    TransitionOutcome::Committed => {
                        let new = new_version
                            .as_ref()
                            .map_or_else(|| "?".to_owned(), ToString::to_string);
                        lines.push(Line::new(
                            t,
                            "design",
                            Tone::Good,
                            &format!("committed: version {base_version} -> {new}"),
                        ));
                    }
                    TransitionOutcome::Rejected => {
                        lines.push(Line::new(
                            t,
                            "design",
                            Tone::Bad,
                            &format!(
                                "proposal rejected (based on version {base_version}); the agent will repair it"
                            ),
                        ));
                        // A gate's reasons come back here prefixed with the
                        // gate's name; show only the ones not printed above.
                        for reason in reasons {
                            let shown = self.gate_reasons.iter().any(|g| reason.ends_with(g.as_str()));
                            if !shown {
                                lines.push(Line::more(t, Tone::Bad, reason));
                            }
                        }
                    }
                }
                self.gate_reasons.clear();
            }
            EventPayload::ApprovalRequested {
                approval_id,
                summary,
                ..
            } => {
                let (run, approval) = (event.run_id.as_str(), approval_id.as_str());
                lines.push(Line::new(t, "APPROVAL", Tone::Warn, summary));
                lines.push(Line::bare(Tone::Plain, ""));
                lines.push(Line::bare(
                    Tone::Warn,
                    "  The run waits for you. In another window, with SYNTH_API_TOKEN set:",
                ));
                lines.push(Line::bare(
                    Tone::Command,
                    &format!("    synth run approve {run} {approval} --answer approve"),
                ));
                lines.push(Line::bare(
                    Tone::Command,
                    &format!(
                        "    synth run approve {run} {approval} --answer reject --comment \"<why>\""
                    ),
                ));
                lines.push(Line::bare(Tone::Plain, ""));
            }
            EventPayload::ApprovalAnswered {
                answer,
                approval_id,
                comment,
            } => {
                let mut text = format!(
                    "{answer} {} by {}",
                    approval_id.as_str(),
                    actor_name(&event.actor)
                );
                if let Some(comment) = comment {
                    text.push_str(&format!(": {}", comment.as_str()));
                }
                lines.push(Line::new(t, "approval", Tone::Info, &text));
            }
            EventPayload::ArtifactPublished {
                artifact_id,
                format,
                ..
            } => lines.push(Line::new(
                t,
                "artifact",
                Tone::Info,
                &format!("{} ({})", artifact_id.as_str(), format.as_str()),
            )),
            EventPayload::CancelRequested { reason } => {
                let text = match reason {
                    Some(reason) => format!("requested: {}", reason.as_str()),
                    None => "requested".to_owned(),
                };
                lines.push(Line::new(t, "cancel", Tone::Bad, &text));
            }
            EventPayload::Error { error } => lines.push(Line::new(
                t,
                "ERROR",
                Tone::Bad,
                &format!("{}: {}", error.code.as_str(), error.message.as_str()),
            )),
            EventPayload::RunStopped {
                sdr_version,
                stop_reason,
            } => {
                let design = sdr_version
                    .as_ref()
                    .map(|v| format!("  (design at version {v})"))
                    .unwrap_or_default();
                let line = match stop_reason {
                    StopReason::ApprovalRequired => Line::new(
                        t,
                        "PAUSED",
                        Tone::Warn,
                        &format!("waiting for your approval{design}"),
                    ),
                    StopReason::Success => {
                        Line::new(t, "STOPPED", Tone::Good, &format!("{stop_reason}{design}"))
                    }
                    StopReason::Failure | StopReason::BudgetExhausted | StopReason::Cancelled => {
                        Line::new(t, "STOPPED", Tone::Bad, &format!("{stop_reason}{design}"))
                    }
                };
                lines.push(line);
            }
            EventPayload::CheckCompleted { .. } => {}
        }
        lines
    }

    /// The lines still held back, at the end of the stream.
    pub(crate) fn finish(&mut self) -> Vec<Line> {
        let mut lines = Vec::new();
        self.flush_checks(&mut lines);
        lines
    }

    fn flush_checks(&mut self, lines: &mut Vec<Line>) {
        let checks = std::mem::take(&mut self.checks);
        let Some(first) = checks.first() else {
            return;
        };
        let t = first.time.as_str();
        let mut by_check: Vec<(&str, usize)> = Vec::new();
        for check in &checks {
            match by_check.iter_mut().find(|(id, _)| *id == check.id) {
                Some((_, count)) => *count += 1,
                None => by_check.push((&check.id, 1)),
            }
        }
        let by_check = by_check
            .iter()
            .map(|(id, count)| format!("{id} x{count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let not_passed: Vec<&Check> = checks
            .iter()
            .filter(|c| c.verdict != Verdict::Pass)
            .collect();
        if not_passed.is_empty() {
            lines.push(Line::new(
                t,
                "checks",
                Tone::Good,
                &format!("{} passed ({by_check})", checks.len()),
            ));
            return;
        }
        lines.push(Line::new(
            t,
            "checks",
            Tone::Bad,
            &format!(
                "{} of {} did not pass ({by_check})",
                not_passed.len(),
                checks.len()
            ),
        ));
        for check in not_passed {
            lines.push(Line::more(
                t,
                Tone::Bad,
                &format!(
                    "{}: {}  evidence {}",
                    check.id, check.verdict, check.evidence
                ),
            ));
        }
    }
}

fn actor_name(actor: &Actor) -> &str {
    match actor {
        Actor::Agent { agent_id } => agent_id.as_str(),
        Actor::User { user_id } => user_id.as_str(),
        Actor::System { component } => component.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;
    use serde_json::{Value, json};

    use super::*;

    macro_rules! example {
        ($name:literal) => {
            include_str!(concat!("../../../../schemas/events/examples/", $name))
        };
    }

    fn utc() -> Zone {
        Zone::Fixed(FixedOffset::east_opt(0).unwrap())
    }

    fn event(json: &str) -> Event {
        serde_json::from_str(json).unwrap()
    }

    /// An example event with its payload's fields replaced.
    fn edited(json: &str, payload: Value) -> Event {
        let mut value: Value = serde_json::from_str(json).unwrap();
        for (key, field) in payload.as_object().unwrap() {
            value["payload"][key] = field.clone();
        }
        serde_json::from_value(value).unwrap()
    }

    fn render(events: &[Event]) -> String {
        let mut renderer = Renderer::new(utc());
        let mut lines: Vec<Line> = events.iter().flat_map(|e| renderer.push(e)).collect();
        lines.extend(renderer.finish());
        lines.iter().map(Line::plain).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn every_example_event_renders_as_its_golden_text() {
        let goldens: &[(&str, &str)] = &[
            (
                example!("event.approval_answered.json"),
                "09:11:02  approval approve appr-0001 by dev: Looks right.",
            ),
            (
                example!("event.approval_requested.json"),
                "09:04:14  APPROVAL Commit the final design (SDR version 2 from base 1)?\n\
                 \n  The run waits for you. In another window, with SYNTH_API_TOKEN set:\n    \
                 synth run approve run-0001 appr-0001 --answer approve\n    \
                 synth run approve run-0001 appr-0001 --answer reject --comment \"<why>\"\n",
            ),
            (
                example!("event.approval_requested.rules.json"),
                "09:04:14  APPROVAL approve_final_design: Ask a human before committing the design the run intends to stop on.\n\
                 \n  The run waits for you. In another window, with SYNTH_API_TOKEN set:\n    \
                 synth run approve run-0001 appr-0001 --answer approve\n    \
                 synth run approve run-0001 appr-0001 --answer reject --comment \"<why>\"\n",
            ),
            (
                example!("event.artifact_published.json"),
                "09:12:30  artifact art-part-step-1 (step)",
            ),
            (
                example!("event.cancel_requested.json"),
                "10:30:00  cancel   requested: Wrong requirements loaded.",
            ),
            (
                example!("event.check_completed.json"),
                "09:04:13  checks   1 passed (requirement.numeric_threshold x1)",
            ),
            (
                example!("event.context_assembled.json"),
                "09:00:02  context  assembled for the next model call (6120 tokens, design at version 2)",
            ),
            (
                example!("event.error.json"),
                "11:00:05  ERROR    model.unavailable: The model provider returned 503 after 3 attempts.",
            ),
            (
                example!("event.gate_decided.json"),
                "09:04:13  gate     sdr_commit: ask\n\
                 09:04:13             Transition is marked final; approve_final_design requires approval.",
            ),
            (
                example!("event.job_state_changed.json"),
                "09:00:01  job      queued -> running  (Claimed by worker-a.)",
            ),
            (
                example!("event.job_state_changed.suspended.json"),
                "09:04:15  job      running -> suspended  (The final commit needs approval.)",
            ),
            (
                example!("event.model_call_completed.json"),
                "09:00:09  model    example-model-1 answered  (6120 tokens in, 410 out)",
            ),
            (
                example!("event.run_started.json"),
                "09:00:00  run      started, agent main_orchestrator",
            ),
            (
                example!("event.run_stopped.json"),
                "09:12:40  STOPPED  success  (design at version 2)",
            ),
            (
                example!("event.sdr_transition.json"),
                "09:11:03  design   committed: version 1 -> 2",
            ),
            (
                example!("event.sdr_transition.recovered.json"),
                "09:11:03  design   committed: version 1 -> 2",
            ),
            (
                example!("event.sdr_transition.rejected.json"),
                "09:05:40  design   proposal rejected (based on version 1); the agent will repair it\n\
                 09:05:40             Base version 1 is stale; current version is 2.",
            ),
            (
                example!("event.tool_call_completed.json"),
                "09:00:14  tool     <- measure_part ok in 3870 ms",
            ),
            (
                example!("event.tool_call_completed.recovered.json"),
                "09:00:14  tool     <- measure_part ok in 3870 ms  (recovered after a crash, not called again)",
            ),
            (
                example!("event.tool_call_completed.timeout.json"),
                "09:02:14  tool     <- create_part error in 120000 ms  tool.timeout: create_part did not finish within 120 s.",
            ),
            (
                example!("event.tool_call_started.json"),
                "09:00:10  tool     -> measure_part  (step step-0007)",
            ),
        ];
        for (json, golden) in goldens {
            assert_eq!(render(&[event(json)]), *golden, "{json}");
        }
    }

    #[test]
    fn every_example_file_has_a_golden() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../schemas/events/examples");
        let count = std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "json"))
            })
            .count();
        assert_eq!(count, 21, "a new example in {dir} needs a golden above");
    }

    #[test]
    fn a_burst_of_checks_is_one_line() {
        let check = example!("event.check_completed.json");
        let pass = |id: &str| edited(check, json!({"check_id": id, "verdict": "pass"}));
        let stopped = event(example!("event.run_stopped.json"));
        assert_eq!(
            render(&[pass("a.one"), pass("a.one"), pass("b.two"), stopped.clone()]),
            "09:04:13  checks   3 passed (a.one x2, b.two x1)\n\
             09:12:40  STOPPED  success  (design at version 2)"
        );
        let failed = edited(
            check,
            json!({"check_id": "b.two", "verdict": "fail", "evidence_id": "ev-b-2"}),
        );
        assert_eq!(
            render(&[pass("a.one"), failed, pass("a.one")]),
            "09:04:13  checks   1 of 3 did not pass (a.one x2, b.two x1)\n\
             09:04:13             b.two: fail  evidence ev-b-2"
        );
    }

    #[test]
    fn a_rejected_transition_does_not_repeat_its_gates_reasons() {
        let gate = edited(
            example!("event.gate_decided.json"),
            json!({"decision": "reject_transition", "reasons": ["Base version 1 is stale."]}),
        );
        let rejected = edited(
            example!("event.sdr_transition.rejected.json"),
            json!({"reasons": ["sdr_commit: Base version 1 is stale.", "The mass check failed."]}),
        );
        assert_eq!(
            render(&[gate.clone(), rejected.clone()]),
            "09:04:13  gate     sdr_commit: reject_transition\n\
             09:04:13             Base version 1 is stale.\n\
             09:05:40  design   proposal rejected (based on version 1); the agent will repair it\n\
             09:05:40             The mass check failed."
        );
        // The remembered reasons are cleared by the transition.
        assert_eq!(
            render(&[gate, rejected.clone(), rejected])
                .lines()
                .filter(|l| l.ends_with("Base version 1 is stale."))
                .count(),
            2
        );
    }

    #[test]
    fn the_approval_hint_names_the_run_and_the_approval() {
        let mut value: Value =
            serde_json::from_str(example!("event.approval_requested.json")).unwrap();
        value["run_id"] = json!("run-7f3a");
        value["payload"]["approval_id"] = json!("appr-9c1d");
        let text = render(&[serde_json::from_value(value).unwrap()]);
        assert!(text.contains("    synth run approve run-7f3a appr-9c1d --answer approve\n"));
        assert!(text.contains(
            "    synth run approve run-7f3a appr-9c1d --answer reject --comment \"<why>\""
        ));
    }

    #[test]
    fn control_characters_in_a_tool_error_are_removed() {
        let timeout = edited(
            example!("event.tool_call_completed.timeout.json"),
            json!({"error": {
                "code": "tool.failed",
                "message": "bad\u{1b}[2J\u{1b}]0;owned\u{7}input\nsecond line",
                "retryable": false
            }}),
        );
        assert_eq!(
            render(&[timeout]),
            "09:02:14  tool     <- create_part error in 120000 ms  tool.failed: bad[2J]0;ownedinput second line"
        );
    }

    #[test]
    fn times_are_shown_in_the_given_zone() {
        let plus_two = Zone::Fixed(FixedOffset::east_opt(2 * 3600).unwrap());
        let mut renderer = Renderer::new(plus_two);
        let lines = renderer.push(&event(example!("event.run_started.json")));
        assert_eq!(lines.first().map(|l| l.time.as_str()), Some("11:00:00"));
    }

    #[test]
    fn colour_never_reaches_the_text_and_the_stream_decides_it() {
        let line = Line::new("09:00:00", "tool", Tone::Bad, "x");
        let mut styled = Vec::new();
        line.write(&mut styled).unwrap();
        let styled = String::from_utf8(styled).unwrap();
        assert!(styled.contains('\u{1b}'));
        let mut stripped = anstream::StripStream::new(Vec::new());
        line.write(&mut stripped).unwrap();
        assert_eq!(
            String::from_utf8(stripped.into_inner()).unwrap(),
            format!("{}\n", line.plain())
        );
    }
}
