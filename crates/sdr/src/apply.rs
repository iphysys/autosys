//! Applies a transition's operations to its base document and checks the
//! semantic rules the schema cannot express:
//!
//! - element ids are unique across the document (system.schema.json);
//! - `set_parameter` replaces only a parameter;
//! - `link_evidence` targets an existing requirement or parameter;
//! - decision and assumption `related_ids`, and a parameter's assumption
//!   source, name existing elements once every operation is applied.
//!
//! A requirement's `numeric.parameter_id` may name a parameter that does not
//! exist yet: requirements often come before the design that meets them.

use std::collections::HashMap;

use contracts::ids::{ElementId, EvidenceId, SdrVersion};
use contracts::types::{
    Assumption, Decision, Operation, Parameter, ParameterSource, Requirement, SdrDocument,
};

use crate::transitions::{Problem, ProblemKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Requirement,
    Parameter,
    Decision,
    Assumption,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Requirement => "requirement",
            Kind::Parameter => "parameter",
            Kind::Decision => "decision",
            Kind::Assumption => "assumption",
        }
    }
}

/// Version `next`: `base` with `operations` applied, and every problem found.
/// An operation with a problem is skipped, so later operations are still
/// checked.
pub(crate) fn apply(
    base: &SdrDocument,
    next: SdrVersion,
    operations: &[Operation],
) -> (SdrDocument, Vec<Problem>) {
    let mut doc = base.clone();
    doc.parent_version = Some(base.version.clone());
    doc.version = next;
    let mut kinds = element_kinds(&doc);
    let mut problems = Vec::new();

    for (index, operation) in operations.iter().enumerate() {
        let at = |message: String| format!("operation {index}: {message}");
        match operation {
            Operation::AddRequirement { requirement } => {
                match add(&mut kinds, &requirement.id, Kind::Requirement) {
                    Ok(()) => doc.requirements.push(requirement.clone()),
                    Err(p) => problems.push(Problem::new(p.kind, at(p.message))),
                }
            }
            Operation::SetParameter { parameter } => match kinds.get(parameter.id.as_str()) {
                Some(Kind::Parameter) => {
                    if let Some(existing) = doc
                        .design
                        .parameters
                        .iter_mut()
                        .find(|p| p.id == parameter.id)
                    {
                        *existing = parameter.clone();
                    }
                }
                Some(other) => problems.push(Problem::new(
                    ProblemKind::DuplicateElement,
                    at(format!(
                        "{} is a {}, not a parameter",
                        parameter.id.as_str(),
                        other.name()
                    )),
                )),
                None => {
                    kinds.insert(parameter.id.as_str().to_owned(), Kind::Parameter);
                    doc.design.parameters.push(parameter.clone());
                }
            },
            Operation::AddDecision { decision } => {
                match add(&mut kinds, &decision.id, Kind::Decision) {
                    Ok(()) => doc.design.decisions.push(decision.clone()),
                    Err(p) => problems.push(Problem::new(p.kind, at(p.message))),
                }
            }
            Operation::AddAssumption { assumption } => {
                match add(&mut kinds, &assumption.id, Kind::Assumption) {
                    Ok(()) => doc.design.assumptions.push(assumption.clone()),
                    Err(p) => problems.push(Problem::new(p.kind, at(p.message))),
                }
            }
            Operation::LinkEvidence {
                element_id,
                evidence_id,
            } => {
                if let Err(p) = link(&mut doc, &kinds, element_id, evidence_id) {
                    problems.push(Problem::new(p.kind, at(p.message)));
                }
            }
        }
    }

    problems.extend(unresolved_references(&doc, &kinds));
    (doc, problems)
}

fn element_kinds(doc: &SdrDocument) -> HashMap<String, Kind> {
    let requirements = doc.requirements.iter().map(|r| (&r.id, Kind::Requirement));
    let parameters = doc
        .design
        .parameters
        .iter()
        .map(|p| (&p.id, Kind::Parameter));
    let decisions = doc.design.decisions.iter().map(|d| (&d.id, Kind::Decision));
    let assumptions = doc
        .design
        .assumptions
        .iter()
        .map(|a| (&a.id, Kind::Assumption));
    requirements
        .chain(parameters)
        .chain(decisions)
        .chain(assumptions)
        .map(|(id, kind)| (id.as_str().to_owned(), kind))
        .collect()
}

fn add(kinds: &mut HashMap<String, Kind>, id: &ElementId, kind: Kind) -> Result<(), Problem> {
    if let Some(existing) = kinds.get(id.as_str()) {
        return Err(Problem::new(
            ProblemKind::DuplicateElement,
            format!(
                "{} {} already exists as a {}",
                kind.name(),
                id.as_str(),
                existing.name()
            ),
        ));
    }
    kinds.insert(id.as_str().to_owned(), kind);
    Ok(())
}

/// Adds `evidence_id` to the element's evidence, once.
fn link(
    doc: &mut SdrDocument,
    kinds: &HashMap<String, Kind>,
    element_id: &ElementId,
    evidence_id: &EvidenceId,
) -> Result<(), Problem> {
    let evidence = match kinds.get(element_id.as_str()) {
        Some(Kind::Requirement) => doc
            .requirements
            .iter_mut()
            .find(|r| &r.id == element_id)
            .map(|r: &mut Requirement| &mut r.evidence_ids),
        Some(Kind::Parameter) => doc
            .design
            .parameters
            .iter_mut()
            .find(|p| &p.id == element_id)
            .map(|p: &mut Parameter| &mut p.evidence_ids),
        Some(other) => {
            return Err(Problem::new(
                ProblemKind::CannotHoldEvidence,
                format!(
                    "{} is a {}, which holds no evidence",
                    element_id.as_str(),
                    other.name()
                ),
            ));
        }
        None => None,
    };
    let Some(evidence) = evidence else {
        return Err(Problem::new(
            ProblemKind::UnknownElement,
            format!("element {} does not exist", element_id.as_str()),
        ));
    };
    if !evidence.contains(evidence_id) {
        evidence.push(evidence_id.clone());
    }
    Ok(())
}

fn unresolved_references(doc: &SdrDocument, kinds: &HashMap<String, Kind>) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut missing = |owner: &ElementId, field: &str, id: &ElementId| {
        problems.push(Problem::new(
            ProblemKind::UnknownElement,
            format!(
                "{} {field} names {}, which does not exist",
                owner.as_str(),
                id.as_str()
            ),
        ));
    };
    for Decision {
        id, related_ids, ..
    } in &doc.design.decisions
    {
        for related in related_ids
            .iter()
            .filter(|r| !kinds.contains_key(r.as_str()))
        {
            missing(id, "related_ids", related);
        }
    }
    for Assumption {
        id, related_ids, ..
    } in &doc.design.assumptions
    {
        for related in related_ids
            .iter()
            .filter(|r| !kinds.contains_key(r.as_str()))
        {
            missing(id, "related_ids", related);
        }
    }
    for parameter in &doc.design.parameters {
        if let ParameterSource::Assumption { assumption_id } = &parameter.source
            && kinds.get(assumption_id.as_str()) != Some(&Kind::Assumption)
        {
            missing(&parameter.id, "source assumption_id", assumption_id);
        }
    }
    problems
}
