use contracts::{ErrorCode, Gate, GateRequest};

use super::{assert_code, cancelled_ctx, ctx};

/// The `Gate` contract. `request` must be of the gate's subject kind.
pub async fn gate(gate: &dyn Gate, request: GateRequest<'_>) {
    let name = gate.id().as_str().to_owned();
    assert_eq!(
        request.subject.kind(),
        gate.subject_kind(),
        "gate suite precondition: gate {name} does not decide this kind of subject"
    );
    let first = gate
        .decide(request.clone(), &ctx())
        .await
        .unwrap_or_else(|e| panic!("gate {name}: decision failed: {e}"));
    let second = gate
        .decide(request.clone(), &ctx())
        .await
        .unwrap_or_else(|e| panic!("gate {name}: decision failed: {e}"));
    assert_eq!(
        first, second,
        "gate {name}: same request, different decisions"
    );
    assert_code(
        gate.decide(request, &cancelled_ctx()).await,
        ErrorCode::Cancelled,
        "gate: cancelled context",
    );
}
