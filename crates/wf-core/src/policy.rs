//! The workforce policy: item types, lanes and assignment. Stored verbatim as TOML in the log.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::LazyLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::item::ItemState;
use crate::name::{
    AgentName, CommandName, LaneName, RecoveryLevelName, Requirement, ReviewLevelName,
    ReviewStepName, TypeName,
};

pub const DEFAULT_POLICY_TOML: &str = include_str!("default_policy.toml");

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("invalid policy: {0}")]
    Parse(String),
    #[error("built-in type `{0}` is missing")]
    BuiltinMissing(TypeName),
    #[error("built-in lane `{0}` is missing")]
    BuiltinLaneMissing(LaneName),
    #[error("initial state `{state}` of type `{type_name}` is not one of its states")]
    InitialNotInStates {
        type_name: TypeName,
        state: ItemState,
    },
    #[error(
        "type `{0}` names its states the old way; list the states it uses, such as \
         `initial = \"open\"` and `states = [\"draft\", \"open\", \"done\"]`, \
         from draft, open, active, review, done and cancelled"
    )]
    OldStates(TypeName),
    #[error("type `{0}` uses `active` but not `open`, to return to")]
    ActiveWithoutOpen(TypeName),
    #[error("type `{type_name}` may depend on unknown type `{target}`")]
    UnknownDependencyTarget {
        type_name: TypeName,
        target: TypeName,
    },
    #[error("type `{type_name}` is in the unknown lane `{lane}`")]
    UnknownLane { type_name: TypeName, lane: LaneName },
    #[error("review level `{0}` is defined twice")]
    DuplicateReviewLevel(ReviewLevelName),
    #[error("`default` is not a review level: it resets an item to the level of its type")]
    ReservedReviewLevel,
    #[error("type `{type_name}` has the unknown review level `{level}`")]
    UnknownReviewLevel {
        type_name: TypeName,
        level: ReviewLevelName,
    },
    #[error("type `{0}` has a review level but does not use `review`, to wait in")]
    ReviewWithoutReviewState(TypeName),
    #[error("recovery level `{0}` is defined twice")]
    DuplicateRecoveryLevel(RecoveryLevelName),
    #[error("`default` is not a recovery level name: it resets an item to the level of its type")]
    ReservedRecoveryLevel,
    #[error("type `{type_name}` has the unknown recovery level `{level}`")]
    UnknownRecoveryLevel {
        type_name: TypeName,
        level: RecoveryLevelName,
    },
    #[error("review level `{0}` has an entry without steps")]
    EmptyReviewEntry(ReviewLevelName),
    #[error(
        "review level `{level}` has two steps {} in one entry that differ; steps of one entry with \
         the same name, or both without, are the same step",
        .name.as_ref().map_or("without a name".to_string(), |name| format!("named `{name}`"))
    )]
    ConflictingReviewSteps {
        level: ReviewLevelName,
        name: Option<ReviewStepName>,
    },
    #[error("the check `{0}` is defined twice")]
    DuplicateCheck(CommandName),
    #[error("the check `{0}` runs nothing: `run` names the program and its arguments")]
    EmptyCheck(CommandName),
    #[error(
        "`[claims] lease` is {}, shorter than {}: devices announce themselves every minute, and \
         two lost announcements must not give their claims away",
        humantime_serde::re::humantime::format_duration(*.0),
        humantime_serde::re::humantime::format_duration(ClaimsConfig::MIN_LEASE)
    )]
    LeaseTooShort(Duration),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Priority,
    Labels,
    Requires,
}

/// `transitions = "any"`, which policies already in the log say: it was the only value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Transitions {
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
    /// The state items of the type are created in, unless they are created as drafts.
    #[serde(deserialize_with = "written_states::initial")]
    pub initial: ItemState,
    /// The states items of the type may be moved to.
    #[serde(deserialize_with = "written_states::deserialize")]
    pub states: BTreeSet<ItemState>,
    #[serde(default)]
    transitions: Option<Transitions>,
    /// The review level of items of this type that do not name one.
    #[serde(default)]
    pub review: Option<ReviewLevelName>,
    /// The recovery level of items of this type that do not name one. Without one, the principal
    /// decides after the first unsuccessful attempt.
    #[serde(default)]
    pub recovery: Option<RecoveryLevelName>,
}

impl TypeConfig {
    pub fn uses(&self, state: ItemState) -> bool {
        self.states.contains(&state)
    }

    pub fn allows_field(&self, field: Field) -> bool {
        self.fields.contains(&field)
    }

    pub fn allows_dependency_on(&self, target: &TypeName) -> bool {
        self.depends_on.contains(target)
    }
}

/// A lane items and agents can be put in. Nothing is configured per lane yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneConfig {}

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
    /// What has to pass on the merge result before the target moves, in this order.
    #[serde(rename = "check", default)]
    pub checks: Vec<CheckConfig>,
    /// What the hardware of a device must offer for it to make merges, as its checks run there.
    #[serde(default)]
    pub requires: Vec<Requirement>,
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
            checks: Vec::new(),
            requires: Vec::new(),
        }
    }
}

