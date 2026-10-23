//! Deterministic checks of an SDR document (blueprint section 16.2). Each
//! returns findings; the [`crate::Evaluator`] turns them into evidence.

mod requirements;
mod schema;
mod units;

pub use requirements::NumericThresholdCheck;
pub use schema::SchemaCheck;
pub use units::UnitsCheck;

use contracts::ids::{CheckId, ElementId};
use contracts::types::{CheckInput, CheckMethod, Quantity, SdrDocument, SemVer, Verdict};
use contracts::{CheckDescriptor, CheckFinding, CheckSubject, Error, ErrorCode};

use crate::units::Unit;

fn descriptor(id: &str, version: &str) -> Result<CheckDescriptor, Error> {
    let internal = |what: &str| Error::new(ErrorCode::Internal, format!("invalid check {what}"));
    Ok(CheckDescriptor {
        id: id.parse::<CheckId>().map_err(|_| internal("id"))?,
        version: version.parse::<SemVer>().map_err(|_| internal("version"))?,
        method: CheckMethod::Deterministic,
    })
}

/// The document a document check evaluates. These checks judge what would be
/// stored, so a bare transition is not a valid subject.
fn document<'a>(subject: CheckSubject<'a>, check: &str) -> Result<&'a SdrDocument, Error> {
    match subject {
        CheckSubject::Document(doc) => Ok(doc),
        CheckSubject::Transition { .. } => Err(Error::new(
            ErrorCode::InvalidInput,
            format!("the {check} check evaluates documents, not transitions"),
        )),
    }
}

/// A check input: the quantity as received and, when its unit is known, the
/// same value in coherent SI units.
fn input(name: &str, quantity: &Quantity) -> Result<CheckInput, Error> {
    let normalized = match Unit::parse(quantity.unit.as_str()) {
        Ok(unit) => Some(si_quantity(unit.to_si(quantity.value), &unit)?),
        Err(_) => None,
    };
    Ok(CheckInput {
        name: name
            .parse()
            .map_err(|_| Error::new(ErrorCode::Internal, format!("invalid input name {name}")))?,
        quantity: quantity.clone(),
        normalized,
        source_evidence_id: None,
    })
}

fn si_quantity(value: f64, unit: &Unit) -> Result<Quantity, Error> {
    Ok(Quantity {
        value,
        unit: unit
            .dimension()
            .si_code()
            .parse()
            .map_err(|_| Error::new(ErrorCode::Internal, "SI unit code is not a valid unit"))?,
    })
}

fn finding(
    subject_ids: Vec<ElementId>,
    inputs: Vec<CheckInput>,
    verdict: Verdict,
    message: String,
) -> CheckFinding {
    CheckFinding {
        subject_ids,
        inputs,
        verdict,
        message: Some(message),
    }
}
