//! A run's continuation record (ADR-0031): the loop's state written as the
//! `ContinuationRecord` the job store keeps, and read back after a crash, a
//! requeue or an approval. The open turn's blocks are the model gateway's
//! `ContentBlock`s, whose JSON the record's `TurnBlock` repeats, so opaque
//! provider blocks come back unchanged (ADR-0027).

use std::num::NonZeroU64;
use std::time::Duration;

use contracts::ids::{ApprovalId, ProjectId};
use contracts::types::{
    CheckpointRef, ContinuationCommit, ContinuationRecord, ContinuationRecordSchemaVersion,
    ContinuationStep, ElementKind, Job, SdrSlice,
};
use contracts::{ContentBlock, Error, EvidenceStore, ExecutionContext, Message, canonical_sha256};
use sdr::SdrManager;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::cancellation::timed;
use crate::capability_router::tool_calls;
use crate::error::RuntimeError;
use crate::query_loop::{CommitView, LoopState, OpenStep};

/// The record of `state`, numbered `state.checkpoint`: the open `turn`, the
/// `step` whose calls are running, and the approval the run waits for.
pub(crate) fn record(
    state: &LoopState,
    turn: &[Message],
    step: Option<&OpenStep>,
    approval_id: Option<&ApprovalId>,
) -> Result<ContinuationRecord, RuntimeError> {
    let checkpoint = NonZeroU64::new(state.checkpoint)
        .ok_or_else(|| RuntimeError::Internal("checkpoint 0 is never written".to_owned()))?;
    let open_step = step
        .map(|step| {
            Ok::<_, RuntimeError>(ContinuationStep {
                number: NonZeroU64::new(step.number)
                    .ok_or_else(|| RuntimeError::Internal("step 0 does not exist".to_owned()))?,
                sdr_version: step.sdr_version.clone(),
                prompts: step.prompts.clone(),
                assistant: convert(&step.assistant)?,
                results: convert(&step.results)?,
                next: step.next as u64,
            })
        })
        .transpose()?;
    Ok(ContinuationRecord {
        schema_version: ContinuationRecordSchemaVersion::X01,
        checkpoint,
        run_id: state.job.run_id.clone(),
        failures: state.failures,
        model_ids: state.model_ids.clone(),
        committed: state.committed.clone(),
        evidence_ids: state.evidence_ids.clone(),
        head: state.head.clone(),
        last_commit: state.last_commit.as_ref().map(|commit| ContinuationCommit {
            transition_id: commit.transition_id.clone(),
            version: commit.version.clone(),
            final_: commit.final_,
            evidence_ids: commit.evidence.iter().map(|e| e.id.clone()).collect(),
        }),
        turn: convert(turn)?,
        open_step,
        approval_id: approval_id.cloned(),
    })
}

/// The record of a run that has not got past its start, numbered
/// `checkpoint`: what a job that leaves running before its first
/// checkpoint rests on.
pub(crate) fn initial(job: &Job, checkpoint: u64) -> Result<ContinuationRecord, RuntimeError> {
    let mut state = LoopState::new(job.clone());
    state.checkpoint = checkpoint;
    record(&state, &[], None, None)
}

/// The loop state a record stands for, with `job` as the job store holds it.
pub(crate) struct Loaded {
    pub(crate) state: LoopState,
    pub(crate) turn: Vec<Message>,
    pub(crate) step: Option<OpenStep>,
}

