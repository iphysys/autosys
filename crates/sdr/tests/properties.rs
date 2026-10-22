//! Property tests for PR 5's done-when: no version is committed without
//! validation, and stale base versions are rejected.
//!
//! Each case runs a random sequence of transitions, each either valid or
//! carrying one known defect, against a random base version. `validate` must
//! agree with the label, and the repository must end up holding exactly the
//! validated documents, in order. Seeds are fixed, so runs are reproducible
//! (blueprint section 16.3).

#![allow(clippy::expect_used, reason = "integration tests")]

mod support;

use contracts::ids::SdrVersion;
use contracts::types::{Operation, SdrDocument};
use contracts::{ErrorCode, SdrRepository};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use sdr::{ProblemKind, Validation};
use std::collections::HashSet;
use support::*;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Defect {
    None,
    EmptyOperations,
    EmptyRelatedIds,
    NonFiniteValue,
    DuplicateId,
    UnknownElement,
    EvidenceOnDecision,
}

impl Defect {
    /// The problem a defect must produce when the base is the head.
    fn expected(self) -> Option<ProblemKind> {
        match self {
            Defect::None => None,
            Defect::EmptyOperations | Defect::EmptyRelatedIds | Defect::NonFiniteValue => {
                Some(ProblemKind::Schema)
            }
            Defect::DuplicateId => Some(ProblemKind::DuplicateElement),
            Defect::UnknownElement => Some(ProblemKind::UnknownElement),
            Defect::EvidenceOnDecision => Some(ProblemKind::CannotHoldEvidence),
        }
    }

