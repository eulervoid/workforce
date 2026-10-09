//! The workforce policy: item types, lanes and assignment. Stored verbatim as TOML in the log.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::identity::Role;
use crate::name::{LaneName, StateName, TypeName};

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
}

/// Fixed set of behavioural categories. Built-in behaviour keys on these, never on state names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Inactive,
    Open,
    Active,
    Blocked,
    Done,
    Cancelled,
}

impl Category {
    pub fn is_terminal(self) -> bool {
        match self {
            Category::Done | Category::Cancelled => true,
            Category::Inactive | Category::Open | Category::Active | Category::Blocked => false,
        }
    }
}

impl std::fmt::Display for Category {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Category::Inactive => "inactive",
            Category::Open => "open",
            Category::Active => "active",
            Category::Blocked => "blocked",
            Category::Done => "done",
            Category::Cancelled => "cancelled",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateConfig {
    pub name: StateName,
    pub category: Category,
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
}

impl TypeConfig {
    pub fn category_of(&self, state: &StateName) -> Option<Category> {
        self.states
            .iter()
            .find(|candidate| &candidate.name == state)
            .map(|candidate| candidate.category)
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

/// Unknown top-level tables (`[review]`, `[transcripts]`, ...) are kept in the stored text but
/// ignored here until a later milestone gives them meaning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub types: BTreeMap<TypeName, TypeConfig>,
    #[serde(default)]
    pub lanes: BTreeMap<LaneName, LaneConfig>,
    #[serde(default)]
    pub assignment: AssignmentConfig,
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

    fn validate(&self) -> Result<(), PolicyError> {
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
            if config.category_of(&config.initial).is_none() {
                return Err(PolicyError::UnknownInitialState {
                    type_name: name.clone(),
                    state: config.initial.clone(),
                });
            }
            let has = |category| config.states.iter().any(|state| state.category == category);
            if has(Category::Active) && !has(Category::Open) {
                return Err(PolicyError::ActiveWithoutOpen(name.clone()));
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
    fn categories_of_default_task_states() {
        let policy = Policy::default_policy();
        let task = policy.type_config(&type_name("task")).unwrap();
        let category = |state: &str| task.category_of(&StateName::new(state).unwrap());
        assert_eq!(category("backlog"), Some(Category::Inactive));
        assert_eq!(category("in_review"), Some(Category::Blocked));
        assert_eq!(category("dropped"), Some(Category::Cancelled));
        assert_eq!(category("nope"), None);
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
    fn postcard_round_trip() {
        let policy = Policy::default_policy();
        let bytes = postcard::to_stdvec(policy).unwrap();
        assert_eq!(&postcard::from_bytes::<Policy>(&bytes).unwrap(), policy);
    }
}
