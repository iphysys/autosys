//! Assembling a context (blueprint section 8, ADR-0029).
//!
//! [`assemble`] is a pure function: the same inputs always give the same
//! context and the same hash. It takes no clock, no randomness and does no
//! I/O. [`ContextBuilder::build`] reads the SDR and the event log, calls
//! `assemble`, and records the result in the event log.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use contracts::ids::SdrVersion;
use contracts::types::{
    Actor, ContextProfile, ContextSectionName, Event, EventPayload, Job, SdrDocument, Sha256,
    Timestamp, TokenUsage, VersionedHash,
};
use contracts::{
    Clock, ContentBlock, Error, ErrorCode, EventLog, ExecutionContext, Message, ModelRequest,
    NewEvent, Role, ToolSpec,
};
use sdr::SdrManager;
use serde::Serialize;
use tracing::Instrument;

use crate::compaction::{Pool, Trim, fit};
use crate::error::ContextError;
use crate::hash::context_hash;
use crate::prompts::{Prompt, PromptLibrary};
use crate::render::{
    Item, ORDER, Omission, SectionBody, Trust, messages_tokens, open_turn, section_tag, trust_of,
    untrusted,
};
use crate::selection;
use crate::token_budget::{ByteEstimate, ContextProfiles, TokenCounter, cap};

/// Every context starts with this prompt; the agent's own prompt follows it.
const BASE_PROMPT: &str = "system/base.md";

/// The component recorded as the actor of `context_assembled`.
const COMPONENT: &str = "context";

/// The state a context is built from: which SDR version, and which events.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub sdr_version: SdrVersion,
    /// Events after this one are not seen. 0 for a run with no events.
    pub last_event_seq: u64,
}

/// Why each part of a context was included, and what it cost.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Manifest {
    pub profile: String,
    pub sdr_version: u64,
    pub last_event_seq: u64,
    /// The prompts in the instructions, with their versions and hashes, for
    /// run provenance.
    pub prompts: Vec<VersionedHash>,
    /// Counted by the caller, for example the tool definitions.
    pub reserved_tokens: u64,
    pub total_tokens: u64,
    /// All sections, in the order the model sees them.
    pub sections: Vec<SectionManifest>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SectionManifest {
    pub name: ContextSectionName,
    pub trust: Trust,
    pub tokens: u64,
    pub cap: u64,
    /// Where each included item came from, for example `prompt:system.base@0.1.0`,
    /// `sdr:R-001@v7`, `event:42` or `tool:measure@call-1`.
    pub items: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted: Option<Omission>,
}

/// A context ready to send, with what is needed to inspect and record it.
#[derive(Clone, Debug, PartialEq)]
pub struct AssembledContext {
    system: String,
    messages: Vec<Message>,
    manifest: Manifest,
    hash: Sha256,
    total_tokens: u64,
    event_seq: Option<u64>,
}

impl AssembledContext {
    pub fn system(&self) -> &str {
        &self.system
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The hash of exactly what the model sees. See the `hash` module.
    pub fn hash(&self) -> &Sha256 {
        &self.hash
    }

    /// The estimated size, reserved tokens included.
    pub fn total_tokens(&self) -> u64 {
        self.total_tokens
    }

    /// The sequence number of the `context_assembled` event that recorded
    /// this context. `None` for a context from [`assemble`].
    pub fn event_seq(&self) -> Option<u64> {
        self.event_seq
    }

    /// The request for one model call. The tool definitions and the output
    /// limit are the caller's: pass the same tools whose size went into
    /// `reserved_tokens`.
    pub fn into_model_request(
        self,
        model_id: String,
        tools: Vec<ToolSpec>,
        max_output_tokens: u32,
    ) -> ModelRequest {
        ModelRequest {
            model_id,
            system: self.system,
            messages: self.messages,
            tools,
            max_output_tokens,
        }
    }

    /// Compares the estimate with the input tokens the provider reported, and
    /// logs it. The `context_assembled` and `model_call_completed` events
    /// keep the pair, so `bytes_per_token` can be tuned from real runs.
    pub fn compare_usage(&self, usage: &TokenUsage) -> UsageComparison {
        let comparison = UsageComparison {
            estimated_input_tokens: self.total_tokens,
            actual_input_tokens: usage.input_tokens,
        };
        if comparison.under_estimated() {
            tracing::warn!(
                context_hash = self.hash.as_str(),
                estimated = comparison.estimated_input_tokens,
                actual = comparison.actual_input_tokens,
                "the context was larger than estimated; lower bytes_per_token"
            );
        } else {
            tracing::debug!(
                context_hash = self.hash.as_str(),
                estimated = comparison.estimated_input_tokens,
                actual = comparison.actual_input_tokens,
                "context size estimate compared"
            );
        }
        comparison
    }

    fn recorded(mut self, event_seq: u64) -> Self {
        self.event_seq = Some(event_seq);
        self
    }
}

/// The estimate of a context next to what the provider counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageComparison {
    pub estimated_input_tokens: u64,
    pub actual_input_tokens: u64,
}

