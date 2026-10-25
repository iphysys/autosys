//! Startup checks that no single component makes on its own. Each failure
//! stops the worker before it claims anything.

use std::collections::BTreeSet;
use std::sync::Arc;

use contracts::Check;
use contracts::types::GatesPolicy;
use model_gateway::Env;

use crate::BootstrapError;
use crate::settings::DATABASE_URL_ENV;

/// The checks registered with the evaluator must be exactly the ones the
/// `sdr_commit` gate requires. A required check that is not registered
/// could never pass. A registered check that is not required would make the
/// evaluation of a commit differ from the evidence its version records,
/// which is what a run recovered after that commit reads (ADR-0031).
pub(crate) fn registered_checks_are_required(
    checks: &[Arc<dyn Check>],
    gates: &GatesPolicy,
) -> Result<(), BootstrapError> {
    let registered: BTreeSet<&str> = checks.iter().map(|c| c.descriptor().id.as_str()).collect();
    let required: BTreeSet<&str> = gates
        .sdr_commit
        .required_checks
        .iter()
        .map(|c| c.as_str())
        .collect();
    let mut problems = Vec::new();
    let missing: Vec<&str> = required.difference(&registered).copied().collect();
    if !missing.is_empty() {
        problems.push(format!(
            "config/policies/gates.yaml requires checks that are not registered: {}",
            missing.join(", ")
        ));
    }
    let unrequired: Vec<&str> = registered.difference(&required).copied().collect();
    if !unrequired.is_empty() {
        problems.push(format!(
            "registered checks that config/policies/gates.yaml does not require: {}",
            unrequired.join(", ")
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(BootstrapError::new("checks", problems.join("; ")))
    }
}

/// The database URL. Its value is never logged or put in an error.
pub(crate) fn database_url(env: &dyn Env) -> Result<String, BootstrapError> {
    env.var(DATABASE_URL_ENV).ok_or_else(|| {
        BootstrapError::new(
            "environment",
            format!("{DATABASE_URL_ENV} is not set (see .env.example)"),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use contracts::types::GatesPolicy;
    use evaluation::checks::{NumericThresholdCheck, SchemaCheck, UnitsCheck};

    use super::*;

    /// A gates policy that requires `required`, through the config loader.
    fn gates(required: &[&str]) -> GatesPolicy {
        let text = format!(
            "{{\"schema_version\":\"0.1\",\"sdr_commit\":{{\"required_checks\":{required:?}}}}}"
        );
        infra::config::ConfigLoader::new(contracts::Environment::Test)
            .and_then(|loader| loader.parse(&text, "gates.yaml"))
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn all_checks() -> Vec<Arc<dyn Check>> {
        vec![
            Arc::new(SchemaCheck::new().unwrap_or_else(|e| panic!("{e}"))),
            Arc::new(UnitsCheck::new().unwrap_or_else(|e| panic!("{e}"))),
            Arc::new(NumericThresholdCheck::new().unwrap_or_else(|e| panic!("{e}"))),
        ]
    }

    #[test]
    fn the_committed_checks_match_the_committed_gate() {
        let committed = gates(&["schema", "units", "requirement.numeric_threshold"]);
        assert!(registered_checks_are_required(&all_checks(), &committed).is_ok());
    }

    #[test]
    fn a_registered_check_the_gate_does_not_require_fails_startup() {
        let error = registered_checks_are_required(&all_checks(), &gates(&["schema", "units"]))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            error.contains("does not require: requirement.numeric_threshold"),
            "{error}"
        );
    }

    #[test]
    fn a_required_check_that_is_not_registered_fails_startup() {
        let error = registered_checks_are_required(
            &all_checks()[..2],
            &gates(&["schema", "units", "requirement.numeric_threshold"]),
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(
            error.contains("not registered: requirement.numeric_threshold"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_database_url_fails_startup_without_a_value() {
        let error = database_url(&HashMap::new())
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("DATABASE_URL is not set"), "{error}");
    }
}
