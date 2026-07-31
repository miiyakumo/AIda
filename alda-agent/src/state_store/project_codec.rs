//! Versioned stored-event DTOs isolated from live domain capabilities.
//!
//! Every deserializable field is a primitive or a codec-owned enum. Conversion
//! into the domain always re-enters the domain validation boundary.

use serde::{Deserialize, Serialize};

use crate::artifact_store::{
    ArtifactAuditPlanV1, ArtifactRecoveryGuard, ArtifactStore, RecoveredArtifactCapability,
};
use crate::domain::{
    ArtifactAvailability, ArtifactHash, ArtifactRecord, BranchId, BriefRevisionId, CommandId,
    Constraint, ConstraintId, ConstraintOutcome, ConstraintStrength, ConstraintWaiver,
    CreativeBrief, DomainError, DomainProjectId, DurabilityCapability, EvidenceEnvelope,
    EvidenceId, EvidenceProducer, EvidenceSubject, HumanDecision, MarkerId, MusicalScope,
    ProjectEvent, RevisionId, RevisionOrigin, SchemaVersion, ScoreId, ScoreRevision, StablePartId,
    TakeId,
};
use crate::state_store::{
    AppendRequest, ReadyProjectWriter, StateStoreError, StoredCommandRecordV1, TransactionCommit,
    TransactionProbe, validate_sha256,
};

