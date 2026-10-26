//! Cost of a call from the prices in `config/models/routing.yaml`.

use crate::providers::ModelUsage;
use crate::routing::Routing;

const TOKENS_PER_MTOK: f64 = 1_000_000.0;

/// The cost in USD of the attempts in `usage`, each at its own model's price.
/// `None` if any of those models has no price: an unknown cost is never
/// reported as zero.
pub fn cost_usd(routing: &Routing, usage: &[ModelUsage]) -> Option<f64> {
    usage.iter().try_fold(0.0, |total, attempt| {
        let price = routing.price(&attempt.model)?;
        // Token counts stay far below 2^52, so the conversion is exact.
        #[allow(clippy::cast_precision_loss)]
        let (input, output) = (
            attempt.usage.input_tokens as f64,
            attempt.usage.output_tokens as f64,
        );
        Some(
            total
                + input * price.input_usd_per_mtok / TOKENS_PER_MTOK
                + output * price.output_usd_per_mtok / TOKENS_PER_MTOK,
        )
    })
}

#[cfg(test)]
mod tests {
    use contracts::types::TokenUsage;
    use serde_json::json;

    use super::*;
    use crate::test_support::{fake_profile, fake_routing};

    fn routing() -> Routing {
        fake_routing(
            "main",
            vec![fake_profile("main", true)],
            json!([
                { "model": "example-large", "input_usd_per_mtok": 4.0, "output_usd_per_mtok": 20.0 },
                { "model": "example-larger", "input_usd_per_mtok": 5.0, "output_usd_per_mtok": 25.0 }
            ]),
        )
    }

    fn usage(model: &str, input_tokens: u64, output_tokens: u64) -> ModelUsage {
        ModelUsage {
            model: model.to_owned(),
            usage: TokenUsage {
                input_tokens,
                output_tokens,
            },
        }
    }

    #[test]
    fn each_attempt_is_priced_at_its_own_model() {
        let cost = cost_usd(
            &routing(),
            &[
                usage("example-large", 1_000_000, 0),
                usage("example-larger", 0, 1_000_000),
            ],
        )
        .expect("both models are priced");
        assert!((cost - 29.0).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn an_unpriced_model_makes_the_cost_unknown() {
        assert_eq!(
            cost_usd(
                &routing(),
                &[usage("example-large", 10, 10), usage("unpriced", 10, 10)]
            ),
            None
        );
    }
}
