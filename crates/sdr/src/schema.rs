//! The SDR schemas, compiled once per manager (ADR-0024).

use contracts::types::{SdrDocument, Transition};
use contracts::{Error, SchemaSet};
use serde_json::Value;

const TRANSITION: &str = "sdr/transition.schema.json";
const DOCUMENT: &str = "sdr/system.schema.json";

pub(crate) struct Schemas {
    set: SchemaSet,
}

impl Schemas {
    pub(crate) fn load() -> Result<Self, Error> {
        Ok(Self {
            set: SchemaSet::compile(&[TRANSITION, DOCUMENT])?,
        })
    }

    /// Every way `transition` breaks `transition.schema.json`.
    pub(crate) fn transition_errors(&self, transition: &Transition) -> Result<Vec<String>, Error> {
        self.set.errors_of(TRANSITION, transition)
    }

    /// Every way `document` breaks `system.schema.json`.
    pub(crate) fn document_errors(&self, document: &SdrDocument) -> Result<Vec<String>, Error> {
        self.set.errors_of(DOCUMENT, document)
    }

    /// Every way a raw document breaks `system.schema.json`.
    pub(crate) fn raw_document_errors(&self, document: &Value) -> Result<Vec<String>, Error> {
        self.set.errors(DOCUMENT, document)
    }
}
