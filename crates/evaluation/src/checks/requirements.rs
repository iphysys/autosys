use async_trait::async_trait;
use contracts::types::{Criterion, Parameter, ParameterSource, Quantity, Requirement, Verdict};
use contracts::{Check, CheckDescriptor, CheckFinding, CheckSubject, Error, ExecutionContext};

use super::units::criterion_quantities;
use super::{descriptor, document, finding, input};
use crate::units::Unit;

/// Relative tolerance for comparisons after unit conversion, so that, for
/// example, 300 g satisfies "at most 0.3 kg" despite floating-point rounding.
const RELATIVE_TOLERANCE: f64 = 1e-9;

/// Check `requirement.numeric_threshold` (ADR-0021): compares the parameter a
/// requirement names with the requirement's threshold, after converting both
/// to coherent SI units. One finding per requirement. A requirement this
/// check cannot decide (no numeric criterion, no parameter value yet, or
/// incompatible units) is `unverified`, never `pass`.
pub struct NumericThresholdCheck {
    descriptor: CheckDescriptor,
}

impl NumericThresholdCheck {
    pub const ID: &'static str = "requirement.numeric_threshold";

    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            descriptor: descriptor(Self::ID, "0.1.0")?,
        })
    }
}

#[async_trait]
impl Check for NumericThresholdCheck {
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
        doc.requirements
            .iter()
            .map(|requirement| {
                let parameter = requirement.numeric.as_ref().and_then(|n| {
                    doc.design
                        .parameters
                        .iter()
                        .find(|p| p.id == n.parameter_id)
                });
                evaluate(requirement, parameter)
            })
            .collect()
    }
}

fn evaluate(
    requirement: &Requirement,
    parameter: Option<&Parameter>,
) -> Result<CheckFinding, Error> {
    let id = requirement.id.as_str();
    let Some(numeric) = &requirement.numeric else {
        return Ok(finding(
            vec![requirement.id.clone()],
            Vec::new(),
            Verdict::Unverified,
            format!("{id} has no numeric criterion"),
        ));
    };
    let mut inputs = criterion_quantities(&numeric.criterion)
        .into_iter()
        .map(|(name, q)| input(name, q))
        .collect::<Result<Vec<_>, Error>>()?;
    let Some(parameter) = parameter else {
        return Ok(finding(
            vec![requirement.id.clone()],
            inputs,
            Verdict::Unverified,
            format!(
                "{id}: parameter {} has no value yet",
                numeric.parameter_id.as_str()
            ),
        ));
    };
    let mut value_input = input("value", &parameter.value)?;
    if let ParameterSource::ToolResult { evidence_id } = &parameter.source {
        value_input.source_evidence_id = Some(evidence_id.clone());
    }
    inputs.insert(0, value_input);
    let subject_ids = vec![requirement.id.clone(), parameter.id.clone()];

    let value = &parameter.value;
    let outcome = match compare(value, &numeric.criterion) {
        Ok(outcome) => outcome,
        Err(reason) => {
            return Ok(finding(
                subject_ids,
                inputs,
                Verdict::Unverified,
                format!("{id}: cannot compare: {reason}"),
            ));
        }
    };
    let shown = format!(
        "{} = {} {}",
        parameter.id.as_str(),
        value.value,
        value.unit.as_str()
    );
    let (verdict, message) = if outcome.satisfied {
        (Verdict::Pass, format!("{id}: {shown} {}", outcome.claim))
    } else {
        (
            Verdict::Fail,
            format!("{id}: {shown} is not {}", outcome.claim),
        )
    };
    Ok(finding(subject_ids, inputs, verdict, message))
}

struct Outcome {
    satisfied: bool,
    /// The criterion in words, for example "<= 1 kg".
    claim: String,
}

/// Compares in coherent SI units. Fails with a reason when units are unknown
/// or of different dimensions.
fn compare(value: &Quantity, criterion: &Criterion) -> Result<Outcome, String> {
    let si = |q: &Quantity| -> Result<f64, String> {
        let unit = Unit::parse(q.unit.as_str()).map_err(|e| e.to_string())?;
        let reference = Unit::parse(value.unit.as_str()).map_err(|e| e.to_string())?;
        unit.convert(q.value, &reference)
            .map_err(|e| e.to_string())
            .map(|in_value_unit| reference.to_si(in_value_unit))
    };
    let v = si(value)?;
    let shown = |q: &Quantity| format!("{} {}", q.value, q.unit.as_str());
    Ok(match criterion {
        Criterion::Le { limit } => Outcome {
            satisfied: at_most(v, si(limit)?),
            claim: format!("<= {}", shown(limit)),
        },
        Criterion::Ge { limit } => Outcome {
            satisfied: at_most(si(limit)?, v),
            claim: format!(">= {}", shown(limit)),
        },
        Criterion::Eq { target, tolerance } => Outcome {
            satisfied: at_most((v - si(target)?).abs(), si(tolerance)?.abs()),
            claim: format!("= {} within {}", shown(target), shown(tolerance)),
        },
        Criterion::Within { min, max } => Outcome {
            satisfied: at_most(si(min)?, v) && at_most(v, si(max)?),
            claim: format!("within {} to {}", shown(min), shown(max)),
        },
    })
}

/// `a <= b`, counting values equal within [`RELATIVE_TOLERANCE`] as equal.
fn at_most(a: f64, b: f64) -> bool {
    a <= b || (a - b).abs() <= RELATIVE_TOLERANCE * a.abs().max(b.abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_after_conversion_does_not_flip_a_boundary() {
        assert!(at_most(300.0 * 1e-3, 0.3));
        assert!(at_most(0.3, 300.0 * 1e-3));
        assert!(!at_most(0.300_001, 0.3));
    }
}
