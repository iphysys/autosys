use std::collections::HashMap;

use async_trait::async_trait;
use contracts::types::{Criterion, Quantity, Requirement, Verdict};
use contracts::{Check, CheckDescriptor, CheckFinding, CheckSubject, Error, ExecutionContext};

use super::{descriptor, document, finding, input};
use crate::units::{Dimension, Unit};

/// Check `units` (blueprint section 7.5): every quantity has a supported UCUM
/// unit, and each numeric requirement states its limits in the dimension of
/// the parameter it constrains. One finding per parameter and per numeric
/// requirement.
pub struct UnitsCheck {
    descriptor: CheckDescriptor,
}

impl UnitsCheck {
    pub const ID: &'static str = "units";

    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            descriptor: descriptor(Self::ID, "0.1.0")?,
        })
    }
}

/// The quantities a criterion names, with their input names.
pub(crate) fn criterion_quantities(criterion: &Criterion) -> Vec<(&'static str, &Quantity)> {
    match criterion {
        Criterion::Le { limit } | Criterion::Ge { limit } => vec![("limit", limit)],
        Criterion::Eq { target, tolerance } => vec![("target", target), ("tolerance", tolerance)],
        Criterion::Within { min, max } => vec![("min", min), ("max", max)],
    }
}

#[async_trait]
impl Check for UnitsCheck {
    fn descriptor(&self) -> &CheckDescriptor {
        &self.descriptor
    }

    async fn evaluate(
        &self,
        subject: CheckSubject<'_>,
        ctx: &ExecutionContext,
    ) -> Result<Vec<CheckFinding>, Error> {
        ctx.ensure_not_cancelled()?;
        let doc = document(subject, Self::ID)?;
        let mut findings = Vec::new();
        let mut parameter_units: HashMap<&str, Result<Unit, String>> = HashMap::new();

        for parameter in &doc.design.parameters {
            let unit = Unit::parse(parameter.value.unit.as_str()).map_err(|e| e.to_string());
            let (verdict, message) = match &unit {
                Ok(u) => (
                    Verdict::Pass,
                    format!(
                        "{} is in {} ({})",
                        parameter.id.as_str(),
                        u.code(),
                        u.dimension()
                    ),
                ),
                Err(e) => (Verdict::Fail, format!("{}: {e}", parameter.id.as_str())),
            };
            findings.push(finding(
                vec![parameter.id.clone()],
                vec![input("value", &parameter.value)?],
                verdict,
                message,
            ));
            parameter_units.insert(parameter.id.as_str(), unit);
        }

        for requirement in &doc.requirements {
            if let Some(found) = requirement_finding(requirement, &parameter_units)? {
                findings.push(found);
            }
        }
        Ok(findings)
    }
}

/// The finding for a numeric requirement; `None` for a text-only one.
fn requirement_finding(
    requirement: &Requirement,
    parameter_units: &HashMap<&str, Result<Unit, String>>,
) -> Result<Option<CheckFinding>, Error> {
    let Some(numeric) = &requirement.numeric else {
        return Ok(None);
    };
    let quantities = criterion_quantities(&numeric.criterion);
    let inputs = quantities
        .iter()
        .map(|(name, q)| input(name, q))
        .collect::<Result<Vec<_>, Error>>()?;
    let mut problems = Vec::new();
    let mut dimension: Option<(&str, Dimension)> = None;
    for (name, quantity) in &quantities {
        match Unit::parse(quantity.unit.as_str()) {
            Err(e) => problems.push(format!("{name}: {e}")),
            Ok(unit) => match dimension {
                Some((first, d)) if d != unit.dimension() => problems.push(format!(
                    "unit mismatch: {first} is {d} but {name} is {}",
                    unit.dimension()
                )),
                Some(_) => {}
                None => dimension = Some((name, unit.dimension())),
            },
        }
    }
    let parameter = numeric.parameter_id.as_str();
    if let (Some((name, d)), Some(Ok(unit))) = (dimension, parameter_units.get(parameter))
        && unit.dimension() != d
    {
        problems.push(format!(
            "unit mismatch: {name} is {d} but parameter {parameter} is {} ({})",
            unit.dimension(),
            unit.code()
        ));
    }
    let id = requirement.id.as_str();
    let (verdict, message) = if problems.is_empty() {
        (Verdict::Pass, format!("{id}: units are consistent"))
    } else {
        (Verdict::Fail, format!("{id}: {}", problems.join("; ")))
    };
    let mut subject_ids = vec![requirement.id.clone()];
    if parameter_units.contains_key(parameter) {
        subject_ids.push(numeric.parameter_id.clone());
    }
    Ok(Some(finding(subject_ids, inputs, verdict, message)))
}