impl UsageComparison {
    /// The provider counted more than the estimate. This is the direction
    /// that risks an overflow at the provider.
    pub fn under_estimated(&self) -> bool {
        self.actual_input_tokens > self.estimated_input_tokens
    }

    /// Actual over estimated, or `None` when nothing was estimated.
    pub fn ratio(&self) -> Option<f64> {
        (self.estimated_input_tokens > 0)
            .then(|| self.actual_input_tokens as f64 / self.estimated_input_tokens as f64)
    }
}

/// Everything [`assemble`] needs. The order of `design` elements and of
/// `events` does not matter.
pub struct AssemblyInputs<'a> {
    pub job: &'a Job,
    /// The base prompt first, then the agent's. Their text is the system prompt.
    pub instructions: &'a [&'a Prompt],
    /// The SDR slice the task may read, at `snapshot.sdr_version`.
    pub design: &'a SdrDocument,
    /// Events of the job's run. Those after `snapshot.last_event_seq` are ignored.
    pub events: &'a [Event],
    /// The in-flight tool turn, starting with an assistant message. Empty
    /// between turns. Tool results are passed as the tool returned them.
    pub open_turn: &'a [Message],
    /// Tokens the caller will add to the request, such as tool definitions.
    pub reserved_tokens: u64,
    pub snapshot: &'a Snapshot,
    pub counter: &'a dyn TokenCounter,
}

fn check_cap(
    section: ContextSectionName,
    needed: u64,
    profile: &ContextProfile,
) -> Result<(), ContextError> {
    let limit = cap(profile, section);
    if needed > limit {
        return Err(ContextError::Overflow {
            section,
            needed,
            limit,
        });
    }
    Ok(())
}

