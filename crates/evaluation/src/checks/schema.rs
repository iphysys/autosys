use async_trait::async_trait;
use contracts::types::Verdict;
use contracts::{
    Check, CheckDescriptor, CheckFinding, CheckSubject, Error, ExecutionContext, SchemaSet,
};

use super::{descriptor, finding};

const DOCUMENT: &str = "sdr/system.schema.json";
const TRANSITION: &str = "sdr/transition.schema.json";

/// Check `schema`: the subject satisfies its canonical JSON Schema, including
/// constraints the Rust types do not enforce. A document is checked against
/// `system.schema.json`, a transition against `transition.schema.json`.
pub struct SchemaCheck {
    descriptor: CheckDescriptor,
    schemas: SchemaSet,
}

impl SchemaCheck {
    pub const ID: &'static str = "schema";

    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            descriptor: descriptor(Self::ID, "0.1.0")?,
            schemas: SchemaSet::compile(&[DOCUMENT, TRANSITION])?,
        })
    }
}

#[async_trait]
impl Check for SchemaCheck {
    fn descriptor(&self) -> &CheckDescriptor {
        &self.descriptor
    }

    async fn evaluate(
        &self,
        subject: CheckSubject<'_>,
        ctx: &ExecutionContext,
    ) -> Result<Vec<CheckFinding>, Error> {
        ctx.ensure_not_cancelled()?;
        let (schema, errors) = match subject {
            CheckSubject::Document(doc) => (DOCUMENT, self.schemas.errors_of(DOCUMENT, doc)?),
            CheckSubject::Transition { transition, .. } => {
                (TRANSITION, self.schemas.errors_of(TRANSITION, transition)?)
            }
        };
        let result = if errors.is_empty() {
            finding(
                Vec::new(),
                Vec::new(),
                Verdict::Pass,
                format!("satisfies {schema}"),
            )
        } else {
            finding(
                Vec::new(),
                Vec::new(),
                Verdict::Fail,
                format!("breaks {schema}: {}", errors.join("; ")),
            )
        };
        Ok(vec![result])
    }
}