fn parse_optional<T>(
    value: Option<String>,
    parse: impl FnOnce(String) -> Result<T, DomainError>,
) -> Result<Option<T>, DomainError> {
    value.map(parse).transpose()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredHumanDecisionV1 {
    actor: String,
    timestamp: String,
    note: String,
    source_command: String,
}

impl StoredHumanDecisionV1 {
    fn from_domain(value: &HumanDecision) -> Self {
        Self {
            actor: value.actor.clone(),
            timestamp: value.timestamp.clone(),
            note: value.note.clone(),
            source_command: value.source_command.as_str().to_owned(),
        }
    }

    fn into_domain(self) -> Result<HumanDecision, DomainError> {
        HumanDecision::trusted_replay(
            self.actor,
            self.timestamp,
            self.note,
            CommandId::parse(self.source_command)?,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum StoredMusicalScopeV1 {
    WholeScore,
    StablePart(String),
    MarkerRange { from: String, to: String },
}

impl StoredMusicalScopeV1 {
    fn from_domain(value: &MusicalScope) -> Self {
        match value {
            MusicalScope::WholeScore => Self::WholeScore,
            MusicalScope::StablePart(id) => Self::StablePart(id.as_str().to_owned()),
            MusicalScope::MarkerRange { from, to } => Self::MarkerRange {
                from: from.as_str().to_owned(),
                to: to.as_str().to_owned(),
            },
        }
    }

    fn into_domain(self) -> Result<MusicalScope, DomainError> {
        Ok(match self {
            Self::WholeScore => MusicalScope::WholeScore,
            Self::StablePart(id) => MusicalScope::StablePart(StablePartId::parse(id)?),
            Self::MarkerRange { from, to } => MusicalScope::MarkerRange {
                from: MarkerId::parse(from)?,
                to: MarkerId::parse(to)?,
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredConstraintWaiverV1 {
    constraint_id: String,
    revision_id: String,
    scope: StoredMusicalScopeV1,
    actor: String,
    reason: String,
    timestamp: String,
}

impl StoredConstraintWaiverV1 {
    fn from_domain(value: &ConstraintWaiver) -> Self {
        Self {
            constraint_id: value.constraint_id.as_str().to_owned(),
            revision_id: value.revision_id.as_str().to_owned(),
            scope: StoredMusicalScopeV1::from_domain(&value.scope),
            actor: value.actor.clone(),
            reason: value.reason.clone(),
            timestamp: value.timestamp.clone(),
        }
    }

    fn into_domain(self) -> Result<ConstraintWaiver, DomainError> {
        ConstraintWaiver::trusted_replay(
            ConstraintId::parse(self.constraint_id)?,
            RevisionId::parse(self.revision_id)?,
            self.scope.into_domain()?,
            self.actor,
            self.reason,
            self.timestamp,
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum StoredArtifactAvailabilityV1 {
    FixtureOnly,
    VerifiedDurable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredArtifactRecordV1 {
    hash: String,
    size: u64,
    availability: StoredArtifactAvailabilityV1,
    layout_version: Option<u32>,
    store_instance_id: Option<String>,
    durability: Option<String>,
    store_commit_identity: Option<String>,
}

impl StoredArtifactRecordV1 {
    fn from_domain(value: &ArtifactRecord) -> Self {
        Self {
            hash: value.hash().as_str().to_owned(),
            size: value.size(),
            availability: match value.availability() {
                ArtifactAvailability::FixtureOnly => StoredArtifactAvailabilityV1::FixtureOnly,
                ArtifactAvailability::VerifiedDurable => {
                    StoredArtifactAvailabilityV1::VerifiedDurable
                }
            },
            layout_version: value.layout_version(),
            store_instance_id: value.store_instance_id().map(ToOwned::to_owned),
            durability: value.durability().map(|capability| match capability {
                DurabilityCapability::LinuxFileAndDirectorySynced => {
                    "linux_file_and_directory_synced".to_owned()
                }
            }),
            store_commit_identity: value.store_commit_identity().map(ToOwned::to_owned),
        }
    }

    fn into_domain(self) -> Result<ArtifactRecord, DomainError> {
        let hash = ArtifactHash::parse(self.hash)?;
        let record = match self.availability {
            StoredArtifactAvailabilityV1::FixtureOnly => {
                if self.layout_version.is_some()
                    || self.store_instance_id.is_some()
                    || self.durability.is_some()
                    || self.store_commit_identity.is_some()
                {
                    return Err(DomainError::ProjectionCorrupt);
                }
                ArtifactRecord::trusted_fixture(hash, self.size)
            }
            StoredArtifactAvailabilityV1::VerifiedDurable => ArtifactRecord::verified_durable(
                hash,
                self.size,
                self.layout_version.ok_or(DomainError::ProjectionCorrupt)?,
                self.store_instance_id
                    .ok_or(DomainError::ProjectionCorrupt)?,
                match self.durability.as_deref() {
                    Some("linux_file_and_directory_synced") => {
                        DurabilityCapability::LinuxFileAndDirectorySynced
                    }
                    _ => return Err(DomainError::ProjectionCorrupt),
                },
                self.store_commit_identity
                    .ok_or(DomainError::ProjectionCorrupt)?,
            )?,
        };
        record.validate_audit()?;
        Ok(record)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredCreativeBriefV1 {
    id: String,
    project_id: String,
    previous: Option<String>,
    user_description: String,
    goals: Vec<String>,
    instrumentation: Vec<String>,
    open_questions: Vec<String>,
}

impl StoredCreativeBriefV1 {
    fn from_domain(value: &CreativeBrief) -> Self {
        Self {
            id: value.id.as_str().to_owned(),
            project_id: value.project_id.as_str().to_owned(),
            previous: value.previous.as_ref().map(|id| id.as_str().to_owned()),
            user_description: value.user_description.clone(),
            goals: value.goals.clone(),
            instrumentation: value.instrumentation.clone(),
            open_questions: value.open_questions.clone(),
        }
    }

    fn into_domain(self) -> Result<CreativeBrief, DomainError> {
        let value = CreativeBrief {
            id: BriefRevisionId::parse(self.id)?,
            project_id: DomainProjectId::parse(self.project_id)?,
            previous: parse_optional(self.previous, BriefRevisionId::parse)?,
            user_description: self.user_description,
            goals: self.goals,
            instrumentation: self.instrumentation,
            open_questions: self.open_questions,
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum StoredConstraintStrengthV1 {
    Hard,
    Soft,
    Advisory,
}

impl From<ConstraintStrength> for StoredConstraintStrengthV1 {
    fn from(value: ConstraintStrength) -> Self {
        match value {
            ConstraintStrength::Hard => Self::Hard,
            ConstraintStrength::Soft => Self::Soft,
            ConstraintStrength::Advisory => Self::Advisory,
        }
    }
}

impl From<StoredConstraintStrengthV1> for ConstraintStrength {
    fn from(value: StoredConstraintStrengthV1) -> Self {
        match value {
            StoredConstraintStrengthV1::Hard => Self::Hard,
            StoredConstraintStrengthV1::Soft => Self::Soft,
            StoredConstraintStrengthV1::Advisory => Self::Advisory,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredConstraintV1 {
    id: String,
    brief_revision_id: String,
    strength: StoredConstraintStrengthV1,
    description: String,
    machine_rule: Option<(String, u32)>,
    scope: StoredMusicalScopeV1,
}

impl StoredConstraintV1 {
    fn from_domain(value: &Constraint) -> Self {
        Self {
            id: value.id.as_str().to_owned(),
            brief_revision_id: value.brief_revision_id.as_str().to_owned(),
            strength: value.strength.into(),
            description: value.description.clone(),
            machine_rule: value
                .machine_rule
                .as_ref()
                .map(|(key, version)| (key.clone(), version.get())),
            scope: StoredMusicalScopeV1::from_domain(&value.scope),
        }
    }

    fn into_domain(self) -> Result<Constraint, DomainError> {
        let value = Constraint {
            id: ConstraintId::parse(self.id)?,
            brief_revision_id: BriefRevisionId::parse(self.brief_revision_id)?,
            strength: self.strength.into(),
            description: self.description,
            machine_rule: self
                .machine_rule
                .map(|(key, version)| Ok((key, SchemaVersion::new(version)?)))
                .transpose()?,
            scope: self.scope.into_domain()?,
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum StoredRevisionOriginV1 {
    Human,
    Agent,
    DeterministicFixture,
}

impl From<RevisionOrigin> for StoredRevisionOriginV1 {
    fn from(value: RevisionOrigin) -> Self {
        match value {
            RevisionOrigin::Human => Self::Human,
            RevisionOrigin::Agent => Self::Agent,
            RevisionOrigin::DeterministicFixture => Self::DeterministicFixture,
        }
    }
}

impl From<StoredRevisionOriginV1> for RevisionOrigin {
    fn from(value: StoredRevisionOriginV1) -> Self {
        match value {
            StoredRevisionOriginV1::Human => Self::Human,
            StoredRevisionOriginV1::Agent => Self::Agent,
            StoredRevisionOriginV1::DeterministicFixture => Self::DeterministicFixture,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredScoreRevisionV1 {
    id: String,
    project_id: String,
    score_id: String,
    take_id: String,
    branch_id: String,
    parents: Vec<String>,
    brief_revision_id: String,
    source_artifact: String,
    ir_artifact: Option<String>,
    origin: StoredRevisionOriginV1,
}

impl StoredScoreRevisionV1 {
    fn from_domain(value: &ScoreRevision) -> Self {
        Self {
            id: value.id.as_str().to_owned(),
            project_id: value.project_id.as_str().to_owned(),
            score_id: value.score_id.as_str().to_owned(),
            take_id: value.take_id.as_str().to_owned(),
            branch_id: value.branch_id.as_str().to_owned(),
            parents: value
                .parents
                .iter()
                .map(|id| id.as_str().to_owned())
                .collect(),
            brief_revision_id: value.brief_revision_id.as_str().to_owned(),
            source_artifact: value.source_artifact.as_str().to_owned(),
            ir_artifact: value
                .ir_artifact
                .as_ref()
                .map(|hash| hash.as_str().to_owned()),
            origin: value.origin.into(),
        }
    }

    fn into_domain(self) -> Result<ScoreRevision, DomainError> {
        Ok(ScoreRevision {
            id: RevisionId::parse(self.id)?,
            project_id: DomainProjectId::parse(self.project_id)?,
            score_id: ScoreId::parse(self.score_id)?,
            take_id: TakeId::parse(self.take_id)?,
            branch_id: BranchId::parse(self.branch_id)?,
            parents: self
                .parents
                .into_iter()
                .map(RevisionId::parse)
                .collect::<Result<_, _>>()?,
            brief_revision_id: BriefRevisionId::parse(self.brief_revision_id)?,
            source_artifact: ArtifactHash::parse(self.source_artifact)?,
            ir_artifact: parse_optional(self.ir_artifact, ArtifactHash::parse)?,
            origin: self.origin.into(),
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum StoredConstraintOutcomeV1 {
    Pass,
    Fail,
    Unknown,
    NotApplicable,
}

impl From<ConstraintOutcome> for StoredConstraintOutcomeV1 {
    fn from(value: ConstraintOutcome) -> Self {
        match value {
            ConstraintOutcome::Pass => Self::Pass,
            ConstraintOutcome::Fail => Self::Fail,
            ConstraintOutcome::Unknown => Self::Unknown,
            ConstraintOutcome::NotApplicable => Self::NotApplicable,
        }
    }
}

impl From<StoredConstraintOutcomeV1> for ConstraintOutcome {
    fn from(value: StoredConstraintOutcomeV1) -> Self {
        match value {
            StoredConstraintOutcomeV1::Pass => Self::Pass,
            StoredConstraintOutcomeV1::Fail => Self::Fail,
            StoredConstraintOutcomeV1::Unknown => Self::Unknown,
            StoredConstraintOutcomeV1::NotApplicable => Self::NotApplicable,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum StoredEvidenceProducerV1 {
    DeterministicFixture,
    Human,
    Tool,
}

impl From<EvidenceProducer> for StoredEvidenceProducerV1 {
    fn from(value: EvidenceProducer) -> Self {
        match value {
            EvidenceProducer::DeterministicFixture => Self::DeterministicFixture,
            EvidenceProducer::Human => Self::Human,
            EvidenceProducer::Tool => Self::Tool,
        }
    }
}

impl From<StoredEvidenceProducerV1> for EvidenceProducer {
    fn from(value: StoredEvidenceProducerV1) -> Self {
        match value {
            StoredEvidenceProducerV1::DeterministicFixture => Self::DeterministicFixture,
            StoredEvidenceProducerV1::Human => Self::Human,
            StoredEvidenceProducerV1::Tool => Self::Tool,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum StoredEvidenceSubjectV1 {
    H0(StoredMusicalScopeV1),
    Constraint(String, StoredMusicalScopeV1),
}

impl StoredEvidenceSubjectV1 {
    fn from_domain(value: &EvidenceSubject) -> Self {
        match value {
            EvidenceSubject::H0(scope) => Self::H0(StoredMusicalScopeV1::from_domain(scope)),
            EvidenceSubject::Constraint(id, scope) => Self::Constraint(
                id.as_str().to_owned(),
                StoredMusicalScopeV1::from_domain(scope),
            ),
        }
    }

    fn into_domain(self) -> Result<EvidenceSubject, DomainError> {
        Ok(match self {
            Self::H0(scope) => EvidenceSubject::H0(scope.into_domain()?),
            Self::Constraint(id, scope) => {
                EvidenceSubject::Constraint(ConstraintId::parse(id)?, scope.into_domain()?)
            }
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredEvidenceEnvelopeV1 {
    id: String,
    revision_id: String,
    subject_hash: String,
    subject: StoredEvidenceSubjectV1,
    outcome: StoredConstraintOutcomeV1,
    producer: StoredEvidenceProducerV1,
    method: String,
    artifact_refs: Vec<String>,
    created_at: String,
}

impl StoredEvidenceEnvelopeV1 {
    fn from_domain(value: &EvidenceEnvelope) -> Self {
        Self {
            id: value.id.as_str().to_owned(),
            revision_id: value.revision_id.as_str().to_owned(),
            subject_hash: value.subject_hash.as_str().to_owned(),
            subject: StoredEvidenceSubjectV1::from_domain(&value.subject),
            outcome: value.outcome.into(),
            producer: value.producer.into(),
            method: value.method.clone(),
            artifact_refs: value
                .artifact_refs
                .iter()
                .map(|hash| hash.as_str().to_owned())
                .collect(),
            created_at: value.created_at.clone(),
        }
    }

    fn into_domain(self) -> Result<EvidenceEnvelope, DomainError> {
        let value = EvidenceEnvelope {
            id: EvidenceId::parse(self.id)?,
            revision_id: RevisionId::parse(self.revision_id)?,
            subject_hash: ArtifactHash::parse(self.subject_hash)?,
            subject: self.subject.into_domain()?,
            outcome: self.outcome.into(),
            producer: self.producer.into(),
            method: self.method,
            artifact_refs: self
                .artifact_refs
                .into_iter()
                .map(ArtifactHash::parse)
                .collect::<Result<_, _>>()?,
            created_at: self.created_at,
        };
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    content = "value",
    rename_all = "snake_case"
)]
#[allow(
    private_interfaces,
    reason = "the stored event is visible to its parent framing module, while nested codec DTOs remain encapsulated"
)]
pub(crate) enum StoredProjectEventV1 {
    ProjectInitialized {
        project_id: String,
        score_id: String,
        default_take_id: String,
        default_branch_id: String,
    },
    TakeCreated {
        project_id: String,
        score_id: String,
        take_id: String,
        common_base: Option<String>,
        default_branch_id: String,
    },
    BranchCreated {
        project_id: String,
        score_id: String,
        take_id: String,
        branch_id: String,
        fork_base: Option<String>,
    },
    BriefRevisionCreated(StoredCreativeBriefV1),
    ConstraintDeclared(StoredConstraintV1),
    FixtureArtifactDeclared(StoredArtifactRecordV1),
    ArtifactRegistered(StoredArtifactRecordV1),
    RevisionCreated(StoredScoreRevisionV1),
    EvidenceRecorded(StoredEvidenceEnvelopeV1),
    ConstraintWaived(StoredConstraintWaiverV1),
    RevisionPromotedToCandidate {
        revision_id: String,
    },
    RevisionAccepted {
        revision_id: String,
        decision: StoredHumanDecisionV1,
    },
    RevisionRejected {
        revision_id: String,
        decision: StoredHumanDecisionV1,
    },
    RevisionAborted {
        revision_id: String,
        decision: StoredHumanDecisionV1,
    },
    BranchHeadAdvanced {
        branch_id: String,
        expected: Option<String>,
        new_head: String,
    },
}

impl StoredProjectEventV1 {
    pub(crate) fn from_domain(value: &ProjectEvent) -> Self {
        match value {
            ProjectEvent::ProjectInitialized {
                project_id,
                score_id,
                default_take_id,
                default_branch_id,
            } => Self::ProjectInitialized {
                project_id: project_id.as_str().to_owned(),
                score_id: score_id.as_str().to_owned(),
                default_take_id: default_take_id.as_str().to_owned(),
                default_branch_id: default_branch_id.as_str().to_owned(),
            },
            ProjectEvent::TakeCreated {
                project_id,
                score_id,
                take_id,
                common_base,
                default_branch_id,
            } => Self::TakeCreated {
                project_id: project_id.as_str().to_owned(),
                score_id: score_id.as_str().to_owned(),
                take_id: take_id.as_str().to_owned(),
                common_base: common_base.as_ref().map(|id| id.as_str().to_owned()),
                default_branch_id: default_branch_id.as_str().to_owned(),
            },
            ProjectEvent::BranchCreated {
                project_id,
                score_id,
                take_id,
                branch_id,
                fork_base,
            } => Self::BranchCreated {
                project_id: project_id.as_str().to_owned(),
                score_id: score_id.as_str().to_owned(),
                take_id: take_id.as_str().to_owned(),
                branch_id: branch_id.as_str().to_owned(),
                fork_base: fork_base.as_ref().map(|id| id.as_str().to_owned()),
            },
            ProjectEvent::BriefRevisionCreated(value) => {
                Self::BriefRevisionCreated(StoredCreativeBriefV1::from_domain(value))
            }
            ProjectEvent::ConstraintDeclared(value) => {
                Self::ConstraintDeclared(StoredConstraintV1::from_domain(value))
            }
            ProjectEvent::FixtureArtifactDeclared(value) => {
                Self::FixtureArtifactDeclared(StoredArtifactRecordV1::from_domain(value))
            }
            ProjectEvent::ArtifactRegistered(value) => {
                Self::ArtifactRegistered(StoredArtifactRecordV1::from_domain(value))
            }
            ProjectEvent::RevisionCreated(value) => {
                Self::RevisionCreated(StoredScoreRevisionV1::from_domain(value))
            }
            ProjectEvent::EvidenceRecorded(value) => {
                Self::EvidenceRecorded(StoredEvidenceEnvelopeV1::from_domain(value))
            }
            ProjectEvent::ConstraintWaived(value) => {
                Self::ConstraintWaived(StoredConstraintWaiverV1::from_domain(value))
            }
            ProjectEvent::RevisionPromotedToCandidate { revision_id } => {
                Self::RevisionPromotedToCandidate {
                    revision_id: revision_id.as_str().to_owned(),
                }
            }
            ProjectEvent::RevisionAccepted {
                revision_id,
                decision,
            } => Self::RevisionAccepted {
                revision_id: revision_id.as_str().to_owned(),
                decision: StoredHumanDecisionV1::from_domain(decision),
            },
            ProjectEvent::RevisionRejected {
                revision_id,
                decision,
            } => Self::RevisionRejected {
                revision_id: revision_id.as_str().to_owned(),
                decision: StoredHumanDecisionV1::from_domain(decision),
            },
            ProjectEvent::RevisionAborted {
                revision_id,
                decision,
            } => Self::RevisionAborted {
                revision_id: revision_id.as_str().to_owned(),
                decision: StoredHumanDecisionV1::from_domain(decision),
            },
            ProjectEvent::BranchHeadAdvanced {
                branch_id,
                expected,
                new_head,
            } => Self::BranchHeadAdvanced {
                branch_id: branch_id.as_str().to_owned(),
                expected: expected.as_ref().map(|id| id.as_str().to_owned()),
                new_head: new_head.as_str().to_owned(),
            },
        }
    }

    pub(crate) fn into_domain(self) -> Result<ProjectEvent, DomainError> {
        Ok(match self {
            Self::ProjectInitialized {
                project_id,
                score_id,
                default_take_id,
                default_branch_id,
            } => ProjectEvent::ProjectInitialized {
                project_id: DomainProjectId::parse(project_id)?,
                score_id: ScoreId::parse(score_id)?,
                default_take_id: TakeId::parse(default_take_id)?,
                default_branch_id: BranchId::parse(default_branch_id)?,
            },
            Self::TakeCreated {
                project_id,
                score_id,
                take_id,
                common_base,
                default_branch_id,
            } => ProjectEvent::TakeCreated {
                project_id: DomainProjectId::parse(project_id)?,
                score_id: ScoreId::parse(score_id)?,
                take_id: TakeId::parse(take_id)?,
                common_base: parse_optional(common_base, RevisionId::parse)?,
                default_branch_id: BranchId::parse(default_branch_id)?,
            },
            Self::BranchCreated {
                project_id,
                score_id,
                take_id,
                branch_id,
                fork_base,
            } => ProjectEvent::BranchCreated {
                project_id: DomainProjectId::parse(project_id)?,
                score_id: ScoreId::parse(score_id)?,
                take_id: TakeId::parse(take_id)?,
                branch_id: BranchId::parse(branch_id)?,
                fork_base: parse_optional(fork_base, RevisionId::parse)?,
            },
            Self::BriefRevisionCreated(value) => {
                ProjectEvent::BriefRevisionCreated(value.into_domain()?)
            }
            Self::ConstraintDeclared(value) => {
                ProjectEvent::ConstraintDeclared(value.into_domain()?)
            }
            Self::FixtureArtifactDeclared(value) => {
                ProjectEvent::FixtureArtifactDeclared(value.into_domain()?)
            }
            Self::ArtifactRegistered(value) => {
                ProjectEvent::ArtifactRegistered(value.into_domain()?)
            }
            Self::RevisionCreated(value) => ProjectEvent::RevisionCreated(value.into_domain()?),
            Self::EvidenceRecorded(value) => ProjectEvent::EvidenceRecorded(value.into_domain()?),
            Self::ConstraintWaived(value) => ProjectEvent::ConstraintWaived(value.into_domain()?),
            Self::RevisionPromotedToCandidate { revision_id } => {
                ProjectEvent::RevisionPromotedToCandidate {
                    revision_id: RevisionId::parse(revision_id)?,
                }
            }
            Self::RevisionAccepted {
                revision_id,
                decision,
            } => ProjectEvent::RevisionAccepted {
                revision_id: RevisionId::parse(revision_id)?,
                decision: decision.into_domain()?,
            },
            Self::RevisionRejected {
                revision_id,
                decision,
            } => ProjectEvent::RevisionRejected {
                revision_id: RevisionId::parse(revision_id)?,
                decision: decision.into_domain()?,
            },
            Self::RevisionAborted {
                revision_id,
                decision,
            } => ProjectEvent::RevisionAborted {
                revision_id: RevisionId::parse(revision_id)?,
                decision: decision.into_domain()?,
            },
            Self::BranchHeadAdvanced {
                branch_id,
                expected,
                new_head,
            } => ProjectEvent::BranchHeadAdvanced {
                branch_id: BranchId::parse(branch_id)?,
                expected: parse_optional(expected, RevisionId::parse)?,
                new_head: RevisionId::parse(new_head)?,
            },
        })
    }

    pub(crate) const fn is_artifact_registered(&self) -> bool {
        matches!(self, Self::ArtifactRegistered(_))
    }
}

/// Primitive-only Project redo plan frozen in the control WAL.
///
/// Deserialization reconstructs no live Artifact capability. Registered
/// Artifact facts must be supplied separately after a same-handle Store audit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredProjectPlanV1 {
    schema_version: u32,
    project_id: String,
    expected_pre_sequence: u64,
    expected_pre_batch_checksum: Option<String>,
    transaction_id: String,
    command_record: Option<StoredCommandRecordV1>,
    events: Vec<StoredProjectEventV1>,
    canonical_plan_digest: String,
}

impl StoredProjectPlanV1 {
    pub(crate) fn from_append_request(
        project_id: &DomainProjectId,
        expected_pre_sequence: u64,
        expected_pre_batch_checksum: Option<String>,
        request: &AppendRequest,
    ) -> Result<Self, StateStoreError> {
        if let Some(checksum) = &expected_pre_batch_checksum {
            validate_sha256(checksum)?;
        }
        let events = request
            .events
            .iter()
            .map(StoredProjectEventV1::from_domain)
            .collect::<Vec<_>>();
        let canonical_plan_digest = request.canonical_plan_digest(project_id)?;
        let plan = Self {
            schema_version: 1,
            project_id: project_id.as_str().to_owned(),
            expected_pre_sequence,
            expected_pre_batch_checksum,
            transaction_id: request.transaction_id.clone(),
            command_record: request.command_record.clone(),
            events,
            canonical_plan_digest,
        };
        plan.validate_shape()?;
        Ok(plan)
    }

    pub(crate) fn project_id(&self) -> Result<DomainProjectId, StateStoreError> {
        DomainProjectId::parse(self.project_id.clone())
            .map_err(|_| StateStoreError::IncompatibleSchema)
    }

    pub(crate) const fn expected_pre_sequence(&self) -> u64 {
        self.expected_pre_sequence
    }

    pub(crate) fn expected_pre_batch_checksum(&self) -> Option<&str> {
        self.expected_pre_batch_checksum.as_deref()
    }

    pub(crate) fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub(crate) fn canonical_plan_digest(&self) -> &str {
        &self.canonical_plan_digest
    }

    pub(crate) fn command_record(&self) -> Option<&StoredCommandRecordV1> {
        self.command_record.as_ref()
    }

    pub(crate) fn validate(&self) -> Result<(), StateStoreError> {
        self.validate_shape()
    }

    pub(crate) fn registered_artifact_events(&self) -> Vec<StoredProjectEventV1> {
        self.events
            .iter()
            .filter(|event| event.is_artifact_registered())
            .cloned()
            .collect()
    }

    pub(crate) fn into_append_request(
        self,
        recovered_artifacts: Vec<ProjectEvent>,
    ) -> Result<AppendRequest, StateStoreError> {
        self.validate_shape()?;
        let mut recovered = recovered_artifacts.into_iter();
        let mut events = Vec::with_capacity(self.events.len());
        for stored in self.events {
            if stored.is_artifact_registered() {
                let Some(event) = recovered.next() else {
                    return Err(StateStoreError::ArtifactRecoveryRejected);
                };
                if !matches!(event, ProjectEvent::ArtifactRegistered(_)) {
                    return Err(StateStoreError::ArtifactRecoveryRejected);
                }
                events.push(event);
            } else {
                events.push(
                    stored
                        .into_domain()
                        .map_err(|_| StateStoreError::ProjectionRejected)?,
                );
            }
        }
        if recovered.next().is_some() {
            return Err(StateStoreError::ArtifactRecoveryRejected);
        }
        let project_id = DomainProjectId::parse(self.project_id)
            .map_err(|_| StateStoreError::IncompatibleSchema)?;
        let request = AppendRequest {
            transaction_id: self.transaction_id,
            command_record: self.command_record,
            events,
        };
        if request.canonical_plan_digest(&project_id)? != self.canonical_plan_digest {
            return Err(StateStoreError::ChecksumMismatch);
        }
        Ok(request)
    }

    fn validate_shape(&self) -> Result<(), StateStoreError> {
        if self.schema_version != 1 || self.transaction_id.is_empty() || self.events.is_empty() {
            return Err(StateStoreError::IncompatibleSchema);
        }
        if let Some(checksum) = &self.expected_pre_batch_checksum {
            validate_sha256(checksum)?;
        } else if self.expected_pre_sequence != 0 {
            return Err(StateStoreError::IncompatibleSchema);
        }
        validate_sha256(&self.canonical_plan_digest)?;
        let project_id = DomainProjectId::parse(self.project_id.clone())
            .map_err(|_| StateStoreError::IncompatibleSchema)?;
        let domain_events = self
            .events
            .iter()
            .filter(|event| !event.is_artifact_registered())
            .cloned()
            .map(|event| {
                event
                    .into_domain()
                    .map_err(|_| StateStoreError::ProjectionRejected)
            })
            .collect::<Result<Vec<_>, _>>()?;
        // A complete digest check is deferred until audited Artifact facts are
        // supplied; plans without Artifact registration can be checked now.
        if self.registered_artifact_events().is_empty() {
            let request = AppendRequest {
                transaction_id: self.transaction_id.clone(),
                command_record: self.command_record.clone(),
                events: domain_events,
            };
            if request.canonical_plan_digest(&project_id)? != self.canonical_plan_digest {
                return Err(StateStoreError::ChecksumMismatch);
            }
        }
        Ok(())
    }
}

/// Consumes one reverified Artifact capability into exactly the
/// `ArtifactRegistered` fact frozen by the corresponding stored Project plan.
///
/// Ordinary stored-log replay continues through [`StoredProjectEventV1::into_domain`].
/// Control-WAL redo must use this handoff for an Artifact registration so that
/// primitive audit data alone can never manufacture a live Project fact.
pub(crate) fn recovered_artifact_registered_event(
    stored_event: StoredProjectEventV1,
    audit_plan: &ArtifactAuditPlanV1,
    capability: RecoveredArtifactCapability,
) -> Result<ProjectEvent, StateStoreError> {
    let StoredProjectEventV1::ArtifactRegistered(stored_record) = stored_event else {
        return Err(StateStoreError::ArtifactRecoveryRejected);
    };
    let expected_record = stored_record
        .into_domain()
        .map_err(|_| StateStoreError::ArtifactRecoveryRejected)?;
    let recovered_record = capability
        .into_project_record(audit_plan)
        .map_err(|_| StateStoreError::ArtifactRecoveryRejected)?;
    if expected_record != recovered_record {
        return Err(StateStoreError::ArtifactRecoveryRejected);
    }
    Ok(ProjectEvent::ArtifactRegistered(recovered_record))
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RecoveredArtifactProjectHandoff {
    Append(ProjectEvent),
    AlreadyCommitted(TransactionCommit),
}

/// Executes the recovery-only Artifact handoff behind the Project transaction
/// probe.
///
/// `Absent` reaudits and yields one append event. `SamePlanCommitted` returns
/// the durable result without minting another capability. A conflicting plan
/// fails closed.
#[allow(
    clippy::too_many_arguments,
    reason = "all independent control, Project, and audit bindings are explicit at this trust boundary"
)]
pub(crate) fn recover_artifact_for_project_plan(
    writer: &ReadyProjectWriter,
    transaction_id: &str,
    canonical_plan_digest: &str,
    artifact_store: &ArtifactStore,
    recovery_guard: &ArtifactRecoveryGuard,
    expected_control_transaction_id: &str,
    stored_event: StoredProjectEventV1,
    audit_plan: &ArtifactAuditPlanV1,
) -> Result<RecoveredArtifactProjectHandoff, StateStoreError> {
    match writer.probe_transaction(transaction_id, canonical_plan_digest) {
        TransactionProbe::Absent => {
            let capability = artifact_store
                .audit_recovery_artifact(
                    recovery_guard,
                    expected_control_transaction_id,
                    audit_plan,
                )
                .map_err(|_| StateStoreError::ArtifactRecoveryRejected)?;
            recovered_artifact_registered_event(stored_event, audit_plan, capability)
                .map(RecoveredArtifactProjectHandoff::Append)
        }
        TransactionProbe::SamePlanCommitted(committed) => {
            Ok(RecoveredArtifactProjectHandoff::AlreadyCommitted(committed))
        }
        TransactionProbe::ConflictingPlan => Err(StateStoreError::IdempotencyConflict),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::artifact_store::ArtifactStore;

    use super::*;

    fn decode(json: serde_json::Value) -> Result<ProjectEvent, DomainError> {
        serde_json::from_value::<StoredProjectEventV1>(json)
            .expect("stored shape")
            .into_domain()
    }

    #[test]
    fn primitive_codec_rejects_invalid_nested_values() {
        let invalid_id = serde_json::json!({
            "type": "project_initialized",
            "value": {
                "project_id": "../escape",
                "score_id": "score-1",
                "default_take_id": "take-1",
                "default_branch_id": "branch-1"
            }
        });
        assert_eq!(decode(invalid_id), Err(DomainError::InvalidDomainId));

        let invalid_hash = serde_json::json!({
            "type": "fixture_artifact_declared",
            "value": {
                "hash": "sha256:NOT-CANONICAL",
                "size": 1,
                "availability": "FixtureOnly",
                "layout_version": null,
                "store_instance_id": null,
                "durability": null,
                "store_commit_identity": null
            }
        });
        assert_eq!(decode(invalid_hash), Err(DomainError::InvalidDomainValue));

        let zero_schema = serde_json::json!({
            "type": "constraint_declared",
            "value": {
                "id": "constraint-1",
                "brief_revision_id": "brief-1",
                "strength": "Hard",
                "description": "playable",
                "machine_rule": ["range", 0],
                "scope": "WholeScore"
            }
        });
        assert_eq!(decode(zero_schema), Err(DomainError::InvalidDomainValue));

        let invalid_scope = serde_json::json!({
            "type": "constraint_declared",
            "value": {
                "id": "constraint-1",
                "brief_revision_id": "brief-1",
                "strength": "Hard",
                "description": "playable",
                "machine_rule": null,
                "scope": {"StablePart": "../part"}
            }
        });
        assert_eq!(decode(invalid_scope), Err(DomainError::InvalidDomainId));

        let invalid_revision = serde_json::json!({
            "type": "revision_created",
            "value": {
                "id": "revision-1",
                "project_id": "project-1",
                "score_id": "score-1",
                "take_id": "take-1",
                "branch_id": "branch-1",
                "parents": ["bad/parent"],
                "brief_revision_id": "brief-1",
                "source_artifact": format!("sha256:{}", "a".repeat(64)),
                "ir_artifact": null,
                "origin": "Agent"
            }
        });
        assert_eq!(decode(invalid_revision), Err(DomainError::InvalidDomainId));

        let invalid_evidence = serde_json::json!({
            "type": "evidence_recorded",
            "value": {
                "id": "evidence-1",
                "revision_id": "revision-1",
                "subject_hash": format!("sha256:{}", "a".repeat(64)),
                "subject": {"Constraint": ["bad/id", "WholeScore"]},
                "outcome": "Pass",
                "producer": "Tool",
                "method": "lint",
                "artifact_refs": [],
                "created_at": "now"
            }
        });
        assert_eq!(decode(invalid_evidence), Err(DomainError::InvalidDomainId));
    }

    #[test]
    fn recovered_capability_only_hands_off_to_its_artifact_registered_plan() {
        let root = tempfile::tempdir().expect("root");
        let (store, guard) =
            ArtifactStore::open_for_durable_runtime(root.path()).expect("runtime store");
        let receipt = store.put(Cursor::new(b"fixture"), None).expect("put");
        let audit_plan = receipt
            .recovery_audit_plan("control-tx:project-artifact")
            .expect("audit plan");
        let expected_record = receipt.into_record().expect("live receipt record");
        let stored_event = StoredProjectEventV1::ArtifactRegistered(
            StoredArtifactRecordV1::from_domain(&expected_record),
        );
        let capability = store
            .audit_recovery_artifact(&guard, audit_plan.control_transaction_id(), &audit_plan)
            .expect("reaudit");

        assert_eq!(
            recovered_artifact_registered_event(stored_event, &audit_plan, capability)
                .expect("trusted Project handoff"),
            ProjectEvent::ArtifactRegistered(expected_record)
        );

        let capability = store
            .audit_recovery_artifact(&guard, audit_plan.control_transaction_id(), &audit_plan)
            .expect("new capability while aggregate retry remains absent");
        let wrong_event = StoredProjectEventV1::FixtureArtifactDeclared(
            StoredArtifactRecordV1::from_domain(&ArtifactRecord::trusted_fixture(
                ArtifactHash::parse(format!("sha256:{}", "a".repeat(64))).expect("hash"),
                7,
            )),
        );
        assert!(matches!(
            recovered_artifact_registered_event(wrong_event, &audit_plan, capability),
            Err(StateStoreError::ArtifactRecoveryRejected)
        ));
    }

    #[test]
    fn recovered_capability_rejects_a_different_registered_artifact() {
        let root = tempfile::tempdir().expect("root");
        let (store, guard) =
            ArtifactStore::open_for_durable_runtime(root.path()).expect("runtime store");
        let receipt = store.put(Cursor::new(b"fixture"), None).expect("put");
        let audit_plan = receipt
            .recovery_audit_plan("control-tx:project-mismatch")
            .expect("audit plan");
        let other = store
            .put(Cursor::new(b"different"), None)
            .expect("other put");
        let other_record = other.into_record().expect("other record");
        let wrong_stored_event = StoredProjectEventV1::ArtifactRegistered(
            StoredArtifactRecordV1::from_domain(&other_record),
        );
        let capability = store
            .audit_recovery_artifact(&guard, audit_plan.control_transaction_id(), &audit_plan)
            .expect("reaudit");

        assert!(matches!(
            recovered_artifact_registered_event(wrong_stored_event, &audit_plan, capability),
            Err(StateStoreError::ArtifactRecoveryRejected)
        ));
    }
}