/// How claims outlive the devices that hold them: `[claims]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimsConfig {
    /// A device not heard from for this long, such as `"3m"`, is gone, and what its sessions claim
    /// can be taken over.
    #[serde(default = "ClaimsConfig::default_lease", with = "humantime_serde")]
    pub lease: Duration,
}

impl ClaimsConfig {
    /// Three announcements a minute apart, so that two lost ones do not give a claim away.
    pub const MIN_LEASE: Duration = Duration::from_secs(3 * 60);

    fn default_lease() -> Duration {
        Self::MIN_LEASE
    }
}

impl Default for ClaimsConfig {
    fn default() -> Self {
        Self {
            lease: Self::default_lease(),
        }
    }
}

/// A command that must pass on the merge result before work lands: `[[merge.check]]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConfig {
    /// What the check is called where its result is shown, such as `clippy`. Unique.
    pub name: CommandName,
    /// The program and its arguments, run without a shell.
    pub run: Vec<String>,
    /// How long it may run before it is stopped, such as `"20m"`. A machine may allow less.
    #[serde(default = "CheckConfig::default_timeout", with = "humantime_serde")]
    pub timeout: Duration,
}

impl CheckConfig {
    fn default_timeout() -> Duration {
        Duration::from_secs(30 * 60)
    }
}

/// One step of a review level: an agent chosen by capabilities, or the principal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewStep {
    Agent {
        /// Tells the step from the others that run at once. Verdicts count for the step of their
        /// review session's name.
        name: Option<ReviewStepName>,
        requires: BTreeSet<Requirement>,
        /// What this reviewer looks at, beside the instructions of the agent itself. They live in
        /// the signed policy: a file of the repository could be changed by the reviewed work.
        instructions: Option<String>,
    },
    Principal,
}

/// A named sequence of review entries, each a list of steps that run at once and must all
/// approve before the next entry. Levels are listed from least to most strict. A level without
/// entries needs no review. In policy text an entry of one step may be written as the step alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewLevelConfig {
    pub name: ReviewLevelName,
    #[serde(deserialize_with = "written_steps::deserialize")]
    pub steps: Vec<Vec<ReviewStep>>,
    /// How many rounds of changes requested by agent reviews send an item back to work before
    /// the principal decides; without it, every round does.
    #[serde(default)]
    pub max_rounds: Option<NonZeroU32>,
}

/// How a policy writes review steps. Policies already in the log write an agent step as
/// `{ agent = { requires } }` and the principal as `"human"`; postcard, which carries the policy
/// between the daemon and the CLI, has the plain form of the enum.
mod written_steps {
    use std::collections::BTreeSet;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::ReviewStep;
    use crate::name::{Requirement, ReviewStepName};

