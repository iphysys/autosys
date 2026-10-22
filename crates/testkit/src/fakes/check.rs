use async_trait::async_trait;
use contracts::types::{CheckMethod, Verdict};
use contracts::{Check, CheckDescriptor, CheckFinding, CheckSubject, Error, ExecutionContext};

use crate::builders::id;

/// A deterministic check that returns the same findings for every subject.
pub struct FakeCheck {
    descriptor: CheckDescriptor,
    findings: Vec<CheckFinding>,
}

impl FakeCheck {
    pub fn new(check_id: &str, findings: Vec<CheckFinding>) -> Self {
        Self {
            descriptor: CheckDescriptor {
                id: id(check_id),
                version: id("0.1.0"),
                method: CheckMethod::Deterministic,
            },
            findings,
        }
    }

    /// One finding with `verdict` about `subject_id`.
    pub fn with_verdict(check_id: &str, subject_id: &str, verdict: Verdict) -> Self {
        Self::new(
            check_id,
            vec![CheckFinding {
                subject_ids: vec![id(subject_id)],
                inputs: Vec::new(),
                verdict,
                message: None,
            }],
        )
    }
}

#[async_trait]
impl Check for FakeCheck {
    fn descriptor(&self) -> &CheckDescriptor {
        &self.descriptor
    }

    async fn evaluate(
        &self,
        _subject: CheckSubject<'_>,
        ctx: &ExecutionContext,
    ) -> Result<Vec<CheckFinding>, Error> {
        ctx.ensure_not_cancelled()?;
        Ok(self.findings.clone())
    }
}
