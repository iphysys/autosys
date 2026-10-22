use contracts::types::CheckMethod;
use contracts::{Check, CheckSubject, ErrorCode};

use super::{assert_code, cancelled_ctx, ctx};

/// The `Check` contract, evaluated on `subject`.
pub async fn check(check: &dyn Check, subject: CheckSubject<'_>) {
    let name = check.descriptor().id.as_str().to_owned();
    let first = check
        .evaluate(subject, &ctx())
        .await
        .unwrap_or_else(|e| panic!("check {name}: evaluation failed: {e}"));
    if check.descriptor().method == CheckMethod::Deterministic {
        let second = check
            .evaluate(subject, &ctx())
            .await
            .unwrap_or_else(|e| panic!("check {name}: evaluation failed: {e}"));
        assert_eq!(
            first, second,
            "check {name}: deterministic check changed its findings"
        );
    }
    assert_code(
        check.evaluate(subject, &cancelled_ctx()).await,
        ErrorCode::Cancelled,
        "check: cancelled context",
    );
}