    #[derive(Serialize, Deserialize)]
    enum Binary {
        Agent {
            name: Option<ReviewStepName>,
            requires: BTreeSet<Requirement>,
            instructions: Option<String>,
        },
        Principal,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct AgentStep {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<ReviewStepName>,
        requires: BTreeSet<Requirement>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StoredAgentStep {
        agent: StoredRequires,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StoredRequires {
        requires: BTreeSet<Requirement>,
    }

    #[derive(Deserialize)]
    enum PrincipalWord {
        #[serde(rename = "principal", alias = "human")]
        Principal,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Written {
        Principal(PrincipalWord),
        Stored(StoredAgentStep),
        Agent(AgentStep),
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Group(Vec<ReviewStep>),
        Single(ReviewStep),
    }

    impl Serialize for ReviewStep {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            if !serializer.is_human_readable() {
                return match self.clone() {
                    ReviewStep::Agent {
                        name,
                        requires,
                        instructions,
                    } => Binary::Agent {
                        name,
                        requires,
                        instructions,
                    },
                    ReviewStep::Principal => Binary::Principal,
                }
                .serialize(serializer);
            }
            match self {
                ReviewStep::Agent {
                    name,
                    requires,
                    instructions,
                } => AgentStep {
                    name: name.clone(),
                    requires: requires.clone(),
                    instructions: instructions.clone(),
                }
                .serialize(serializer),
                ReviewStep::Principal => serializer.serialize_str("principal"),
            }
        }
    }

    impl<'de> Deserialize<'de> for ReviewStep {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            if !deserializer.is_human_readable() {
                return Ok(match Binary::deserialize(deserializer)? {
                    Binary::Agent {
                        name,
                        requires,
                        instructions,
                    } => ReviewStep::Agent {
                        name,
                        requires,
                        instructions,
                    },
                    Binary::Principal => ReviewStep::Principal,
                });
            }
            Ok(match Written::deserialize(deserializer)? {
                Written::Principal(PrincipalWord::Principal) => ReviewStep::Principal,
                Written::Stored(StoredAgentStep {
                    agent: StoredRequires { requires },
                }) => ReviewStep::Agent {
                    name: None,
                    requires,
                    instructions: None,
                },
                Written::Agent(AgentStep {
                    name,
                    requires,
                    instructions,
                }) => ReviewStep::Agent {
                    name,
                    requires,
                    instructions,
                },
            })
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Vec<ReviewStep>>, D::Error> {
        if !deserializer.is_human_readable() {
            return Vec::deserialize(deserializer);
        }
        let entries = Vec::<Entry>::deserialize(deserializer)?;
        Ok(entries
            .into_iter()
            .map(|entry| match entry {
                Entry::Group(steps) => steps,
                Entry::Single(step) => vec![step],
            })
            .collect())
    }
}

/// One step of a recovery level, taken when a session ends without finishing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStep {
    /// The item may be started again, by any agent that can take it.
    Retry,
    /// An agent with these capabilities says what went wrong, in a diagnose session, before the
    /// next step. Only work is diagnosed: for reviews and supervision this step retries.
    Agent { requires: BTreeSet<Requirement> },
    /// The item waits for the principal.
    Principal,
}

/// A named sequence of recovery steps: the first is taken after the first unsuccessful attempt,
/// the second after the next, and so on. After the last step the principal decides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryLevelConfig {
    pub name: RecoveryLevelName,
    pub steps: Vec<RecoveryStep>,
}

/// Unknown top-level tables (`[transcripts]`, ...) are kept in the stored text but ignored here
/// until a later milestone gives them meaning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// The agent `wf session start` uses when none is given.
    #[serde(default)]
    pub default_agent: Option<AgentName>,
    pub types: BTreeMap<TypeName, TypeConfig>,
    #[serde(default)]
    pub lanes: BTreeMap<LaneName, LaneConfig>,
    #[serde(default)]
    pub merge: MergeConfig,
    #[serde(default)]
    pub claims: ClaimsConfig,
    /// Ordered from least to most strict: the position is what lowering and raising mean.
    #[serde(default)]
    pub review: Vec<ReviewLevelConfig>,
    #[serde(default)]
    pub recovery: Vec<RecoveryLevelConfig>,
}

static DEFAULT_POLICY: LazyLock<Policy> = LazyLock::new(|| {
    toml::from_str(DEFAULT_POLICY_TOML).expect("the bundled default policy is valid TOML")
});

impl Policy {
    pub fn default_policy() -> &'static Policy {
        &DEFAULT_POLICY
    }

    /// Parses and validates policy text. Policies already in the log may name states the old
    /// way; [`Policy::parse_new`] refuses that for a policy about to be written.
    pub fn parse(source: &str) -> Result<Policy, PolicyError> {
        let mut policy: Policy =
            toml::from_str(source).map_err(|error| PolicyError::Parse(error.to_string()))?;
        let written: written_states::WrittenTypes =
            toml::from_str(source).map_err(|error| PolicyError::Parse(error.to_string()))?;
        for (name, states) in &written.types {
            if let (Some(config), Some(initial)) =
                (policy.types.get_mut(name), states.old_initial())
            {
                config.initial = initial;
            }
        }
        policy.validate()?;
        Ok(policy)
    }

    /// Parses policy text that is about to be written: like [`Policy::parse`], but the states
    /// have to be named the current way.
    pub fn parse_new(source: &str) -> Result<Policy, PolicyError> {
        let policy = Policy::parse(source)?;
        let written: written_states::WrittenTypes =
            toml::from_str(source).map_err(|error| PolicyError::Parse(error.to_string()))?;
        if let Some((name, _)) = written
            .types
            .iter()
            .find(|(_, states)| !states.is_current())
        {
            return Err(PolicyError::OldStates(name.clone()));
        }
        Ok(policy)
    }

    pub fn type_config(&self, type_name: &TypeName) -> Option<&TypeConfig> {
        self.types.get(type_name)
    }

    pub fn review_level(&self, name: &ReviewLevelName) -> Option<&ReviewLevelConfig> {
        self.review.iter().find(|level| &level.name == name)
    }

    pub fn recovery_level(&self, name: &RecoveryLevelName) -> Option<&RecoveryLevelConfig> {
        self.recovery.iter().find(|level| &level.name == name)
    }

    /// How strict a level is: its position in the list.
    pub fn review_rank(&self, name: &ReviewLevelName) -> Option<usize> {
        self.review.iter().position(|level| &level.name == name)
    }