/// Builds a context from its inputs. Deterministic and free of I/O.
///
/// A fixed section that exceeds its cap, or an open turn that does not fit,
/// is an error: nothing is truncated. The trimmable sections give way to the
/// rest, as described in the `compaction` module.
pub fn assemble(
    inputs: &AssemblyInputs<'_>,
    profile: &ContextProfile,
) -> Result<AssembledContext, ContextError> {
    let counter = inputs.counter;
    let snapshot = inputs.snapshot;
    let job = inputs.job;
    if inputs.design.version != snapshot.sdr_version {
        return Err(ContextError::Internal(format!(
            "the design is version {} but the snapshot is version {}",
            inputs.design.version.0, snapshot.sdr_version.0
        )));
    }

    // 1. Instructions: the system prompt.
    let system = inputs
        .instructions
        .iter()
        .map(|p| p.body())
        .collect::<Vec<_>>()
        .join("\n\n");
    let instructions_tokens = counter.count(&system);
    check_cap(
        ContextSectionName::Instructions,
        instructions_tokens,
        profile,
    )?;

    // 2-4. The task, the job and the design: rebuilt from their stores every
    // time, and never cut.
    let task_source = format!("task:{}", job.task.id.as_str());
    let task = SectionBody::new(
        section_tag(
            ContextSectionName::TaskContract,
            &[("source", &task_source)],
        ),
        selection::task_items(job),
    );
    let job_source = format!("job:{}", job.id.as_str());
    let job_state = SectionBody::new(
        section_tag(ContextSectionName::JobState, &[("source", &job_source)]),
        selection::job_items(job),
    );
    let sdr_source = format!("sdr:{}", inputs.design.project_id.as_str());
    let sdr_version = snapshot.sdr_version.0.to_string();
    let design = SectionBody::new(
        section_tag(
            ContextSectionName::DesignState,
            &[("source", &sdr_source), ("version", &sdr_version)],
        ),
        selection::design_items(inputs.design, &snapshot.sdr_version)?,
    );
    let mut fixed_tokens = instructions_tokens;
    for (name, body) in [
        (ContextSectionName::TaskContract, &task),
        (ContextSectionName::JobState, &job_state),
        (ContextSectionName::DesignState, &design),
    ] {
        let tokens = counter.count(&body.render());
        check_cap(name, tokens, profile)?;
        fixed_tokens = fixed_tokens.saturating_add(tokens);
    }

    // 5-6. History and untrusted text, from the events of the window.
    // No sequence number in the tags: the snapshot is in the manifest and the
    // event, and a number that moves with every `context_assembled` would
    // change the hash although the model would see the same content.
    let events_source = format!("events:{}", job.run_id.as_str());
    let tag_attributes = [("source", events_source.as_str())];
    let mut history_items = Vec::new();
    let mut untrusted_items = Vec::new();
    for event in selection::window(
        inputs.events,
        snapshot.last_event_seq,
        profile.history_window_events.get(),
    ) {
        let Some(view) = selection::view(event) else {
            continue;
        };
        let seq = event.seq.get();
        history_items.push(Item {
            provenance: format!("event:{seq}"),
            seq: Some(seq),
            text: view.line,
        });
        for free in view.free_text {
            let source = format!("event:{seq}");
            untrusted_items.push(Item {
                provenance: format!("event:{seq}/{}", free.field),
                seq: Some(seq),
                text: untrusted::with_attributes(
                    &[("source", source.as_str()), ("field", free.field.as_str())],
                    &free.text,
                ),
            });
        }
    }
    let mut history = SectionBody::new(
        section_tag(ContextSectionName::RunHistory, &tag_attributes),
        history_items,
    );
    let mut untrusted_content = SectionBody::new(
        section_tag(ContextSectionName::UntrustedContent, &tag_attributes),
        untrusted_items,
    );

    // 7. The open turn: never trimmed.
    let open = open_turn(inputs.open_turn)?;
    let open_tokens = messages_tokens(&open.messages, counter)?;

    let fitted = fit(
        &Pool {
            total: profile.total_tokens.get(),
            reserved: inputs.reserved_tokens,
            fixed: fixed_tokens,
            open_turn: open_tokens,
        },
        Trim {
            name: ContextSectionName::RunHistory,
            body: &mut history,
            cap: cap(profile, ContextSectionName::RunHistory),
        },
        Trim {
            name: ContextSectionName::UntrustedContent,
            body: &mut untrusted_content,
            cap: cap(profile, ContextSectionName::UntrustedContent),
        },
        counter,
    )?;

    // The wire layout: the sections in one user message, then the open turn.
    let blocks: Vec<ContentBlock> = [&task, &job_state, &design, &history, &untrusted_content]
        .into_iter()
        .map(|body| ContentBlock::Text {
            text: body.render(),
        })
        .collect();
    let mut messages = vec![Message {
        role: Role::User,
        content: blocks,
    }];
    messages.extend(open.messages);

    let total_tokens = fixed_tokens
        .saturating_add(open_tokens)
        .saturating_add(fitted.run_history)
        .saturating_add(fitted.untrusted_content)
        .saturating_add(inputs.reserved_tokens);
    if total_tokens > profile.total_tokens.get() {
        return Err(ContextError::Internal(format!(
            "the fitted context uses {total_tokens} tokens, above the total {}",
            profile.total_tokens
        )));
    }

    let hash = context_hash(&system, &messages)?;
    let prompts: Vec<VersionedHash> = inputs
        .instructions
        .iter()
        .map(|p| p.provenance().clone())
        .collect();
    let manifest = Manifest {
        profile: profile.name.as_str().to_owned(),
        sdr_version: snapshot.sdr_version.0,
        last_event_seq: snapshot.last_event_seq,
        prompts,
        reserved_tokens: inputs.reserved_tokens,
        total_tokens,
        sections: ORDER
            .into_iter()
            .map(|name| {
                let (tokens, items, omitted) = match name {
                    ContextSectionName::Instructions => (
                        instructions_tokens,
                        inputs.instructions.iter().map(|p| p.label()).collect(),
                        None,
                    ),
                    ContextSectionName::TaskContract => {
                        (counter.count(&task.render()), task.provenance(), None)
                    }
                    ContextSectionName::JobState => (
                        counter.count(&job_state.render()),
                        job_state.provenance(),
                        None,
                    ),
                    ContextSectionName::DesignState => {
                        (counter.count(&design.render()), design.provenance(), None)
                    }
                    ContextSectionName::RunHistory => {
                        (fitted.run_history, history.provenance(), history.omission())
                    }
                    ContextSectionName::UntrustedContent => (
                        fitted.untrusted_content,
                        untrusted_content.provenance(),
                        untrusted_content.omission(),
                    ),
                    ContextSectionName::OpenTurn => (open_tokens, open.provenance.clone(), None),
                };
                SectionManifest {
                    name,
                    trust: trust_of(name),
                    tokens,
                    cap: cap(profile, name),
                    items,
                    omitted,
                }
            })
            .collect(),
    };

    Ok(AssembledContext {
        system,
        messages,
        manifest,
        hash,
        total_tokens,
        event_seq: None,
    })
}

