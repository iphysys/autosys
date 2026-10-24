//! The gateway and the routing over fake providers: profile resolution,
//! the refusal of a profile without tool calling for an agent with tools,
//! missing variables, unregistered providers, and calls. No real provider is
//! involved, so these tests hold whichever providers are registered.

use std::sync::Arc;

use contracts::ErrorCode;
use contracts::types::ModelRouting;
use serde_json::{Value, json};
use testkit::builders::{agent_spec as agent, id, parse};

use super::ModelGateway;
use crate::registry::ProviderRegistry;
use crate::routing::{PROFILE_OVERRIDE_ENV, Routing};
use crate::test_support::{
    FAKE_KEY_ENV, FakeFactory, OTHER_FAKE_KEY_ENV, ctx, env, fake_profile, fake_registry,
    fake_routing, profile_of, request,
};

/// `main` calls tools, `chat` does not; `main` is the default.
fn routing() -> Routing {
    fake_routing(
        "main",
        vec![fake_profile("main", true), fake_profile("chat", false)],
        json!([{ "model": "main-model", "input_usd_per_mtok": 1.0, "output_usd_per_mtok": 2.0 }]),
    )
}

fn key() -> [(&'static str, &'static str); 1] {
    [(FAKE_KEY_ENV, "key")]
}

#[test]
fn a_profile_without_tool_calling_is_refused_for_an_agent_with_tools_that_names_it() {
    let error = ModelGateway::start(
        routing(),
        &[agent(
            "designer",
            Some("chat"),
            &["create_plate", "measure"],
        )],
        &env(&key()),
    )
    .err()
    .expect("refused");
    assert_eq!(error.code(), ErrorCode::InvalidInput);
    let message = error.message();
    assert!(message.contains("agent `designer` has tools"), "{message}");
    assert!(message.contains("`chat`"), "{message}");
    assert!(message.contains("tool_calling: false"), "{message}");
}

#[test]
fn the_override_cannot_give_an_agent_with_tools_a_profile_without_tool_calling() {
    let vars = env(&[(PROFILE_OVERRIDE_ENV, "chat"), key()[0]]);
    let error = ModelGateway::start(
        routing(),
        &[agent("designer", None, &["create_plate"])],
        &vars,
    )
    .err()
    .expect("refused");
    assert!(error.message().contains("tool_calling: false"), "{error}");
    assert!(
        routing()
            .check_agent(&agent("designer", Some("chat"), &["create_plate"]))
            .is_err()
    );
}

#[test]
fn an_agent_without_tools_may_use_a_profile_without_tool_calling() {
    let vars = env(&[(PROFILE_OVERRIDE_ENV, "chat"), key()[0]]);
    let gateway =
        ModelGateway::start(routing(), &[agent("writer", None, &[])], &vars).expect("starts");
    assert_eq!(gateway.default_profile().as_str(), "chat");
    assert_eq!(
        gateway
            .agent_profile(&id("writer"))
            .expect("resolved")
            .as_str(),
        "chat"
    );
    assert_eq!(
        gateway
            .agent_profile(&id("stranger"))
            .err()
            .map(|e| e.code()),
        Some(ErrorCode::NotFound)
    );
}

#[test]
fn agents_without_a_profile_get_the_default_and_others_keep_theirs() {
    let gateway = ModelGateway::start(
        routing(),
        &[
            agent("designer", None, &["create_plate"]),
            agent("writer", Some("chat"), &[]),
        ],
        &env(&key()),
    )
    .expect("starts");
    assert_eq!(gateway.default_profile().as_str(), "main");
    assert_eq!(
        gateway
            .agent_profile(&id("designer"))
            .expect("resolved")
            .as_str(),
        "main"
    );
    assert_eq!(
        gateway
            .agent_profile(&id("writer"))
            .expect("resolved")
            .as_str(),
        "chat"
    );
}

#[test]
fn a_missing_or_blank_variable_fails_at_startup_naming_it() {
    for vars in [env(&[]), env(&[(FAKE_KEY_ENV, " ")])] {
        let error = ModelGateway::start(routing(), &[], &vars)
            .err()
            .expect("no key");
        assert_eq!(error.code(), ErrorCode::InvalidInput);
        assert!(
            error.message().contains(&format!(
                "model profile `main`: environment variable {FAKE_KEY_ENV} is not set"
            )),
            "{error}"
        );
    }
}