    fn validate(&self) -> Result<(), PolicyError> {
        let mut levels = BTreeSet::new();
        for level in &self.review {
            if !levels.insert(&level.name) {
                return Err(PolicyError::DuplicateReviewLevel(level.name.clone()));
            }
            if level.name.as_str() == "default" {
                return Err(PolicyError::ReservedReviewLevel);
            }
            for entry in &level.steps {
                if entry.is_empty() {
                    return Err(PolicyError::EmptyReviewEntry(level.name.clone()));
                }
                // So that it never matters which of them a verdict counts for.
                for (index, step) in entry.iter().enumerate() {
                    let ReviewStep::Agent { name, .. } = step else {
                        continue;
                    };
                    let conflicts = entry[..index].iter().any(|earlier| {
                        matches!(earlier, ReviewStep::Agent { name: other, .. } if other == name)
                            && earlier != step
                    });
                    if conflicts {
                        return Err(PolicyError::ConflictingReviewSteps {
                            level: level.name.clone(),
                            name: name.clone(),
                        });
                    }
                }
            }
        }
        let mut recovery_levels = BTreeSet::new();
        for level in &self.recovery {
            if !recovery_levels.insert(&level.name) {
                return Err(PolicyError::DuplicateRecoveryLevel(level.name.clone()));
            }
            if level.name.as_str() == "default" {
                return Err(PolicyError::ReservedRecoveryLevel);
            }
        }
        let mut checks = BTreeSet::new();
        for check in &self.merge.checks {
            if !checks.insert(&check.name) {
                return Err(PolicyError::DuplicateCheck(check.name.clone()));
            }
            if check.run.is_empty() {
                return Err(PolicyError::EmptyCheck(check.name.clone()));
            }
        }
        if self.claims.lease < ClaimsConfig::MIN_LEASE {
            return Err(PolicyError::LeaseTooShort(self.claims.lease));
        }
        if let Some(name) = DEFAULT_POLICY
            .types
            .keys()
            .find(|name| !self.types.contains_key(*name))
        {
            return Err(PolicyError::BuiltinMissing(name.clone()));
        }
        if !self.lanes.contains_key(&LaneName::supervision()) {
            return Err(PolicyError::BuiltinLaneMissing(LaneName::supervision()));
        }
        for (name, config) in &self.types {
            if !config.uses(config.initial) {
                return Err(PolicyError::InitialNotInStates {
                    type_name: name.clone(),
                    state: config.initial,
                });
            }
            if !self.lanes.contains_key(&config.lane) {
                return Err(PolicyError::UnknownLane {
                    type_name: name.clone(),
                    lane: config.lane.clone(),
                });
            }
            if config.uses(ItemState::Active) && !config.uses(ItemState::Open) {
                return Err(PolicyError::ActiveWithoutOpen(name.clone()));
            }
            if let Some(level) = &config.review {
                if self.review_level(level).is_none() {
                    return Err(PolicyError::UnknownReviewLevel {
                        type_name: name.clone(),
                        level: level.clone(),
                    });
                }
                let reviews = self
                    .review_level(level)
                    .is_some_and(|level| !level.steps.is_empty());
                if reviews && !config.uses(ItemState::Review) {
                    return Err(PolicyError::ReviewWithoutReviewState(name.clone()));
                }
            }
            if let Some(level) = &config.recovery
                && self.recovery_level(level).is_none()
            {
                return Err(PolicyError::UnknownRecoveryLevel {
                    type_name: name.clone(),
                    level: level.clone(),
                });
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

/// How a policy lists the states of a type. Policies already in the log list them the old way,
/// as `{ name, category }` tables, where the category is the state; postcard, which carries the
/// policy between the daemon and the CLI, always has the current form.
mod written_states {
    use std::collections::{BTreeMap, BTreeSet};

    use serde::{Deserialize, Deserializer};

    use crate::item::ItemState;
    use crate::name::TypeName;

    /// An old name the old list of states does not resolve reads as `draft`; see
    /// [`TypeStates::old_initial`].
    pub(super) fn initial<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ItemState, D::Error> {
        if !deserializer.is_human_readable() {
            return ItemState::deserialize(deserializer);
        }
        let name = String::deserialize(deserializer)?;
        Ok(ItemState::from_written(&name).unwrap_or(ItemState::Draft))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeSet<ItemState>, D::Error> {
        if !deserializer.is_human_readable() {
            return BTreeSet::deserialize(deserializer);
        }
        let listed = Vec::<Listed>::deserialize(deserializer)?;
        listed
            .into_iter()
            .map(|listed| match listed {
                Listed::State(name) => ItemState::from_written(&name)
                    .ok_or_else(|| serde::de::Error::custom(crate::item::ItemStateError(name))),
                Listed::Old { category, .. } => Ok(category.state()),
            })
            .collect()
    }

    /// A state by name, old names too, or an old `{ name, category }` table.
    #[derive(Clone, Deserialize)]
    #[serde(untagged)]
    enum Listed {
        State(String),
        Old { name: String, category: Category },
    }

    #[derive(Clone, Copy, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Category {
        Inactive,
        Open,
        Active,
        Review,
        Done,
        Cancelled,
    }

    impl Category {
        fn state(self) -> ItemState {
            match self {
                Category::Inactive => ItemState::Draft,
                Category::Open => ItemState::Open,
                Category::Active => ItemState::Active,
                Category::Review => ItemState::Review,
                Category::Done => ItemState::Done,
                Category::Cancelled => ItemState::Cancelled,
            }
        }
    }

    /// The names of the states of every type, as written.
    #[derive(Deserialize)]
    pub(super) struct WrittenTypes {
        pub(super) types: BTreeMap<TypeName, TypeStates>,
    }

    #[derive(Deserialize)]
    pub(super) struct TypeStates {
        initial: String,
        states: Vec<toml::Value>,
        transitions: Option<toml::Value>,
    }

    impl TypeStates {
        /// The initial state of an old list of states: the category of the state it names.
        pub(super) fn old_initial(&self) -> Option<ItemState> {
            self.states.iter().find_map(|state| {
                let Listed::Old { name, category } = state.clone().try_into().ok()? else {
                    return None;
                };
                (name == self.initial).then(|| category.state())
            })
        }

        pub(super) fn is_current(&self) -> bool {
            let current = |name: &str| name.parse::<ItemState>().is_ok();
            self.transitions.is_none()
                && current(&self.initial)
                && self
                    .states
                    .iter()
                    .all(|state| state.as_str().is_some_and(current))
        }
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
        let names: Vec<_> = policy.types.keys().map(|name| name.as_str()).collect();
        assert_eq!(names, ["decision", "distill", "task"]);
    }

    #[test]
    fn the_built_in_types_use_the_states_of_the_proposal() {
        let policy = Policy::default_policy();
        let states = |name: &str| {
            let config = policy.type_config(&type_name(name)).unwrap();
            (
                config.initial,
                config.states.iter().copied().collect::<Vec<_>>(),
            )
        };
        assert_eq!(states("task"), (ItemState::Open, ItemState::ALL.to_vec()));
        assert_eq!(
            states("decision"),
            (
                ItemState::Review,
                vec![ItemState::Review, ItemState::Done, ItemState::Cancelled]
            )
        );
        assert_eq!(
            states("distill"),
            (
                ItemState::Open,
                vec![
                    ItemState::Open,
                    ItemState::Active,
                    ItemState::Done,
                    ItemState::Cancelled
                ]
            )
        );
    }

    const OLD_DEFAULT_POLICY: &str = include_str!("tests/old_default_policy.toml");

    #[test]
    fn policies_with_the_old_states_still_parse_but_are_not_written() {
        let old = Policy::parse(OLD_DEFAULT_POLICY).unwrap();
        let decision = old.type_config(&type_name("decision")).unwrap();
        assert_eq!(decision.initial, ItemState::Review);
        let distill = old.type_config(&type_name("distill")).unwrap();
        assert!(!distill.uses(ItemState::Cancelled));
        assert_eq!(
            Policy::parse_new(OLD_DEFAULT_POLICY),
            Err(PolicyError::OldStates(type_name("decision")))
        );
        assert_eq!(
            &Policy::parse_new(DEFAULT_POLICY_TOML).unwrap(),
            Policy::default_policy()
        );
    }

    #[test]
    fn stored_policies_with_assignment_and_lane_claims_still_parse() {
        let source = DEFAULT_POLICY_TOML.replace(
            "[lanes.meta]",
            "[lanes.meta]\nclaim = [\"member\"]\ntypes = [\"distill\"]",
        ) + "\n[assignment]\nmode = \"dispatch\"\nlease = \"30m\"\n";
        assert_eq!(&Policy::parse(&source).unwrap(), Policy::default_policy());
    }

    #[test]
    fn custom_types_are_accepted() {
        let source = format!(
            "{DEFAULT_POLICY_TOML}\n[types.bug]\nlane = \"default\"\ninitial = \"new\"\n\
             states = [{{ name = \"new\", category = \"open\" }}, {{ name = \"fixed\", category = \"done\" }}]\n"
        );
        let policy = Policy::parse(&source).unwrap();
        let bug = policy.type_config(&type_name("bug")).unwrap();
        // The initial state is the category of the old state it names.
        assert_eq!(bug.initial, ItemState::Open);
        assert_eq!(
            bug.states,
            BTreeSet::from([ItemState::Open, ItemState::Done])
        );
        let current = format!(
            "{DEFAULT_POLICY_TOML}\n[types.bug]\nlane = \"default\"\ninitial = \"open\"\n\
             states = [\"open\", \"done\"]\n"
        );
        assert!(Policy::parse_new(&current).is_ok());
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            Policy::parse("types = 3"),
            Err(PolicyError::Parse(_))
        ));
    }

    #[test]
    fn rejects_missing_supervision_lane() {
        let source = DEFAULT_POLICY_TOML.replace("[lanes.supervision]", "[lanes.oversight]");
        assert_eq!(
            Policy::parse(&source),
            Err(PolicyError::BuiltinLaneMissing(LaneName::supervision()))
        );
    }

    #[test]
    fn rejects_missing_builtin() {
        let source = DEFAULT_POLICY_TOML.replace("[types.distill]", "[types.distillery]");
        assert_eq!(
            Policy::parse(&source),
            Err(PolicyError::BuiltinMissing(type_name("distill")))
        );
    }

    fn with_custom(custom: &str) -> String {
        format!("{DEFAULT_POLICY_TOML}\n{custom}")
    }

    #[test]
    fn rejects_an_initial_state_the_type_does_not_use() {
        let source = with_custom(
            "[types.bug]\nlane = \"default\"\ninitial = \"draft\"\nstates = [\"open\"]\n",
        );
        assert_eq!(
            Policy::parse(&source),
            Err(PolicyError::InitialNotInStates {
                type_name: type_name("bug"),
                state: ItemState::Draft
            })
        );
        let unknown = with_custom(
            "[types.bug]\nlane = \"default\"\ninitial = \"ghost\"\nstates = [\"open\"]\n",
        );
        assert!(matches!(
            Policy::parse(&unknown),
            Err(PolicyError::InitialNotInStates { .. })
        ));
        assert_eq!(
            Policy::parse_new(&unknown.replace("\"open\"]", "\"draft\"]")),
            Err(PolicyError::OldStates(type_name("bug")))
        );
    }

    #[test]
    fn rejects_active_without_open() {
        let source = with_custom(
            "[types.bug]\nlane = \"default\"\ninitial = \"active\"\nstates = [\"active\"]\n",
        );
        assert_eq!(
            Policy::parse(&source),
            Err(PolicyError::ActiveWithoutOpen(type_name("bug")))
        );
    }

    #[test]
    fn rejects_an_unknown_lane() {
        let source =
            with_custom("[types.bug]\nlane = \"ghost\"\ninitial = \"open\"\nstates = [\"open\"]\n");
        assert_eq!(
            Policy::parse(&source),
            Err(PolicyError::UnknownLane {
                type_name: type_name("bug"),
                lane: LaneName::new("ghost").unwrap()
            })
        );
    }

    #[test]
    fn after_merge_commands_are_not_config_yet() {
        // `[[merge.after]]` and `[merge] after_device` are planned (SPEC section 10): until they
        // run, a policy that names them is refused rather than read as if they did.
        let after_device =
            DEFAULT_POLICY_TOML.replace("[merge]\n", "[merge]\nafter_device = \"server\"\n");
        assert_ne!(after_device, DEFAULT_POLICY_TOML);
        let after =
            with_custom("[[merge.after]]\nname = \"deploy\"\nrun = [\"make\", \"deploy\"]\n");
        for source in [after_device, after] {
            assert!(
                matches!(Policy::parse(&source), Err(PolicyError::Parse(_))),
                "{source}"
            );
        }
    }

    #[test]
    fn rejects_unknown_targets() {
        let unknown_target = with_custom(
            "[types.bug]\nlane = \"default\"\ninitial = \"open\"\ndepends_on = [\"ghost\"]\n\
             states = [\"open\"]\n",
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
        assert_eq!(names, ["none", "light", "standard", "critical", "manual"]);
        let critical = policy
            .review_level(&ReviewLevelName::new("critical").unwrap())
            .unwrap();
        let requires: BTreeSet<_> = ["review", "strong-model"]
            .into_iter()
            .map(|name| Requirement::new(name).unwrap())
            .collect();
        assert_eq!(
            critical.steps,
            [
                vec![ReviewStep::Agent {
                    name: None,
                    requires,
                    instructions: None
                }],
                vec![ReviewStep::Principal]
            ]
        );
        assert_eq!(policy.merge, MergeConfig::default());
        assert!(policy.lanes.contains_key(&LaneName::supervision()));
        let task = policy.type_config(&type_name("task")).unwrap();
        assert_eq!(task.review, Some(ReviewLevelName::new("manual").unwrap()));
        assert_eq!(
            policy.review_rank(&ReviewLevelName::new("standard").unwrap()),
            Some(2)
        );
    }

    #[test]
    fn a_type_names_its_default_review_level() {
        let source = DEFAULT_POLICY_TOML.replace(
            "[types.task]\nreview = \"manual\"\n",
            "[types.task]\nreview = \"standard\"\n",
        );
        let policy = Policy::parse(&source).unwrap();
        let task = policy.type_config(&type_name("task")).unwrap();
        assert_eq!(task.review, Some(ReviewLevelName::new("standard").unwrap()));
    }

    #[test]
    fn stored_policies_that_name_the_human_step_still_parse() {
        let old = DEFAULT_POLICY_TOML.replace(
            "name = \"manual\"\nsteps = [\"principal\"]",
            "name = \"manual\"\nsteps = [\"human\"]",
        );
        assert!(old.contains("[\"human\"]"));
        let policy = Policy::parse(&old).unwrap();
        let manual = policy
            .review_level(&ReviewLevelName::new("manual").unwrap())
            .unwrap();
        assert_eq!(manual.steps, [vec![ReviewStep::Principal]]);
    }

    #[test]
    fn stored_policies_that_write_an_agent_step_the_old_way_still_parse() {
        let old = DEFAULT_POLICY_TOML.replace(
            "steps = [{ requires = [\"review\"] }]",
            "steps = [{ agent = { requires = [\"review\"] } }]",
        );
        assert!(old.contains("agent = {"));
        let policy = Policy::parse(&old).unwrap();
        assert_eq!(policy.review, Policy::default_policy().review);
    }

    #[test]
    fn a_review_entry_may_list_steps_that_run_at_once() {
        let source = with_custom(
            "[[review]]\nname = \"wide\"\nsteps = [[\n\
             { name = \"style\", requires = [\"review\"], instructions = \"Naming.\" },\n\
             { requires = [\"review\"] },\n], \"principal\"]\n",
        );
        let policy = Policy::parse(&source).unwrap();
        let wide = policy
            .review_level(&ReviewLevelName::new("wide").unwrap())
            .unwrap();
        assert_eq!(wide.steps.len(), 2);
        assert_eq!(wide.steps[0].len(), 2);
        assert!(matches!(
            &wide.steps[0][0],
            ReviewStep::Agent { name: Some(name), instructions: Some(text), .. }
                if name.as_str() == "style" && text == "Naming."
        ));
        let bytes = postcard::to_stdvec(wide).unwrap();
        assert_eq!(
            &postcard::from_bytes::<ReviewLevelConfig>(&bytes).unwrap(),
            wide
        );
    }

    #[test]
    fn a_review_level_may_limit_its_rounds_to_a_positive_number() {
        let limited = with_custom(
            "[[review]]\nname = \"bounded\"\nsteps = [{ requires = [\"review\"] }]\nmax_rounds = 3\n",
        );
        let policy = Policy::parse(&limited).unwrap();
        let bounded = policy
            .review_level(&ReviewLevelName::new("bounded").unwrap())
            .unwrap();
        assert_eq!(bounded.max_rounds, NonZeroU32::new(3));
        let bytes = postcard::to_stdvec(bounded).unwrap();
        assert_eq!(
            &postcard::from_bytes::<ReviewLevelConfig>(&bytes).unwrap(),
            bounded
        );
        let standard = policy
            .review_level(&ReviewLevelName::new("standard").unwrap())
            .unwrap();
        assert_eq!(standard.max_rounds, None);
        for rounds in ["0", "-1"] {
            let source = with_custom(&format!(
                "[[review]]\nname = \"bounded\"\nsteps = []\nmax_rounds = {rounds}\n"
            ));
            assert!(
                matches!(Policy::parse(&source), Err(PolicyError::Parse(_))),
                "max_rounds = {rounds}"
            );
        }
    }

    #[test]
    fn rejects_review_entries_that_do_not_hold_together() {
        let empty = with_custom("[[review]]\nname = \"empty\"\nsteps = [[]]\n");
        assert!(matches!(
            Policy::parse(&empty),
            Err(PolicyError::EmptyReviewEntry(_))
        ));
        let differing = with_custom(
            "[[review]]\nname = \"differing\"\nsteps = [[\n\
             { name = \"style\", requires = [\"review\"] },\n\
             { name = \"style\", requires = [\"gpu\"] },\n]]\n",
        );
        assert!(matches!(
            Policy::parse(&differing),
            Err(PolicyError::ConflictingReviewSteps { .. })
        ));
        let twins = with_custom(
            "[[review]]\nname = \"twins\"\nsteps = [[\n\
             { requires = [\"review\"] },\n{ requires = [\"review\"] },\n]]\n",
        );
        assert!(Policy::parse(&twins).is_ok());
    }

    #[test]
    fn default_policy_retries_tasks_twice_before_the_principal_decides() {
        let policy = Policy::default_policy();
        let standard = policy
            .recovery_level(&RecoveryLevelName::new("standard").unwrap())
            .unwrap();
        assert_eq!(
            standard.steps,
            [
                RecoveryStep::Retry,
                RecoveryStep::Retry,
                RecoveryStep::Principal
            ]
        );
        let task = policy.type_config(&type_name("task")).unwrap();
        assert_eq!(
            task.recovery,
            Some(RecoveryLevelName::new("standard").unwrap())
        );
    }

    #[test]
    fn rejects_recovery_levels_that_do_not_hold_together() {
        let duplicate = with_custom("[[recovery]]\nname = \"none\"\nsteps = [\"retry\"]\n");
        assert_eq!(
            Policy::parse(&duplicate),
            Err(PolicyError::DuplicateRecoveryLevel(
                RecoveryLevelName::new("none").unwrap()
            ))
        );
        let reserved = with_custom("[[recovery]]\nname = \"default\"\nsteps = []\n");
        assert_eq!(
            Policy::parse(&reserved),
            Err(PolicyError::ReservedRecoveryLevel)
        );
        let unknown = DEFAULT_POLICY_TOML.replace(
            "review = \"manual\"\nrecovery = \"standard\"",
            "review = \"manual\"\nrecovery = \"ghost\"",
        );
        assert_eq!(
            Policy::parse(&unknown),
            Err(PolicyError::UnknownRecoveryLevel {
                type_name: type_name("task"),
                level: RecoveryLevelName::new("ghost").unwrap()
            })
        );
    }

    #[test]
    fn rejects_review_levels_that_do_not_hold_together() {
        let duplicate = with_custom("[[review]]\nname = \"light\"\nsteps = [\"principal\"]\n");
        assert_eq!(
            Policy::parse(&duplicate),
            Err(PolicyError::DuplicateReviewLevel(
                ReviewLevelName::new("light").unwrap()
            ))
        );
        let reserved = with_custom("[[review]]\nname = \"default\"\nsteps = []\n");
        assert_eq!(
            Policy::parse(&reserved),
            Err(PolicyError::ReservedReviewLevel)
        );
        let unknown = DEFAULT_POLICY_TOML.replace("review = \"manual\"\n", "review = \"ghost\"\n");
        assert_eq!(
            Policy::parse(&unknown),
            Err(PolicyError::UnknownReviewLevel {
                type_name: type_name("task"),
                level: ReviewLevelName::new("ghost").unwrap()
            })
        );
        let unreviewable = with_custom(
            "[types.bug]\nlane = \"default\"\ninitial = \"open\"\nreview = \"light\"\n\
             states = [\"open\", \"done\"]\n",
        );
        assert_eq!(
            Policy::parse(&unreviewable),
            Err(PolicyError::ReviewWithoutReviewState(type_name("bug")))
        );
    }

    #[test]
    fn the_old_review_table_is_gone() {
        let old = with_custom("[review]\napprovals = 2\n");
        assert!(matches!(Policy::parse(&old), Err(PolicyError::Parse(_))));
    }

    #[test]
    fn postcard_round_trip() {
        let policy = Policy::default_policy();
        let bytes = postcard::to_stdvec(policy).unwrap();
        assert_eq!(&postcard::from_bytes::<Policy>(&bytes).unwrap(), policy);
    }

    const CHECKS: &str = "\
[[merge.check]]
name = \"fmt\"
run = [\"cargo\", \"fmt\", \"--check\"]

[[merge.check]]
name = \"test\"
run = [\"cargo\", \"test\"]
timeout = \"20m\"
";

    #[test]
    fn checks_are_listed_in_order_with_a_timeout_of_30_minutes_unless_given() {
        let policy = Policy::parse(&with_custom(CHECKS)).unwrap();
        let checks: Vec<_> = policy
            .merge
            .checks
            .iter()
            .map(|check| (check.name.as_str(), check.run.len(), check.timeout))
            .collect();
        assert_eq!(
            checks,
            [
                ("fmt", 3, Duration::from_secs(30 * 60)),
                ("test", 2, Duration::from_secs(20 * 60))
            ]
        );
        assert_eq!(policy.merge.target, "main");
        let bytes = postcard::to_stdvec(&policy).unwrap();
        assert_eq!(postcard::from_bytes::<Policy>(&bytes).unwrap(), policy);
    }

    #[test]
    fn a_policy_that_repeats_the_name_of_a_check_is_refused() {
        let repeated = with_custom(&format!(
            "{CHECKS}\n[[merge.check]]\nname = \"fmt\"\nrun = [\"true\"]\n"
        ));
        assert_eq!(
            Policy::parse(&repeated),
            Err(PolicyError::DuplicateCheck(
                CommandName::new("fmt").unwrap()
            ))
        );
    }

    #[test]
    fn a_check_that_runs_nothing_is_refused() {
        let empty = with_custom("[[merge.check]]\nname = \"nothing\"\nrun = []\n");
        assert_eq!(
            Policy::parse(&empty),
            Err(PolicyError::EmptyCheck(
                CommandName::new("nothing").unwrap()
            ))
        );
    }

    fn with_lease(lease: &str) -> String {
        with_custom(&format!("[claims]\n{lease}\n"))
    }

    #[test]
    fn a_claim_lasts_three_minutes_after_its_device_went_silent_unless_the_policy_says_longer() {
        let three_minutes = Duration::from_secs(3 * 60);
        assert_eq!(Policy::default_policy().claims.lease, three_minutes);
        let unsaid = Policy::parse(&with_lease("")).unwrap();
        assert_eq!(unsaid.claims.lease, three_minutes);
        let longer = Policy::parse(&with_lease("lease = \"10m\"")).unwrap();
        assert_eq!(longer.claims.lease, Duration::from_secs(10 * 60));
        let bytes = postcard::to_stdvec(&longer).unwrap();
        assert_eq!(postcard::from_bytes::<Policy>(&bytes).unwrap(), longer);
    }

    #[test]
    fn a_lease_two_lost_announcements_would_end_is_refused() {
        let error = Policy::parse(&with_lease("lease = \"2m 59s\"")).unwrap_err();
        assert_eq!(
            error,
            PolicyError::LeaseTooShort(Duration::from_secs(2 * 60 + 59))
        );
        assert!(
            error.to_string().contains("is 2m 59s, shorter than 3m"),
            "{error}"
        );
        assert!(matches!(
            Policy::parse(&with_lease("lease = \"soon\"")),
            Err(PolicyError::Parse(_))
        ));
        assert!(matches!(
            Policy::parse(&with_lease("lease = \"3m\"\nrenew = \"1m\"")),
            Err(PolicyError::Parse(_))
        ));
    }
}