    fn breaks_schema(self) -> bool {
        self.expected() == Some(ProblemKind::Schema)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Base {
    Head,
    /// One below the head; the head itself when the head is version 0.
    Stale,
    Ahead,
}

#[derive(Clone, Copy, Debug)]
struct Step {
    defect: Defect,
    base: Base,
    /// Also update a parameter committed earlier, if there is one.
    update_earlier: bool,
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let defect = prop_oneof![
        4 => Just(Defect::None),
        1 => Just(Defect::EmptyOperations),
        1 => Just(Defect::EmptyRelatedIds),
        1 => Just(Defect::NonFiniteValue),
        1 => Just(Defect::DuplicateId),
        1 => Just(Defect::UnknownElement),
        1 => Just(Defect::EvidenceOnDecision),
    ];
    let base = prop_oneof![4 => Just(Base::Head), 1 => Just(Base::Stale), 1 => Just(Base::Ahead)];
    let step = (defect, base, any::<bool>()).prop_map(|(defect, base, update_earlier)| Step {
        defect,
        base,
        update_earlier,
    });
    proptest::collection::vec(step, 1..12)
}

/// A transition for step `k`: valid operations on fresh ids, plus the defect.
fn build(k: usize, step: Step, committed: &[SdrDocument]) -> Vec<Operation> {
    let (req, par, asm, dec) = (
        format!("req-{k}"),
        format!("par-{k}"),
        format!("asm-{k}"),
        format!("dec-{k}"),
    );
    let mut ops = vec![
        add_requirement(&req),
        set_parameter(&par, k as f64),
        add_assumption(&asm),
        add_decision(&dec, &[&req, &par]),
        link_evidence(&req, &format!("ev-{k}")),
    ];
    let earlier = committed
        .last()
        .and_then(|doc| doc.design.parameters.first())
        .map(|p| p.id.as_str().to_owned());
    if step.update_earlier
        && let Some(earlier) = &earlier
    {
        ops.push(set_parameter(earlier, -1.0));
    }
    match step.defect {
        Defect::None => {}
        Defect::EmptyOperations => ops.clear(),
        Defect::EmptyRelatedIds => ops.push(add_decision(&format!("dec-empty-{k}"), &[])),
        Defect::NonFiniteValue => {
            let mut bad = set_parameter(&format!("par-nan-{k}"), 0.0);
            if let Operation::SetParameter { parameter } = &mut bad {
                parameter.value.value = f64::NAN;
            }
            ops.push(bad);
        }
        Defect::DuplicateId => {
            let existing = committed
                .last()
                .and_then(|doc| doc.requirements.first())
                .map_or(req.clone(), |r| r.id.as_str().to_owned());
            ops.push(add_requirement(&existing));
        }
        Defect::UnknownElement => ops.push(link_evidence(&format!("missing-{k}"), "ev-x")),
        Defect::EvidenceOnDecision => ops.push(link_evidence(&dec, "ev-x")),
    }
    ops
}

fn element_ids(doc: &SdrDocument) -> Vec<&str> {
    doc.requirements
        .iter()
        .map(|r| r.id.as_str())
        .chain(doc.design.parameters.iter().map(|p| p.id.as_str()))
        .chain(doc.design.decisions.iter().map(|d| d.id.as_str()))
        .chain(doc.design.assumptions.iter().map(|a| a.id.as_str()))
        .collect()
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a tokio runtime")
        .block_on(future)
}

fn config(seed: u64) -> Config {
    Config {
        cases: 64,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: None,
        ..Config::default()
    }
}

proptest! {
    #![proptest_config(config(0x5d12_0005))]

    /// Validation agrees with each transition's label; only validated
    /// transitions become versions, and stale or unknown bases never do.
    #[test]
    fn only_validated_transitions_become_versions(plan in steps()) {
        run(async {
            let f = fixture().await;
            // committed[i] is version i + 1, as validate produced it.
            let mut committed: Vec<SdrDocument> = Vec::new();
            for (k, step) in plan.iter().enumerate() {
                let head = committed.len() as u64;
                let base = match step.base {
                    Base::Head => head,
                    Base::Stale if head > 0 => head - 1,
                    Base::Stale => head,
                    Base::Ahead => head + 1,
                };
                let base_kind = if base < head {
                    Some(ProblemKind::StaleBase)
                } else if base > head {
                    Some(ProblemKind::UnknownBase)
                } else {
                    None
                };
                let ops = build(k, *step, &committed);
                let validation = f.validate(transition(&format!("tr-{k}"), base, ops)).await;

                match (validation, base_kind, step.defect) {
                    (Validation::Valid(validated), None, Defect::None) => {
                        let document = validated.document().clone();
                        let record = f
                            .manager
                            .commit(validated, commit_info(&[]), &f.ctx)
                            .await
                            .expect("commit a validated transition");
                        assert_eq!(record.version, SdrVersion(head + 1), "step {k}");
                        committed.push(document);
                    }
                    (Validation::Valid(_), _, _) => {
                        panic!("step {k}: {step:?} on base {base} (head {head}) was accepted")
                    }
                    (Validation::Invalid(invalid), base_kind, defect) => {
                        if let Some(kind) = base_kind {
                            assert!(invalid.has(kind), "step {k}: {step:?}: {:?}", invalid.problems);
                        }
                        let must_find = match base_kind {
                            None => defect.expected(),
                            // With a wrong base nothing is applied; only the
                            // schema is still checked.
                            Some(_) if defect.breaks_schema() => Some(ProblemKind::Schema),
                            Some(_) => None,
                        };
                        if let Some(kind) = must_find {
                            assert!(invalid.has(kind), "step {k}: {step:?}: {:?}", invalid.problems);
                        }
                        assert!(
                            base_kind.is_some() || defect != Defect::None,
                            "step {k}: a valid transition was rejected: {:?}",
                            invalid.problems
                        );
                    }
                }
                let stored_head = f.manager.head(&f.project, &f.ctx).await.expect("head");
                assert_eq!(stored_head, Some(SdrVersion(committed.len() as u64)), "step {k}");
            }

            for (i, expected) in committed.iter().enumerate() {
                let version = SdrVersion(i as u64 + 1);
                let stored = f.repository.document(&f.project, &version).await.expect("stored");
                assert_eq!(&stored, expected, "version {} is not what was validated", version.0);
                let record = f.repository.version_record(&f.project, &version).await.expect("record");
                assert_eq!(record.parent_version, Some(SdrVersion(i as u64)));
                let json = serde_json::to_value(&stored).expect("serialize");
                f.manager.upgrade_document(json).expect("every stored version satisfies the schema");
                let ids = element_ids(&stored);
                let unique: HashSet<&str> = ids.iter().copied().collect();
                assert_eq!(unique.len(), ids.len(), "version {}: duplicate element ids", version.0);
            }
        });
    }

    /// Two transitions validated on the same head: the first commits, the
    /// second is refused as stale, at commit and on revalidation.
    #[test]
    fn a_transition_validated_on_an_old_head_cannot_commit(prior in 0usize..4) {
        run(async {
            let f = fixture().await;
            for k in 0..prior {
                let validation = f
                    .validate(transition(&format!("tr-{k}"), k as u64, vec![add_requirement(&format!("r{k}"))]))
                    .await;
                let Validation::Valid(validated) = validation else { panic!("setup transition {k} invalid") };
                f.manager.commit(validated, commit_info(&[]), &f.ctx).await.expect("setup commit");
            }
            let head = prior as u64;
            let first = f.validate(transition("tr-a", head, vec![add_requirement("ra")])).await;
            let second = f.validate(transition("tr-b", head, vec![add_requirement("rb")])).await;
            let (Validation::Valid(first), Validation::Valid(second)) = (first, second) else {
                panic!("both transitions should validate on the head")
            };
            f.manager.commit(first, commit_info(&[]), &f.ctx).await.expect("first commit");
            let late = f.manager.commit(second, commit_info(&[]), &f.ctx).await;
            assert_eq!(late.map(|_| ()).unwrap_err().code(), ErrorCode::Conflict);
            assert_eq!(f.manager.head(&f.project, &f.ctx).await.expect("head"), Some(SdrVersion(head + 1)));

            let again = f.validate(transition("tr-b", head, vec![add_requirement("rb")])).await;
            let Validation::Invalid(invalid) = again else { panic!("a stale base validated") };
            assert!(invalid.has(ProblemKind::StaleBase), "{:?}", invalid.problems);
        });
    }
}