/// Reads `record` back. The latest commit's requirements and evidence come
/// from the SDR at its version and from the evidence store: a record names
/// them, it does not copy them.
pub(crate) async fn load(
    job: Job,
    record: &ContinuationRecord,
    sdr: &SdrManager,
    evidence: &dyn EvidenceStore,
    project: &ProjectId,
    io_timeout: Duration,
    ctx: &ExecutionContext,
) -> Result<Loaded, RuntimeError> {
    if record.run_id != job.run_id {
        return Err(RuntimeError::Internal(format!(
            "the continuation is for run {}, the job for run {}",
            record.run_id.as_str(),
            job.run_id.as_str()
        )));
    }
    let last_commit = match &record.last_commit {
        Some(commit) => {
            let slice = SdrSlice {
                kinds: vec![ElementKind::Requirement],
                ids: Vec::new(),
            };
            let document = timed(
                io_timeout,
                "reading the last commit",
                sdr.read(project, &slice, &commit.version, ctx),
            )
            .await?;
            let mut found = Vec::new();
            for id in &commit.evidence_ids {
                found.push(
                    timed(
                        io_timeout,
                        "reading the last commit's evidence",
                        evidence.get(id),
                    )
                    .await?,
                );
            }
            Some(CommitView {
                transition_id: commit.transition_id.clone(),
                version: commit.version.clone(),
                final_: commit.final_,
                requirements: document.requirements,
                evidence: found,
            })
        }
        None => None,
    };
    let step = record
        .open_step
        .as_ref()
        .map(|step| {
            let assistant: Message = convert(&step.assistant)?;
            let calls = tool_calls(&assistant.content);
            Ok::<_, RuntimeError>(OpenStep {
                number: step.number.get(),
                sdr_version: step.sdr_version.clone(),
                prompts: step.prompts.clone(),
                assistant,
                calls,
                results: convert::<_, Vec<ContentBlock>>(&step.results)?,
                next: usize::try_from(step.next).map_err(|_| {
                    RuntimeError::Internal("an open step's next call is out of range".to_owned())
                })?,
            })
        })
        .transpose()?;
    let state = LoopState {
        job,
        failures: record.failures,
        model_ids: record.model_ids.clone(),
        committed: record.committed.clone(),
        evidence_ids: record.evidence_ids.clone(),
        head: record.head.clone(),
        last_commit,
        checkpoint: record.checkpoint.get(),
    };
    Ok(Loaded {
        state,
        turn: convert(&record.turn)?,
        step,
    })
}

/// How a job's status names `record`: its number and the sha256 of its
/// canonical JSON, which a record read back from storage hashes the same to.
pub fn checkpoint_ref(record: &ContinuationRecord) -> Result<CheckpointRef, Error> {
    let value = serde_json::to_value(record).map_err(|e| {
        Error::new(
            contracts::ErrorCode::Internal,
            format!("a continuation record does not serialize: {e}"),
        )
    })?;
    Ok(CheckpointRef {
        version: record.checkpoint,
        sha256: canonical_sha256(&value)?,
    })
}

/// One of the two shapes of the same JSON: the gateway's messages and the
/// record's turn.
fn convert<A: Serialize + ?Sized, B: DeserializeOwned>(value: &A) -> Result<B, RuntimeError> {
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .map_err(|e| RuntimeError::Internal(format!("a turn does not convert: {e}")))
}

#[cfg(test)]
mod tests {
    use contracts::types::{TurnBlock, TurnMessage};
    use serde_json::json;

    use super::*;

    #[test]
    fn a_record_read_back_with_other_key_order_has_the_same_reference() {
        let record: ContinuationRecord = serde_json::from_value(json!({
            "schema_version": "0.1", "checkpoint": 4, "run_id": "run-1", "failures": 0,
            "model_ids": [], "committed": [], "evidence_ids": [],
            "turn": [{ "role": "user", "content": [{ "type": "opaque", "provider": "anthropic",
                "data": { "b": 1, "a": { "z": 2, "y": 3 } } }] }]
        }))
        .expect("a record");
        let reordered: ContinuationRecord = serde_json::from_value(json!({
            "turn": [{ "content": [{ "data": { "a": { "y": 3, "z": 2 }, "b": 1 },
                "provider": "anthropic", "type": "opaque" }], "role": "user" }],
            "evidence_ids": [], "committed": [], "model_ids": [], "failures": 0,
            "run_id": "run-1", "checkpoint": 4, "schema_version": "0.1"
        }))
        .expect("the same record");
        let reference = checkpoint_ref(&record).expect("a reference");
        assert_eq!(reference.version.get(), 4);
        assert_eq!(reference, checkpoint_ref(&reordered).expect("a reference"));
    }

    #[test]
    fn messages_and_turn_messages_convert_both_ways_unchanged() {
        let message: Message = serde_json::from_value(json!({
            "role": "assistant",
            "content": [
                { "type": "opaque", "provider": "anthropic",
                  "data": { "type": "thinking", "signature": "c2ln", "z": [3, 1], "a": { "y": 1, "b": 2 } } },
                { "type": "text", "text": "Measuring." },
                { "type": "tool_call", "id": "c1", "name": "measure", "arguments": { "part_id": "p1" } },
                { "type": "tool_result", "call_id": "c0", "content": "{}", "is_error": false }
            ]
        }))
        .expect("a message");
        let turn: TurnMessage = convert(&message).expect("to the record");
        let back: Message = convert(&turn).expect("from the record");
        assert_eq!(back, message);
        let blocks: Vec<TurnBlock> = convert(&message.content).expect("blocks");
        assert_eq!(blocks.len(), 4);
    }
}