#[test]
fn profiles_not_in_use_need_no_variables() {
    let routing = fake_routing(
        "main",
        vec![
            fake_profile("main", true),
            profile_of("other_fake", "other", true),
        ],
        json!([]),
    );
    assert!(ModelGateway::start(routing.clone(), &[], &env(&key())).is_ok());
    let error = ModelGateway::start(routing, &[agent("a", Some("other"), &[])], &env(&key()))
        .err()
        .expect("other is in use");
    assert!(error.message().contains(OTHER_FAKE_KEY_ENV), "{error}");
}

#[test]
fn an_unknown_override_lists_the_profiles() {
    let vars = env(&[(PROFILE_OVERRIDE_ENV, "gpt"), key()[0]]);
    let error = ModelGateway::start(routing(), &[], &vars)
        .err()
        .expect("unknown");
    let message = error.message();
    assert!(message.contains(PROFILE_OVERRIDE_ENV), "{message}");
    assert!(message.contains("main, chat"), "{message}");
}

fn config(value: Value) -> ModelRouting {
    parse(value)
}

#[test]
fn an_unregistered_provider_is_rejected_listing_the_registered_ones() {
    let mut profile = fake_profile("main", true);
    profile["provider"] = json!("removed_provider");
    let error = Routing::new(
        config(json!({
            "schema_version": "0.2", "default_profile": "main", "profiles": [profile], "prices": []
        })),
        fake_registry(),
    )
    .expect_err("unregistered");
    let message = error.message();
    assert!(
        message.contains("`removed_provider` is not registered"),
        "{message}"
    );
    assert!(
        message.contains("registered: fake, other_fake"),
        "{message}"
    );
}

#[test]
fn settings_for_a_provider_without_settings_are_rejected() {
    let mut profile = fake_profile("main", true);
    profile["settings"] = json!({ "api_key_env": "KEY" });
    let error = Routing::new(
        config(json!({
            "schema_version": "0.2", "default_profile": "main", "profiles": [profile], "prices": []
        })),
        fake_registry(),
    )
    .expect_err("no settings allowed");
    assert!(error.message().contains("takes no settings"), "{error}");
}

#[test]
fn duplicates_and_an_unknown_default_are_rejected() {
    let error = Routing::new(
        config(json!({
            "schema_version": "0.2",
            "default_profile": "missing",
            "profiles": [fake_profile("main", true), fake_profile("main", false)],
            "prices": [
                { "model": "m", "input_usd_per_mtok": 1.0, "output_usd_per_mtok": 1.0 },
                { "model": "m", "input_usd_per_mtok": 2.0, "output_usd_per_mtok": 2.0 }
            ]
        })),
        fake_registry(),
    )
    .expect_err("inconsistent routing");
    let message = error.message();
    assert!(
        message.contains("profile `main` is defined twice"),
        "{message}"
    );
    assert!(message.contains("default_profile `missing`"), "{message}");
    assert!(message.contains("model `m` is priced twice"), "{message}");
}

#[test]
fn a_provider_registered_twice_is_rejected() {
    let twice = ProviderRegistry::new(vec![
        Arc::new(FakeFactory {
            name: "fake",
            key_env: FAKE_KEY_ENV,
        }),
        Arc::new(FakeFactory {
            name: "fake",
            key_env: OTHER_FAKE_KEY_ENV,
        }),
    ]);
    assert!(
        twice
            .err()
            .is_some_and(|e| e.message().contains("`fake` is registered twice"))
    );
}

#[tokio::test]
async fn a_call_uses_the_profile_model_and_is_costed() {
    let gateway = ModelGateway::start(routing(), &[], &env(&key())).expect("starts");
    let mut hi = request("set-by-the-gateway", "Hi.");
    hi.max_output_tokens = 100;
    let call = gateway
        .complete(&id("main"), hi, &ctx())
        .await
        .expect("scripted answer");
    assert_eq!(call.profile.as_str(), "main");
    assert_eq!(call.attempts, 1);
    // The scripted response reports 10 input and 5 output tokens of
    // "scripted-model", which has no price.
    assert_eq!(call.cost_usd, None);
}

#[tokio::test]
async fn the_gateway_refuses_more_output_tokens_than_the_profile_allows() {
    let gateway = ModelGateway::start(routing(), &[], &env(&key())).expect("starts");
    let mut too_long = request("m", "Hi.");
    too_long.max_output_tokens = 1001;
    let error = gateway
        .complete(&id("main"), too_long, &ctx())
        .await
        .expect_err("over the profile limit");
    assert_eq!(error.code(), ErrorCode::InvalidInput);

    let unknown = gateway
        .complete(&id("chat"), request("m", "Hi."), &ctx())
        .await
        .expect_err("chat is not in use");
    assert_eq!(unknown.code(), ErrorCode::NotFound);
}
