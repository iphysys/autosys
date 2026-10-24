//! Token estimates and the context profiles that bound them (ADR-0029).
//!
//! There is no tokenizer. A count is an estimate: UTF-8 bytes divided by the
//! profile's `bytes_per_token`, rounded up. The estimate sits behind
//! [`TokenCounter`] so a real tokenizer can replace it, and the
//! `context_assembled` and `model_call_completed` events of real runs show
//! how far off it is.

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use contracts::types::{ContextBudgets, ContextPolicy, ContextProfile, ContextSectionName};

use crate::error::ContextError;
use crate::render::ORDER;

/// The profile a call uses when it names none.
pub const DEFAULT_PROFILE: &str = "default";

pub trait TokenCounter: Send + Sync {
    /// The number of tokens `text` is estimated to use.
    fn count(&self, text: &str) -> u64;
}

/// `ceil(utf8 bytes / bytes_per_token)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteEstimate {
    bytes_per_token: NonZeroU64,
}

impl ByteEstimate {
    pub fn new(bytes_per_token: NonZeroU64) -> Self {
        Self { bytes_per_token }
    }

    pub fn for_profile(profile: &ContextProfile) -> Self {
        Self::new(profile.bytes_per_token)
    }
}

impl TokenCounter for ByteEstimate {
    fn count(&self, text: &str) -> u64 {
        u64::try_from(text.len())
            .unwrap_or(u64::MAX)
            .div_ceil(self.bytes_per_token.get())
    }
}

/// Sections whose content is never cut: one that does not fit fails the build.
/// The open turn is fixed too, but it may exceed its own cap by borrowing
/// from the trimmable sections.
pub(crate) const fn is_fixed(section: ContextSectionName) -> bool {
    match section {
        ContextSectionName::Instructions
        | ContextSectionName::TaskContract
        | ContextSectionName::JobState
        | ContextSectionName::DesignState
        | ContextSectionName::OpenTurn => true,
        ContextSectionName::RunHistory | ContextSectionName::UntrustedContent => false,
    }
}

/// The profile's cap for `section`.
pub(crate) fn cap(profile: &ContextProfile, section: ContextSectionName) -> u64 {
    let sections = &profile.sections;
    match section {
        ContextSectionName::Instructions => sections.instructions,
        ContextSectionName::TaskContract => sections.task_contract,
        ContextSectionName::JobState => sections.job_state,
        ContextSectionName::DesignState => sections.design_state,
        ContextSectionName::RunHistory => sections.run_history,
        ContextSectionName::UntrustedContent => sections.untrusted_content,
        ContextSectionName::OpenTurn => sections.open_turn,
    }
    .get()
}

/// The caps of all fixed sections together.
fn fixed_caps(profile: &ContextProfile) -> u64 {
    ORDER
        .into_iter()
        .filter(|s| is_fixed(*s))
        .fold(0u64, |sum, s| sum.saturating_add(cap(profile, s)))
}

/// The profiles of `config/context.yaml`, checked against the ceiling of
/// `config/policies/context.yaml`.
#[derive(Clone, Debug)]
pub struct ContextProfiles {
    profiles: BTreeMap<String, ContextProfile>,
    ceiling: NonZeroU64,
}

impl ContextProfiles {
    /// Fails with every problem found:
    /// - a profile name used twice, or no profile named `default`;
    /// - a profile whose fixed caps together exceed its total;
    /// - a profile whose total exceeds the policy's ceiling.
    pub fn new(budgets: ContextBudgets, policy: &ContextPolicy) -> Result<Self, ContextError> {
        let ceiling = policy.max_total_tokens;
        let mut problems = Vec::new();
        let mut profiles = BTreeMap::new();
        for profile in budgets.profiles {
            let name = profile.name.as_str().to_owned();
            if fixed_caps(&profile) > profile.total_tokens.get() {
                let fixed: Vec<String> = ORDER
                    .into_iter()
                    .filter(|s| is_fixed(*s))
                    .map(|s| s.to_string())
                    .collect();
                problems.push(format!(
                    "profile `{name}`: the fixed sections ({}) are capped at {} tokens together, \
                     above its total_tokens {}",
                    fixed.join(", "),
                    fixed_caps(&profile),
                    profile.total_tokens
                ));
            }
            if profile.total_tokens > ceiling {
                problems.push(format!(
                    "profile `{name}`: total_tokens {} exceeds the ceiling {ceiling} \
                     (config/policies/context.yaml)",
                    profile.total_tokens
                ));
            }
            if profiles.insert(name.clone(), profile).is_some() {
                problems.push(format!("profile `{name}` is defined more than once"));
            }
        }
        if !profiles.contains_key(DEFAULT_PROFILE) {
            problems.push(format!("there is no profile named `{DEFAULT_PROFILE}`"));
        }
        if problems.is_empty() {
            Ok(Self { profiles, ceiling })
        } else {
            Err(ContextError::InvalidProfile { problems })
        }
    }

    /// The profile called `name`, or `default` when `name` is `None`.
    pub fn resolve(&self, name: Option<&str>) -> Result<&ContextProfile, ContextError> {
        let name = name.unwrap_or(DEFAULT_PROFILE);
        self.profiles
            .get(name)
            .ok_or_else(|| ContextError::UnknownProfile {
                name: name.to_owned(),
                available: self.names().map(str::to_owned).collect(),
            })
    }

    /// Profile names in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.profiles.keys().map(String::as_str)
    }

    pub fn ceiling(&self) -> u64 {
        self.ceiling.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: u64) -> NonZeroU64 {
        NonZeroU64::new(n).expect("non-zero")
    }

    #[test]
    fn the_estimate_rounds_up() {
        let three = ByteEstimate::new(nz(3));
        assert_eq!(three.count(""), 0);
        assert_eq!(three.count("a"), 1);
        assert_eq!(three.count("abc"), 1);
        assert_eq!(three.count("abcd"), 2);
        assert_eq!(three.count("abcdefghi"), 3);
        assert_eq!(three.count("abcdefghij"), 4);
        assert_eq!(ByteEstimate::new(nz(1)).count("abcd"), 4);
    }

    #[test]
    fn the_estimate_counts_bytes_not_characters() {
        // "é" is 2 bytes and "€" 3, so a character is not a token.
        let three = ByteEstimate::new(nz(3));
        assert_eq!(three.count("é"), 1);
        assert_eq!(three.count("ééé"), 2);
        assert_eq!(three.count("€€"), 2);
    }

    #[test]
    fn fixed_sections_are_the_five_that_are_never_cut() {
        let fixed: Vec<_> = ORDER.into_iter().filter(|s| is_fixed(*s)).collect();
        assert_eq!(
            fixed,
            [
                ContextSectionName::Instructions,
                ContextSectionName::TaskContract,
                ContextSectionName::JobState,
                ContextSectionName::DesignState,
                ContextSectionName::OpenTurn,
            ]
        );
    }
}