/// What a call wants built.
pub struct BuildRequest<'a> {
    /// The job being worked on. Its task is the task contract. The run is the
    /// one in the execution context.
    pub job: &'a Job,
    /// The agent's prompt, for example `orchestrator/main.md`.
    pub prompt: &'a str,
    /// A profile in `config/context.yaml`; `None` is `default`.
    pub profile: Option<&'a str>,
    /// The in-flight tool turn, starting with an assistant message. Empty
    /// between turns. Pass tool results raw: they are wrapped here, and a
    /// result wrapped twice would show its first wrapper as text.
    pub open_turn: &'a [Message],
    /// Tokens the request adds, such as the tool definitions.
    pub reserved_tokens: u64,
    /// Pins the SDR version and the last event seen, for a replay or to keep
    /// one view of the design across a whole tool-use turn. `None` takes the
    /// head and the last event now.
    pub at: Option<Snapshot>,
}

/// Builds the context of each model call and records it.
pub struct ContextBuilder {
    sdr: Arc<SdrManager>,
    events: Arc<dyn EventLog>,
    prompts: Arc<PromptLibrary>,
    profiles: ContextProfiles,
    clock: Arc<dyn Clock>,
    actor: Actor,
}

impl ContextBuilder {
    /// Fails if the library lacks the base prompt, `system/base.md`.
    pub fn new(
        sdr: Arc<SdrManager>,
        events: Arc<dyn EventLog>,
        prompts: Arc<PromptLibrary>,
        profiles: ContextProfiles,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ContextError> {
        prompts.get(BASE_PROMPT)?;
        let component = COMPONENT.parse().map_err(|e| {
            ContextError::Internal(format!("{COMPONENT} is not an identifier: {e}"))
        })?;
        Ok(Self {
            sdr,
            events,
            prompts,
            profiles,
            clock,
            actor: Actor::System { component },
        })
    }

    /// Reads the SDR and the event log, assembles the context, and appends a
    /// `context_assembled` event with its hash. The caller must not append
    /// that event again.
    ///
    /// If the event cannot be appended the build fails: a context that is not
    /// recorded is never handed out. A timeout on the append may leave the
    /// event written; a retry then records a second one for the same context.
    pub async fn build(
        &self,
        req: BuildRequest<'_>,
        ctx: &ExecutionContext,
    ) -> Result<AssembledContext, ContextError> {
        let span = tracing::info_span!(
            "context.build",
            run_id = ctx.run_id().map_or("none", |r| r.as_str()),
            task_id = req.job.task.id.as_str(),
            correlation_id = ctx.correlation_id().as_str(),
        );
        self.build_recorded(req, ctx).instrument(span).await
    }

    async fn build_recorded(
        &self,
        req: BuildRequest<'_>,
        ctx: &ExecutionContext,
    ) -> Result<AssembledContext, ContextError> {
        // Everything that can be checked without I/O comes first, so a bad
        // request never reads or writes anything.
        ctx.ensure_not_cancelled()
            .map_err(|_| ContextError::Cancelled)?;
        let profile = self.profiles.resolve(req.profile)?;
        let base = self.prompts.get(BASE_PROMPT)?;
        let agent = self.prompts.get(req.prompt)?;
        let run_id = ctx.run_id().ok_or(ContextError::MissingRun)?;
        open_turn(req.open_turn)?;

        let limit = Duration::from_secs(profile.io_timeout_seconds.get());
        let project = &req.job.project_id;
        let snapshot = match req.at {
            Some(snapshot) => snapshot,
            None => {
                let head = bounded(
                    ctx,
                    limit,
                    "reading the SDR head",
                    self.sdr.head(project, ctx),
                    ContextError::Sdr,
                )
                .await?;
                let sdr_version = head.ok_or_else(|| {
                    ContextError::Sdr(Error::new(
                        ErrorCode::NotFound,
                        format!("project {} has no SDR", project.as_str()),
                    ))
                })?;
                let last_event_seq = bounded(
                    ctx,
                    limit,
                    "reading the last event",
                    self.events.last_seq(run_id),
                    ContextError::Events,
                )
                .await?;
                Snapshot {
                    sdr_version,
                    last_event_seq,
                }
            }
        };

        let design = bounded(
            ctx,
            limit,
            "reading the SDR",
            self.sdr.read(
                project,
                &req.job.task.sdr.read_slice,
                &snapshot.sdr_version,
                ctx,
            ),
            ContextError::Sdr,
        )
        .await?;
        let window = profile.history_window_events.get();
        let events = bounded(
            ctx,
            limit,
            "reading the events",
            self.events.read(
                run_id,
                snapshot.last_event_seq.saturating_sub(window),
                usize::try_from(window).unwrap_or(usize::MAX),
            ),
            ContextError::Events,
        )
        .await?;

        let counter = ByteEstimate::for_profile(profile);
        let instructions = [base, agent];
        let assembled = assemble(
            &AssemblyInputs {
                job: req.job,
                instructions: &instructions,
                design: &design,
                events: &events,
                open_turn: req.open_turn,
                reserved_tokens: req.reserved_tokens,
                snapshot: &snapshot,
                counter: &counter,
            },
            profile,
        )?;

        let stored = bounded(
            ctx,
            limit,
            "recording the context",
            self.events.append(NewEvent {
                run_id: run_id.clone(),
                occurred_at: Timestamp(self.clock.now()),
                correlation_id: ctx.correlation_id().clone(),
                actor: self.actor.clone(),
                payload: EventPayload::ContextAssembled {
                    context_hash: assembled.hash.clone(),
                    total_tokens: assembled.total_tokens,
                    sdr_version: snapshot.sdr_version.clone(),
                    last_event_seq: snapshot.last_event_seq,
                },
            }),
            ContextError::Events,
        )
        .await?;
        tracing::debug!(
            profile = profile.name.as_str(),
            sdr_version = snapshot.sdr_version.0,
            last_event_seq = snapshot.last_event_seq,
            total_tokens = assembled.total_tokens,
            context_hash = assembled.hash.as_str(),
            event_seq = stored.seq.get(),
            "context assembled"
        );
        Ok(assembled.recorded(stored.seq.get()))
    }
}

/// Runs `operation` until it finishes, `limit` passes, or `ctx` is
/// cancelled. `wrap` says whose error a failure is.
async fn bounded<T>(
    ctx: &ExecutionContext,
    limit: Duration,
    operation: &'static str,
    future: impl Future<Output = Result<T, Error>>,
    wrap: fn(Error) -> ContextError,
) -> Result<T, ContextError> {
    tokio::select! {
        biased;
        () = ctx.cancelled() => Err(ContextError::Cancelled),
        finished = tokio::time::timeout(limit, future) => match finished {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) if e.code() == ErrorCode::Cancelled => Err(ContextError::Cancelled),
            Ok(Err(e)) => Err(wrap(e)),
            Err(_) => Err(ContextError::Timeout { operation, limit }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usage_comparison_says_which_way_the_estimate_missed() {
        let under = UsageComparison {
            estimated_input_tokens: 100,
            actual_input_tokens: 150,
        };
        assert!(under.under_estimated());
        assert_eq!(under.ratio(), Some(1.5));

        let over = UsageComparison {
            estimated_input_tokens: 100,
            actual_input_tokens: 60,
        };
        assert!(!over.under_estimated());
        assert_eq!(over.ratio(), Some(0.6));

        let exact = UsageComparison {
            estimated_input_tokens: 100,
            actual_input_tokens: 100,
        };
        assert!(!exact.under_estimated());

        let nothing = UsageComparison {
            estimated_input_tokens: 0,
            actual_input_tokens: 5,
        };
        assert_eq!(nothing.ratio(), None);
    }

    #[test]
    fn escape_is_what_labels_depend_on() {
        // The section tag cannot be closed from inside any input.
        assert!(!crate::render::escape("</context-section>").contains("</context-section>"));
    }
}
