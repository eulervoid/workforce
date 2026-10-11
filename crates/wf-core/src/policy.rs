//! The workforce policy: item types, lanes and assignment. Stored verbatim as TOML in the log.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::identity::Role;
use crate::name::{LaneName, Requirement, ReviewName, StateName, TypeName};

pub const DEFAULT_POLICY_TOML: &str = include_str!("default_policy.toml");

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("invalid policy: {0}")]
    Parse(String),
    #[error("built-in type `{0}` is missing")]
    BuiltinMissing(TypeName),
    #[error("states of built-in type `{0}` cannot be changed")]
    BuiltinStatesChanged(TypeName),
    #[error("type `{0}` has no states")]
    NoStates(TypeName),
    #[error("type `{type_name}` lists state `{state}` twice")]
    DuplicateState {
        type_name: TypeName,
        state: StateName,
    },
    #[error("initial state `{state}` of type `{type_name}` is not one of its states")]
    UnknownInitialState {
        type_name: TypeName,
        state: StateName,
    },
    #[error("type `{0}` has an active state but no open state to return to")]
    ActiveWithoutOpen(TypeName),
    #[error("type `{type_name}` may depend on unknown type `{target}`")]
    UnknownDependencyTarget {
        type_name: TypeName,
        target: TypeName,
    },
    #[error("review level `{0}` is defined twice")]
    DuplicateReviewLevel(ReviewName),
    #[error("review level `{0}` has no steps")]
    NoReviewSteps(ReviewName),
    #[error("type `{type_name}` has the unknown review level `{level}`")]
    UnknownReviewLevel {
        type_name: TypeName,
        level: ReviewName,
    },
    #[error("type `{0}` has a review level but no blocked state to wait in")]
    ReviewWithoutBlocked(TypeName),
}

/// Fixed set of behavioural state categories. Built-in behaviour keys on these, never on state names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateCategory {
    Inactive,
    Open,
    Active,
    Blocked,
    Done,
    Cancelled,
}

impl StateCategory {
    pub fn is_terminal(self) -> bool {
        match self {
            StateCategory::Done | StateCategory::Cancelled => true,
            StateCategory::Inactive
            | StateCategory::Open
            | StateCategory::Active
            | StateCategory::Blocked => false,
        }
    }
}

impl std::fmt::Display for StateCategory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            StateCategory::Inactive => "inactive",
            StateCategory::Open => "open",
            StateCategory::Active => "active",
            StateCategory::Blocked => "blocked",
            StateCategory::Done => "done",
            StateCategory::Cancelled => "cancelled",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateConfig {
    pub name: StateName,
    #[serde(rename = "category")]
    pub state_category: StateCategory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Priority,
    Labels,
    Requires,
}

/// Which transitions between states are allowed. Explicit lists and guards come later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transitions {
    #[default]
    Any,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeConfig {
    pub lane: LaneName,
    #[serde(default)]
    pub fields: Vec<Field>,
    #[serde(default)]
    pub depends_on: Vec<TypeName>,
    pub initial: StateName,
    #[serde(default)]
    pub transitions: Transitions,
    pub states: Vec<StateConfig>,
    /// The review level of items of this type that do not name one.
    #[serde(default)]
    pub review: Option<ReviewName>,
}

impl TypeConfig {
    /// The first state with the category. The runner moves items to these states.
    pub fn first_state(&self, state_category: StateCategory) -> Option<&StateName> {
        self.states
            .iter()
            .find(|state| state.state_category == state_category)
            .map(|state| &state.name)
    }

    pub fn state_category_of(&self, state: &StateName) -> Option<StateCategory> {
        self.states
            .iter()
            .find(|candidate| &candidate.name == state)
            .map(|candidate| candidate.state_category)
    }

    pub fn allows_field(&self, field: Field) -> bool {
        self.fields.contains(&field)
    }

    pub fn allows_dependency_on(&self, target: &TypeName) -> bool {
        self.depends_on.contains(target)
    }
}

/// Stored, not acted on in M0.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneConfig {
    #[serde(default)]
    pub claim: Vec<Role>,
    #[serde(default)]
    pub types: Vec<TypeName>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentMode {
    #[default]
    Dispatch,
    Pull,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fallback {
    #[default]
    Optimistic,
    Wait,
}

/// A claim lease. Written as `30m` in TOML.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease(pub Duration);

impl Default for Lease {
    fn default() -> Self {
        Lease(Duration::from_secs(30 * 60))
    }
}

impl Serialize for Lease {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.collect_str(&humantime::format_duration(self.0))
        } else {
            self.0.as_secs().serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for Lease {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let text = String::deserialize(deserializer)?;
            humantime::parse_duration(&text)
                .map(Lease)
                .map_err(serde::de::Error::custom)
        } else {
            u64::deserialize(deserializer).map(|secs| Lease(Duration::from_secs(secs)))
        }
    }
}

