//! 纯 B1 domain value 与事实；这些类型不是 wire DTO。
#![allow(
    clippy::missing_errors_doc,
    reason = "constructors consistently return the module's typed DomainError"
)]

use serde::{Deserialize, Serialize};
use sha2::Digest;
use thiserror::Error;

macro_rules! domain_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, DomainError> {
                let value = value.into();
                validate_text(&value, true).map_err(|_| DomainError::InvalidDomainId)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

domain_id!(DomainProjectId);
domain_id!(ScoreId);
domain_id!(BriefRevisionId);
domain_id!(ConstraintId);
domain_id!(TakeId);
domain_id!(BranchId);
domain_id!(RevisionId);
domain_id!(EvidenceId);
domain_id!(StablePartId);
domain_id!(MarkerId);
domain_id!(CommandId);

fn validate_text(value: &str, identifier: bool) -> Result<(), DomainError> {
    if value.is_empty()
        || value.len() > 128
        || value.chars().any(char::is_control)
        || (identifier && (value == "." || value == ".." || value.contains(['/', '\\'])))
    {
        return Err(DomainError::InvalidDomainValue);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SchemaVersion(u32);

impl SchemaVersion {
    pub fn new(value: u32) -> Result<Self, DomainError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(DomainError::InvalidDomainValue)
    }

    #[must_use]
    pub const fn project_event_v1() -> Self {
        Self(1)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum MusicalScope {
    WholeScore,
    StablePart(StablePartId),
    MarkerRange { from: MarkerId, to: MarkerId },
}

impl MusicalScope {
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        matches!(self, Self::WholeScore) || self == other
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ConstraintStrength {
    Hard,
    Soft,
    Advisory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ConstraintOutcome {
    Pass,
    Fail,
    Unknown,
    NotApplicable,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CreativeBrief {
    pub id: BriefRevisionId,
    pub project_id: DomainProjectId,
    pub previous: Option<BriefRevisionId>,
    pub user_description: String,
    pub goals: Vec<String>,
    pub instrumentation: Vec<String>,
    pub open_questions: Vec<String>,
}

impl CreativeBrief {
    pub fn validate(&self) -> Result<(), DomainError> {
        validate_text(&self.user_description, false)?;
        for value in self
            .goals
            .iter()
            .chain(&self.instrumentation)
            .chain(&self.open_questions)
        {
            validate_text(value, false)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Constraint {
    pub id: ConstraintId,
    pub brief_revision_id: BriefRevisionId,
    pub strength: ConstraintStrength,
    pub description: String,
    pub machine_rule: Option<(String, SchemaVersion)>,
    pub scope: MusicalScope,
}

impl Constraint {
    pub fn validate(&self) -> Result<(), DomainError> {
        validate_text(&self.description, false)?;
        if let Some((key, _)) = &self.machine_rule {
            validate_text(key, true)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ArtifactHash(String);

impl ArtifactHash {
    pub fn parse(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        let hex = value.strip_prefix("sha256:").unwrap_or(&value);
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(DomainError::InvalidDomainValue);
        }
        Ok(Self(format!("sha256:{hex}")))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum RevisionOrigin {
    Human,
    Agent,
    DeterministicFixture,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ScoreRevision {
    pub id: RevisionId,
    pub project_id: DomainProjectId,
    pub score_id: ScoreId,
    pub take_id: TakeId,
    pub branch_id: BranchId,
    pub parents: Vec<RevisionId>,
    pub brief_revision_id: BriefRevisionId,
    pub source_artifact: ArtifactHash,
    pub ir_artifact: Option<ArtifactHash>,
    pub origin: RevisionOrigin,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum EvidenceProducer {
    DeterministicFixture,
    Human,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum EvidenceSubject {
    H0(MusicalScope),
    Constraint(ConstraintId, MusicalScope),
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EvidenceEnvelope {
    pub id: EvidenceId,
    pub revision_id: RevisionId,
    pub subject_hash: ArtifactHash,
    pub subject: EvidenceSubject,
    pub outcome: ConstraintOutcome,
    pub producer: EvidenceProducer,
    pub method: String,
    pub artifact_refs: Vec<ArtifactHash>,
    pub created_at: String,
}

impl EvidenceEnvelope {
    pub fn validate(&self) -> Result<(), DomainError> {
        validate_text(&self.method, false)?;
        validate_text(&self.created_at, false)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HumanActor(String);

impl HumanActor {
    #[cfg(test)]
    pub(crate) fn from_authenticated_client(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        validate_text(&value, true)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct HumanDecision {
    pub actor: String,
    pub timestamp: String,
    pub note: String,
    pub source_command: CommandId,
    #[serde(default)]
    authenticated_human: bool,
}

impl HumanDecision {
    pub fn new(
        actor: HumanActor,
        timestamp: impl Into<String>,
        note: impl Into<String>,
        source_command: CommandId,
    ) -> Result<Self, DomainError> {
        let timestamp = timestamp.into();
        let note = note.into();
        validate_text(&timestamp, false)?;
        validate_text(&note, false)?;
        Ok(Self {
            actor: actor.0,
            timestamp,
            note,
            source_command,
            authenticated_human: true,
        })
    }

    #[must_use]
    pub(crate) const fn is_authenticated_human(&self) -> bool {
        self.authenticated_human
    }

    pub(crate) fn trusted_replay(
        actor: String,
        timestamp: String,
        note: String,
        source_command: CommandId,
    ) -> Result<Self, DomainError> {
        validate_text(&actor, true)?;
        validate_text(&timestamp, false)?;
        validate_text(&note, false)?;
        Ok(Self {
            actor,
            timestamp,
            note,
            source_command,
            authenticated_human: true,
        })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ConstraintWaiver {
    pub constraint_id: ConstraintId,
    pub revision_id: RevisionId,
    pub scope: MusicalScope,
    pub actor: String,
    pub reason: String,
    pub timestamp: String,
    #[serde(default)]
    authenticated_human: bool,
}

impl ConstraintWaiver {
    pub fn new(
        constraint_id: ConstraintId,
        revision_id: RevisionId,
        scope: MusicalScope,
        actor: HumanActor,
        reason: impl Into<String>,
        timestamp: impl Into<String>,
    ) -> Result<Self, DomainError> {
        let reason = reason.into();
        let timestamp = timestamp.into();
        validate_text(&reason, false)?;
        validate_text(&timestamp, false)?;
        Ok(Self {
            constraint_id,
            revision_id,
            scope,
            actor: actor.0,
            reason,
            timestamp,
            authenticated_human: true,
        })
    }

    #[must_use]
    pub(crate) const fn is_authenticated_human(&self) -> bool {
        self.authenticated_human
    }

    pub(crate) fn trusted_replay(
        constraint_id: ConstraintId,
        revision_id: RevisionId,
        scope: MusicalScope,
        actor: String,
        reason: String,
        timestamp: String,
    ) -> Result<Self, DomainError> {
        validate_text(&actor, true)?;
        validate_text(&reason, false)?;
        validate_text(&timestamp, false)?;
        Ok(Self {
            constraint_id,
            revision_id,
            scope,
            actor,
            reason,
            timestamp,
            authenticated_human: true,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum RevisionLifecycle {
    Draft,
    Candidate,
    Accepted,
    Rejected,
    Aborted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ArtifactAvailability {
    FixtureOnly,
    VerifiedDurable,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum DurabilityCapability {
    LinuxFileAndDirectorySynced,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ArtifactRecord {
    hash: ArtifactHash,
    size: u64,
    availability: ArtifactAvailability,
    layout_version: Option<u32>,
    store_instance_id: Option<String>,
    durability: Option<DurabilityCapability>,
    store_commit_identity: Option<String>,
}

impl ArtifactRecord {
    #[cfg(test)]
    pub(crate) fn fixture(hash: ArtifactHash, size: u64) -> Self {
        Self {
            hash,
            size,
            availability: ArtifactAvailability::FixtureOnly,
            layout_version: None,
            store_instance_id: None,
            durability: None,
            store_commit_identity: None,
        }
    }

    #[must_use]
    pub fn hash(&self) -> &ArtifactHash {
        &self.hash
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub const fn availability(&self) -> ArtifactAvailability {
        self.availability
    }

    #[must_use]
    pub fn store_commit_identity(&self) -> Option<&str> {
        self.store_commit_identity.as_deref()
    }

    pub(crate) fn verified_durable(
        hash: ArtifactHash,
        size: u64,
        layout_version: u32,
        store_instance_id: String,
        durability: DurabilityCapability,
        store_commit_identity: String,
    ) -> Result<Self, DomainError> {
        validate_text(&store_instance_id, true)?;
        validate_text(&store_commit_identity, true)?;
        Ok(Self {
            hash,
            size,
            availability: ArtifactAvailability::VerifiedDurable,
            layout_version: Some(layout_version),
            store_instance_id: Some(store_instance_id),
            durability: Some(durability),
            store_commit_identity: Some(store_commit_identity),
        })
    }

    pub(crate) fn trusted_fixture(hash: ArtifactHash, size: u64) -> Self {
        Self {
            hash,
            size,
            availability: ArtifactAvailability::FixtureOnly,
            layout_version: None,
            store_instance_id: None,
            durability: None,
            store_commit_identity: None,
        }
    }

    #[must_use]
    pub const fn layout_version(&self) -> Option<u32> {
        self.layout_version
    }

    #[must_use]
    pub fn store_instance_id(&self) -> Option<&str> {
        self.store_instance_id.as_deref()
    }

    #[must_use]
    pub const fn durability(&self) -> Option<DurabilityCapability> {
        self.durability
    }

    pub(crate) fn validate_audit(&self) -> Result<(), DomainError> {
        match self.availability {
            ArtifactAvailability::FixtureOnly => {
                if self.layout_version.is_some()
                    || self.store_instance_id.is_some()
                    || self.durability.is_some()
                    || self.store_commit_identity.is_some()
                {
                    return Err(DomainError::ProjectionCorrupt);
                }
            }
            ArtifactAvailability::VerifiedDurable => {
                let (Some(1), Some(instance), Some(durability), Some(commit)) = (
                    self.layout_version,
                    self.store_instance_id.as_deref(),
                    self.durability,
                    self.store_commit_identity.as_deref(),
                ) else {
                    return Err(DomainError::ProjectionCorrupt);
                };
                if durability != DurabilityCapability::LinuxFileAndDirectorySynced {
                    return Err(DomainError::ProjectionCorrupt);
                }
                let canonical = format!(
                    "alda-artifact-commit-v1\n{}\n{}\n1\n{instance}\nlinux_file_and_directory_synced\n",
                    self.hash.as_str(),
                    self.size
                );
                let expected = format!("sha256:{:x}", sha2::Sha256::digest(canonical.as_bytes()));
                if commit != expected {
                    return Err(DomainError::ProjectionCorrupt);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[allow(
    dead_code,
    reason = "B1 freezes the trusted fact whitelist before B2-B4 wire durable producers exist"
)]
pub(crate) enum ProjectEvent {
    ProjectInitialized {
        project_id: DomainProjectId,
        score_id: ScoreId,
        default_take_id: TakeId,
        default_branch_id: BranchId,
    },
    TakeCreated {
        project_id: DomainProjectId,
        score_id: ScoreId,
        take_id: TakeId,
        common_base: Option<RevisionId>,
        default_branch_id: BranchId,
    },
    BranchCreated {
        project_id: DomainProjectId,
        score_id: ScoreId,
        take_id: TakeId,
        branch_id: BranchId,
        fork_base: Option<RevisionId>,
    },
    BriefRevisionCreated(CreativeBrief),
    ConstraintDeclared(Constraint),
    FixtureArtifactDeclared(ArtifactRecord),
    ArtifactRegistered(ArtifactRecord),
    RevisionCreated(ScoreRevision),
    EvidenceRecorded(EvidenceEnvelope),
    ConstraintWaived(ConstraintWaiver),
    RevisionPromotedToCandidate {
        revision_id: RevisionId,
    },
    RevisionAccepted {
        revision_id: RevisionId,
        decision: HumanDecision,
    },
    RevisionRejected {
        revision_id: RevisionId,
        decision: HumanDecision,
    },
    RevisionAborted {
        revision_id: RevisionId,
        decision: HumanDecision,
    },
    BranchHeadAdvanced {
        branch_id: BranchId,
        expected: Option<RevisionId>,
        new_head: RevisionId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SequencedProjectEvent {
    pub schema_version: SchemaVersion,
    pub sequence: u64,
    pub event: ProjectEvent,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DomainError {
    #[error("invalid domain ID")]
    InvalidDomainId,
    #[error("invalid domain value")]
    InvalidDomainValue,
    #[error("unknown parent")]
    UnknownParent,
    #[error("cross-project reference")]
    CrossProjectReference,
    #[error("cross-score reference")]
    CrossScoreReference,
    #[error("unknown take")]
    UnknownTake,
    #[error("unknown branch")]
    UnknownBranch,
    #[error("invalid fork parent")]
    InvalidForkParent,
    #[error("duplicate parent")]
    DuplicateParent,
    #[error("revision cycle")]
    RevisionCycle,
    #[error("merge is unsupported")]
    UnsupportedMerge,
    #[error("artifact hash mismatch")]
    ArtifactHashMismatch,
    #[error("evidence subject mismatch")]
    EvidenceSubjectMismatch,
    #[error("hard constraint unsatisfied")]
    HardConstraintUnsatisfied,
    #[error("invalid lifecycle transition")]
    InvalidLifecycleTransition,
    #[error("commit conflict")]
    CommitConflict {
        expected: Option<RevisionId>,
        actual: Option<RevisionId>,
    },
    #[error("projection corrupt")]
    ProjectionCorrupt,
    #[error("idempotency conflict")]
    IdempotencyConflict,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_schema_values_and_scope_coverage_fail_closed() {
        for invalid in ["", ".", "..", "a/b", "a\\b", "line\nbreak"] {
            assert_eq!(
                RevisionId::parse(invalid),
                Err(DomainError::InvalidDomainId)
            );
        }
        assert!(RevisionId::parse("r-1").is_ok());
        assert_eq!(SchemaVersion::new(0), Err(DomainError::InvalidDomainValue));
        assert!(SchemaVersion::new(1).is_ok());

        let whole = MusicalScope::WholeScore;
        let violin = MusicalScope::StablePart(StablePartId::parse("violin").expect("part"));
        let cello = MusicalScope::StablePart(StablePartId::parse("cello").expect("part"));
        let range = MusicalScope::MarkerRange {
            from: MarkerId::parse("a").expect("marker"),
            to: MarkerId::parse("b").expect("marker"),
        };
        assert!(whole.covers(&violin));
        assert!(violin.covers(&violin));
        assert!(!violin.covers(&cello));
        assert!(!range.covers(&violin));
    }

    #[test]
    fn brief_constraint_evidence_and_human_values_validate() {
        let project = DomainProjectId::parse("project-1").expect("project");
        let brief_id = BriefRevisionId::parse("brief-1").expect("brief");
        let brief = CreativeBrief {
            id: brief_id.clone(),
            project_id: project,
            previous: None,
            user_description: "Write an etude".to_owned(),
            goals: vec!["clear form".to_owned()],
            instrumentation: vec!["piano".to_owned()],
            open_questions: vec![],
        };
        assert_eq!(brief.validate(), Ok(()));
        let constraint = Constraint {
            id: ConstraintId::parse("constraint-1").expect("constraint"),
            brief_revision_id: brief_id,
            strength: ConstraintStrength::Hard,
            description: "Remain playable".to_owned(),
            machine_rule: Some(("range".to_owned(), SchemaVersion::new(1).expect("schema"))),
            scope: MusicalScope::WholeScore,
        };
        assert_eq!(constraint.validate(), Ok(()));
        assert_ne!(ConstraintOutcome::Unknown, ConstraintOutcome::Pass);
        assert!(HumanActor::from_authenticated_client("human-1").is_ok());
        assert!(HumanActor::from_authenticated_client("agent/tool").is_err());
    }
}