/// Stored, not acted on in M0.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentConfig {
    #[serde(default)]
    pub mode: AssignmentMode,
    #[serde(default)]
    pub fallback: Fallback,
    #[serde(default)]
    pub lease: Lease,
}

/// How reviewed work gets into the target branch. `origin` (push, pull requests) comes in M3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    #[default]
    Local,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeConfig {
    #[serde(default)]
    pub mode: MergeMode,
    /// The branch approved work is merged into.
    #[serde(default = "MergeConfig::default_target")]
    pub target: String,
}

impl MergeConfig {
    fn default_target() -> String {
        "main".to_string()
    }
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            mode: MergeMode::default(),
            target: Self::default_target(),
        }
    }
}

/// One step of a review level: an agent chosen by capabilities, or the human.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStep {
    Agent { requires: BTreeSet<Requirement> },
    Human,
}

/// A named sequence of review steps. Levels are listed from least to most strict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewLevelConfig {
    pub name: ReviewName,
    pub steps: Vec<ReviewStep>,
}

/// Unknown top-level tables (`[transcripts]`, ...) are kept in the stored text but ignored here
/// until a later milestone gives them meaning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub types: BTreeMap<TypeName, TypeConfig>,
    #[serde(default)]
    pub lanes: BTreeMap<LaneName, LaneConfig>,
    #[serde(default)]
    pub assignment: AssignmentConfig,
    #[serde(default)]
    pub merge: MergeConfig,
    /// Ordered from least to most strict: the position is what lowering and raising mean.
    #[serde(default)]
    pub review: Vec<ReviewLevelConfig>,
}

static DEFAULT_POLICY: LazyLock<Policy> = LazyLock::new(|| {
    toml::from_str(DEFAULT_POLICY_TOML).expect("the bundled default policy is valid TOML")
});

impl Policy {
    pub fn default_policy() -> &'static Policy {
        &DEFAULT_POLICY
    }

    pub fn builtin_type_names() -> impl Iterator<Item = &'static TypeName> {
        DEFAULT_POLICY.types.keys()
    }

    /// Parses and validates policy text.
    pub fn parse(source: &str) -> Result<Policy, PolicyError> {
        let policy: Policy =
            toml::from_str(source).map_err(|error| PolicyError::Parse(error.to_string()))?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn type_config(&self, type_name: &TypeName) -> Option<&TypeConfig> {
        self.types.get(type_name)
    }

    pub fn review_level(&self, name: &ReviewName) -> Option<&ReviewLevelConfig> {
        self.review.iter().find(|level| &level.name == name)
    }

    /// How strict a level is: its position in the list.
    pub fn review_rank(&self, name: &ReviewName) -> Option<usize> {
        self.review.iter().position(|level| &level.name == name)
    }

    fn validate(&self) -> Result<(), PolicyError> {
        let mut levels = BTreeSet::new();
        for level in &self.review {
            if !levels.insert(&level.name) {
                return Err(PolicyError::DuplicateReviewLevel(level.name.clone()));
            }
            if level.steps.is_empty() {
                return Err(PolicyError::NoReviewSteps(level.name.clone()));
            }
        }
        for (name, builtin) in &DEFAULT_POLICY.types {
            let configured = self
                .types
                .get(name)
                .ok_or_else(|| PolicyError::BuiltinMissing(name.clone()))?;
            if configured.states != builtin.states {
                return Err(PolicyError::BuiltinStatesChanged(name.clone()));
            }
        }
        for (name, config) in &self.types {
            if config.states.is_empty() {
                return Err(PolicyError::NoStates(name.clone()));
            }
            let mut seen = BTreeSet::new();
            for state in &config.states {
                if !seen.insert(&state.name) {
                    return Err(PolicyError::DuplicateState {
                        type_name: name.clone(),
                        state: state.name.clone(),
                    });
                }
            }
            if config.state_category_of(&config.initial).is_none() {
                return Err(PolicyError::UnknownInitialState {
                    type_name: name.clone(),
                    state: config.initial.clone(),
                });
            }
            let has = |state_category| {
                config
                    .states
                    .iter()
                    .any(|state| state.state_category == state_category)
            };
            if has(StateCategory::Active) && !has(StateCategory::Open) {
                return Err(PolicyError::ActiveWithoutOpen(name.clone()));
            }
            if let Some(level) = &config.review {
                if self.review_level(level).is_none() {
                    return Err(PolicyError::UnknownReviewLevel {
                        type_name: name.clone(),
                        level: level.clone(),
                    });
                }
                if !has(StateCategory::Blocked) {
                    return Err(PolicyError::ReviewWithoutBlocked(name.clone()));
                }
            }
            if let Some(target) = config
                .depends_on
                .iter()
                .find(|target| !self.types.contains_key(*target))
            {
                return Err(PolicyError::UnknownDependencyTarget {
                    type_name: name.clone(),
                    target: target.clone(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_name(name: &str) -> TypeName {
        TypeName::new(name).unwrap()
    }

    #[test]
    fn default_policy_is_valid() {
        let policy = Policy::parse(DEFAULT_POLICY_TOML).unwrap();
        assert_eq!(&policy, Policy::default_policy());
        let names: Vec<_> = Policy::builtin_type_names()
            .map(|name| name.as_str())
            .collect();
        assert_eq!(names, ["decision", "distill", "question", "task"]);
    }

    #[test]
    fn state_categories_of_default_task_states() {
        let policy = Policy::default_policy();
        let task = policy.type_config(&type_name("task")).unwrap();
        let state_category = |state: &str| task.state_category_of(&StateName::new(state).unwrap());
        assert_eq!(state_category("backlog"), Some(StateCategory::Inactive));
        assert_eq!(state_category("in_review"), Some(StateCategory::Blocked));
        assert_eq!(state_category("dropped"), Some(StateCategory::Cancelled));
        assert_eq!(state_category("nope"), None);
    }

    #[test]
    fn lease_parses_humantime() {
        let policy = Policy::default_policy();
        assert_eq!(policy.assignment.lease, Lease(Duration::from_secs(1800)));
    }

    #[test]
    fn custom_types_are_accepted() {
        let source = format!(
            "{DEFAULT_POLICY_TOML}\n[types.bug]\nlane = \"default\"\ninitial = \"new\"\n\
             states = [{{ name = \"new\", category = \"open\" }}, {{ name = \"fixed\", category = \"done\" }}]\n"
        );
        let policy = Policy::parse(&source).unwrap();
        assert!(policy.type_config(&type_name("bug")).is_some());
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            Policy::parse("types = 3"),
            Err(PolicyError::Parse(_))
        ));
    }

    #[test]
    fn rejects_missing_builtin() {
        let source = DEFAULT_POLICY_TOML.replace("[types.distill]", "[types.distillery]");
        assert_eq!(
            Policy::parse(&source),
            Err(PolicyError::BuiltinMissing(type_name("distill")))
        );
    }

    #[test]
    fn rejects_changed_builtin_states() {
        let source = DEFAULT_POLICY_TOML.replace(
            r#"{ name = "answered", category = "done" }"#,
            r#"{ name = "answered", category = "blocked" }"#,
        );
        assert_eq!(
            Policy::parse(&source),
            Err(PolicyError::BuiltinStatesChanged(type_name("question")))
        );
    }

    fn with_custom(custom: &str) -> String {
        format!("{DEFAULT_POLICY_TOML}\n{custom}")
    }

    #[test]
    fn rejects_unknown_initial_state() {
        let source = with_custom(
            "[types.bug]\nlane = \"default\"\ninitial = \"ghost\"\n\
             states = [{ name = \"new\", category = \"open\" }]\n",
        );
        assert_eq!(
            Policy::parse(&source),
            Err(PolicyError::UnknownInitialState {
                type_name: type_name("bug"),
                state: StateName::new("ghost").unwrap()
            })
        );
    }

    #[test]
    fn rejects_active_without_open() {
        let source = with_custom(
            "[types.bug]\nlane = \"default\"\ninitial = \"wip\"\n\
             states = [{ name = \"wip\", category = \"active\" }]\n",
        );
        assert_eq!(
            Policy::parse(&source),
            Err(PolicyError::ActiveWithoutOpen(type_name("bug")))
        );
    }

    #[test]
    fn rejects_duplicate_states_and_unknown_targets() {
        let duplicate = with_custom(
            "[types.bug]\nlane = \"default\"\ninitial = \"new\"\n\
             states = [{ name = \"new\", category = \"open\" }, { name = \"new\", category = \"done\" }]\n",
        );
        assert!(matches!(
            Policy::parse(&duplicate),
            Err(PolicyError::DuplicateState { .. })
        ));
        let unknown_target = with_custom(
            "[types.bug]\nlane = \"default\"\ninitial = \"new\"\ndepends_on = [\"ghost\"]\n\
             states = [{ name = \"new\", category = \"open\" }]\n",
        );
        assert!(matches!(
            Policy::parse(&unknown_target),
            Err(PolicyError::UnknownDependencyTarget { .. })
        ));
    }

    #[test]
    fn default_policy_has_ordered_review_levels_and_a_supervision_lane() {
        let policy = Policy::default_policy();
        let names: Vec<_> = policy
            .review
            .iter()
            .map(|level| level.name.as_str())
            .collect();
        assert_eq!(names, ["light", "standard", "critical"]);
        let critical = policy
            .review_level(&ReviewName::new("critical").unwrap())
            .unwrap();
        let requires: BTreeSet<_> = ["review", "strong-model"]
            .into_iter()
            .map(|name| Requirement::new(name).unwrap())
            .collect();
        assert_eq!(
            critical.steps,
            [ReviewStep::Agent { requires }, ReviewStep::Human]
        );
        assert_eq!(policy.merge, MergeConfig::default());
        assert!(
            policy
                .lanes
                .contains_key(&LaneName::new("supervision").unwrap())
        );
        let question = policy.type_config(&type_name("question")).unwrap();
        assert_eq!(question.lane.as_str(), "supervision");
        assert_eq!(
            question
                .first_state(StateCategory::Blocked)
                .map(StateName::as_str),
            Some("needs_human")
        );
        assert_eq!(question.review, None);
        assert_eq!(
            policy.review_rank(&ReviewName::new("standard").unwrap()),
            Some(1)
        );
    }

    #[test]
    fn a_type_names_its_default_review_level() {
        let source = DEFAULT_POLICY_TOML.replace(
            "[types.task]\nlane = \"default\"",
            "[types.task]\nreview = \"standard\"\nlane = \"default\"",
        );
        let policy = Policy::parse(&source).unwrap();
        let task = policy.type_config(&type_name("task")).unwrap();
        assert_eq!(task.review, Some(ReviewName::new("standard").unwrap()));
    }

    #[test]
    fn rejects_review_levels_that_do_not_hold_together() {
        let duplicate = with_custom("[[review]]\nname = \"light\"\nsteps = [\"human\"]\n");
        assert_eq!(
            Policy::parse(&duplicate),
            Err(PolicyError::DuplicateReviewLevel(
                ReviewName::new("light").unwrap()
            ))
        );
        let empty = with_custom("[[review]]\nname = \"none\"\nsteps = []\n");
        assert_eq!(
            Policy::parse(&empty),
            Err(PolicyError::NoReviewSteps(ReviewName::new("none").unwrap()))
        );
        let unknown = DEFAULT_POLICY_TOML.replace(
            "[types.task]\nlane = \"default\"",
            "[types.task]\nreview = \"ghost\"\nlane = \"default\"",
        );
        assert_eq!(
            Policy::parse(&unknown),
            Err(PolicyError::UnknownReviewLevel {
                type_name: type_name("task"),
                level: ReviewName::new("ghost").unwrap()
            })
        );
        let unreviewable = with_custom(
            "[types.bug]\nlane = \"default\"\ninitial = \"new\"\nreview = \"light\"\n\
             states = [{ name = \"new\", category = \"open\" }, { name = \"fixed\", category = \"done\" }]\n",
        );
        assert_eq!(
            Policy::parse(&unreviewable),
            Err(PolicyError::ReviewWithoutBlocked(type_name("bug")))
        );
    }

    #[test]
    fn the_old_review_table_is_gone() {
        let old = with_custom("[review]\napprovals = 2\n");
        assert!(matches!(Policy::parse(&old), Err(PolicyError::Parse(_))));
    }

    #[test]
    fn the_first_state_of_a_category_is_the_one_the_runner_uses() {
        let task = Policy::default_policy()
            .type_config(&type_name("task"))
            .unwrap();
        let first = |category| task.first_state(category).map(StateName::as_str);
        assert_eq!(first(StateCategory::Open), Some("open"));
        assert_eq!(first(StateCategory::Active), Some("in_progress"));
        assert_eq!(first(StateCategory::Blocked), Some("in_review"));
        assert_eq!(first(StateCategory::Done), Some("done"));
    }

    #[test]
    fn postcard_round_trip() {
        let policy = Policy::default_policy();
        let bytes = postcard::to_stdvec(policy).unwrap();
        assert_eq!(&postcard::from_bytes::<Policy>(&bytes).unwrap(), policy);
    }
}
