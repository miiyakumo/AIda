//! Deterministic in-memory B1 reducer and coordinator.
#![allow(
    clippy::missing_errors_doc,
    reason = "public mutations consistently return the domain error contract"
)]
#![allow(
    clippy::wildcard_imports,
    reason = "the reducer intentionally consumes the complete internal domain vocabulary"
)]

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::artifact_store::CommittedArtifactReceipt;
use crate::domain::*;

impl TryFrom<&crate::protocol::ProjectId> for DomainProjectId {
    type Error = DomainError;

    fn try_from(value: &crate::protocol::ProjectId) -> Result<Self, Self::Error> {
        Self::parse(value.0.clone())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TakeProjection {
    pub score_id: ScoreId,
    pub common_base: Option<RevisionId>,
    pub branches: BTreeSet<BranchId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BranchProjection {
    pub score_id: ScoreId,
    pub take_id: TakeId,
    pub fork_base: Option<RevisionId>,
    pub head: Option<RevisionId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ProjectSnapshot {
    pub project_id: Option<DomainProjectId>,
    pub score_id: Option<ScoreId>,
    pub active_brief: Option<BriefRevisionId>,
    pub briefs: BTreeMap<BriefRevisionId, CreativeBrief>,
    pub constraints: BTreeMap<ConstraintId, Constraint>,
    pub revisions: BTreeMap<RevisionId, ScoreRevision>,
    pub evidence: BTreeMap<EvidenceId, EvidenceEnvelope>,
    pub waivers: Vec<ConstraintWaiver>,
    pub takes: BTreeMap<TakeId, TakeProjection>,
    pub branches: BTreeMap<BranchId, BranchProjection>,
    pub lifecycle: BTreeMap<RevisionId, RevisionLifecycle>,
    pub accepted_revision: Option<RevisionId>,
    pub artifacts: BTreeMap<ArtifactHash, ArtifactRecord>,
    pub last_sequence: u64,
}

impl ProjectSnapshot {
    pub fn canonical_digest(&self) -> Result<String, DomainError> {
        let canonical = serde_json::to_vec(&("b1-project-projection-v1", self))
            .map_err(|_| DomainError::ProjectionCorrupt)?;
        Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
    }

    pub(crate) fn apply(&mut self, envelope: &SequencedProjectEvent) -> Result<(), DomainError> {
        if envelope.schema_version != SchemaVersion::project_event_v1()
            || envelope.sequence != self.last_sequence + 1
        {
            return Err(DomainError::ProjectionCorrupt);
        }
        self.apply_event(&envelope.event)?;
        self.last_sequence = envelope.sequence;
        Ok(())
    }

    fn same_project_score(
        &self,
        project: &DomainProjectId,
        score: &ScoreId,
    ) -> Result<(), DomainError> {
        if self.project_id.as_ref() != Some(project) {
            return Err(DomainError::CrossProjectReference);
        }
        if self.score_id.as_ref() != Some(score) {
            return Err(DomainError::CrossScoreReference);
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "a single exhaustive event whitelist makes reducer coverage auditable"
    )]
    fn apply_event(&mut self, event: &ProjectEvent) -> Result<(), DomainError> {
        match event {
            ProjectEvent::ProjectInitialized {
                project_id,
                score_id,
                default_take_id,
                default_branch_id,
            } => {
                if self.project_id.is_some() {
                    return Err(DomainError::ProjectionCorrupt);
                }
                self.project_id = Some(project_id.clone());
                self.score_id = Some(score_id.clone());
                self.takes.insert(
                    default_take_id.clone(),
                    TakeProjection {
                        score_id: score_id.clone(),
                        common_base: None,
                        branches: BTreeSet::from([default_branch_id.clone()]),
                    },
                );
                self.branches.insert(
                    default_branch_id.clone(),
                    BranchProjection {
                        score_id: score_id.clone(),
                        take_id: default_take_id.clone(),
                        fork_base: None,
                        head: None,
                    },
                );
            }
            ProjectEvent::TakeCreated {
                project_id,
                score_id,
                take_id,
                common_base,
                default_branch_id,
            } => {
                self.same_project_score(project_id, score_id)?;
                if self.takes.contains_key(take_id) || self.branches.contains_key(default_branch_id)
                {
                    return Err(DomainError::ProjectionCorrupt);
                }
                self.validate_fork(common_base.as_ref())?;
                self.takes.insert(
                    take_id.clone(),
                    TakeProjection {
                        score_id: score_id.clone(),
                        common_base: common_base.clone(),
                        branches: BTreeSet::from([default_branch_id.clone()]),
                    },
                );
                self.branches.insert(
                    default_branch_id.clone(),
                    BranchProjection {
                        score_id: score_id.clone(),
                        take_id: take_id.clone(),
                        fork_base: common_base.clone(),
                        head: None,
                    },
                );
            }
            ProjectEvent::BranchCreated {
                project_id,
                score_id,
                take_id,
                branch_id,
                fork_base,
            } => {
                self.same_project_score(project_id, score_id)?;
                self.validate_fork(fork_base.as_ref())?;
                let take = self
                    .takes
                    .get_mut(take_id)
                    .ok_or(DomainError::UnknownTake)?;
                if self.branches.contains_key(branch_id) {
                    return Err(DomainError::ProjectionCorrupt);
                }
                take.branches.insert(branch_id.clone());
                self.branches.insert(
                    branch_id.clone(),
                    BranchProjection {
                        score_id: score_id.clone(),
                        take_id: take_id.clone(),
                        fork_base: fork_base.clone(),
                        head: None,
                    },
                );
            }
            ProjectEvent::BriefRevisionCreated(brief) => {
                brief.validate()?;
                if self.project_id.as_ref() != Some(&brief.project_id)
                    || brief
                        .previous
                        .as_ref()
                        .is_some_and(|id| !self.briefs.contains_key(id))
                    || self.briefs.contains_key(&brief.id)
                {
                    return Err(DomainError::ProjectionCorrupt);
                }
                self.active_brief = Some(brief.id.clone());
                self.briefs.insert(brief.id.clone(), brief.clone());
            }
            ProjectEvent::ConstraintDeclared(constraint) => {
                constraint.validate()?;
                if !self.briefs.contains_key(&constraint.brief_revision_id)
                    || self.constraints.contains_key(&constraint.id)
                {
                    return Err(DomainError::ProjectionCorrupt);
                }
                self.constraints
                    .insert(constraint.id.clone(), constraint.clone());
            }
            ProjectEvent::FixtureArtifactDeclared(record) => {
                if record.availability() != ArtifactAvailability::FixtureOnly {
                    return Err(DomainError::ProjectionCorrupt);
                }
                record.validate_audit()?;
                self.insert_artifact(record)?;
            }
            ProjectEvent::ArtifactRegistered(record) => {
                if record.availability() != ArtifactAvailability::VerifiedDurable {
                    return Err(DomainError::ProjectionCorrupt);
                }
                record.validate_audit()?;
                self.insert_artifact(record)?;
            }
            ProjectEvent::RevisionCreated(revision) => self.create_revision(revision)?,
            ProjectEvent::EvidenceRecorded(evidence) => {
                evidence.validate()?;
                let revision = self
                    .revisions
                    .get(&evidence.revision_id)
                    .ok_or(DomainError::UnknownParent)?;
                if revision.source_artifact != evidence.subject_hash {
                    return Err(DomainError::EvidenceSubjectMismatch);
                }
                if let EvidenceSubject::Constraint(id, _) = &evidence.subject
                    && !self.constraints.contains_key(id)
                {
                    return Err(DomainError::ProjectionCorrupt);
                }
                if self
                    .evidence
                    .insert(evidence.id.clone(), evidence.clone())
                    .is_some()
                {
                    return Err(DomainError::ProjectionCorrupt);
                }
            }
            ProjectEvent::ConstraintWaived(waiver) => {
                let constraint = self
                    .constraints
                    .get(&waiver.constraint_id)
                    .ok_or(DomainError::HardConstraintUnsatisfied)?;
                if !self.revisions.contains_key(&waiver.revision_id)
                    || !waiver.is_authenticated_human()
                    || waiver.actor.is_empty()
                    || waiver.reason.is_empty()
                    || waiver.timestamp.is_empty()
                    || !waiver.scope.covers(&constraint.scope)
                {
                    return Err(DomainError::HardConstraintUnsatisfied);
                }
                self.waivers.push(waiver.clone());
                self.waivers.sort();
            }
            ProjectEvent::RevisionPromotedToCandidate { revision_id } => {
                self.require_lifecycle(revision_id, RevisionLifecycle::Draft)?;
                self.validate_candidate(revision_id)?;
                self.lifecycle
                    .insert(revision_id.clone(), RevisionLifecycle::Candidate);
            }
            ProjectEvent::RevisionAccepted {
                revision_id,
                decision,
            } => {
                Self::require_decision(decision)?;
                self.require_lifecycle(revision_id, RevisionLifecycle::Candidate)?;
                self.validate_hard_constraints(revision_id)?;
                self.lifecycle
                    .insert(revision_id.clone(), RevisionLifecycle::Accepted);
                self.accepted_revision = Some(revision_id.clone());
            }
            ProjectEvent::RevisionRejected {
                revision_id,
                decision,
            } => {
                Self::require_decision(decision)?;
                let current = self
                    .lifecycle
                    .get(revision_id)
                    .ok_or(DomainError::UnknownParent)?;
                if !matches!(
                    current,
                    RevisionLifecycle::Draft | RevisionLifecycle::Candidate
                ) {
                    return Err(DomainError::InvalidLifecycleTransition);
                }
                self.lifecycle
                    .insert(revision_id.clone(), RevisionLifecycle::Rejected);
            }
            ProjectEvent::RevisionAborted {
                revision_id,
                decision,
            } => {
                Self::require_decision(decision)?;
                let current = self
                    .lifecycle
                    .get(revision_id)
                    .ok_or(DomainError::UnknownParent)?;
                if !matches!(
                    current,
                    RevisionLifecycle::Draft | RevisionLifecycle::Candidate
                ) {
                    return Err(DomainError::InvalidLifecycleTransition);
                }
                self.lifecycle
                    .insert(revision_id.clone(), RevisionLifecycle::Aborted);
            }
            ProjectEvent::BranchHeadAdvanced {
                branch_id,
                expected,
                new_head,
            } => {
                let branch = self
                    .branches
                    .get_mut(branch_id)
                    .ok_or(DomainError::UnknownBranch)?;
                if &branch.head != expected {
                    return Err(DomainError::CommitConflict {
                        expected: expected.clone(),
                        actual: branch.head.clone(),
                    });
                }
                let revision = self
                    .revisions
                    .get(new_head)
                    .ok_or(DomainError::UnknownParent)?;
                if &revision.branch_id != branch_id {
                    return Err(DomainError::InvalidForkParent);
                }
                branch.head = Some(new_head.clone());
            }
        }
        Ok(())
    }

    fn insert_artifact(&mut self, record: &ArtifactRecord) -> Result<(), DomainError> {
        if let Some(existing) = self.artifacts.get(record.hash()) {
            let fixture_upgrade = existing.availability() == ArtifactAvailability::FixtureOnly
                && record.availability() == ArtifactAvailability::VerifiedDurable
                && existing.size() == record.size();
            if existing != record && !fixture_upgrade {
                return Err(DomainError::ArtifactHashMismatch);
            }
        }
        self.artifacts.insert(record.hash().clone(), record.clone());
        Ok(())
    }

    fn validate_fork(&self, base: Option<&RevisionId>) -> Result<(), DomainError> {
        if base.is_some_and(|id| !self.revisions.contains_key(id)) {
            return Err(DomainError::InvalidForkParent);
        }
        Ok(())
    }

    fn create_revision(&mut self, revision: &ScoreRevision) -> Result<(), DomainError> {
        self.same_project_score(&revision.project_id, &revision.score_id)?;
        let branch = self
            .branches
            .get(&revision.branch_id)
            .ok_or(DomainError::UnknownBranch)?;
        if branch.take_id != revision.take_id {
            return Err(DomainError::InvalidForkParent);
        }
        if revision.parents.len() > 1 {
            return Err(DomainError::UnsupportedMerge);
        }
        if revision.parents.iter().collect::<BTreeSet<_>>().len() != revision.parents.len() {
            return Err(DomainError::DuplicateParent);
        }
        if revision.parents.contains(&revision.id) {
            return Err(DomainError::RevisionCycle);
        }
        if !self.briefs.contains_key(&revision.brief_revision_id) {
            return Err(DomainError::ProjectionCorrupt);
        }
        for parent in &revision.parents {
            let parent = self
                .revisions
                .get(parent)
                .ok_or(DomainError::UnknownParent)?;
            self.same_project_score(&parent.project_id, &parent.score_id)?;
        }
        let required_parent = branch.head.as_ref().or(branch.fork_base.as_ref());
        if revision.parents.first() != required_parent
            || required_parent.is_some() == revision.parents.is_empty()
        {
            return Err(DomainError::InvalidForkParent);
        }
        if self
            .revisions
            .insert(revision.id.clone(), revision.clone())
            .is_some()
        {
            return Err(DomainError::ProjectionCorrupt);
        }
        self.lifecycle
            .insert(revision.id.clone(), RevisionLifecycle::Draft);
        Ok(())
    }

    fn require_lifecycle(
        &self,
        id: &RevisionId,
        expected: RevisionLifecycle,
    ) -> Result<(), DomainError> {
        if self.lifecycle.get(id) != Some(&expected) {
            return Err(DomainError::InvalidLifecycleTransition);
        }
        Ok(())
    }

    fn require_decision(decision: &HumanDecision) -> Result<(), DomainError> {
        if !decision.is_authenticated_human()
            || decision.actor.is_empty()
            || decision.timestamp.is_empty()
            || decision.note.is_empty()
        {
            return Err(DomainError::InvalidDomainValue);
        }
        Ok(())
    }

    fn validate_candidate(&self, id: &RevisionId) -> Result<(), DomainError> {
        let revision = self.revisions.get(id).ok_or(DomainError::UnknownParent)?;
        if self
            .artifacts
            .get(&revision.source_artifact)
            .is_none_or(|record| record.availability() != ArtifactAvailability::VerifiedDurable)
        {
            return Err(DomainError::HardConstraintUnsatisfied);
        }
        let h0_pass = self.evidence.values().any(|evidence| {
            &evidence.revision_id == id
                && evidence.outcome == ConstraintOutcome::Pass
                && matches!(
                    evidence.subject,
                    EvidenceSubject::H0(MusicalScope::WholeScore)
                )
        });
        if !h0_pass {
            return Err(DomainError::HardConstraintUnsatisfied);
        }
        Ok(())
    }

    fn validate_hard_constraints(&self, revision_id: &RevisionId) -> Result<(), DomainError> {
        for constraint in self
            .constraints
            .values()
            .filter(|item| item.strength == ConstraintStrength::Hard)
        {
            let passed = self.evidence.values().any(|evidence| {
                &evidence.revision_id == revision_id
                    && evidence.outcome == ConstraintOutcome::Pass
                    && matches!(
                        &evidence.subject,
                        EvidenceSubject::Constraint(id, scope)
                            if id == &constraint.id && scope.covers(&constraint.scope)
                    )
            });
            let waived = self.waivers.iter().any(|waiver| {
                &waiver.revision_id == revision_id
                    && waiver.constraint_id == constraint.id
                    && waiver.scope.covers(&constraint.scope)
            });
            if !passed && !waived {
                return Err(DomainError::HardConstraintUnsatisfied);
            }
        }
        Ok(())
    }
}

pub(crate) fn replay(events: &[SequencedProjectEvent]) -> Result<ProjectSnapshot, DomainError> {
    let mut snapshot = ProjectSnapshot::default();
    for event in events {
        snapshot.apply(event)?;
    }
    Ok(snapshot)
}

#[derive(Clone, Debug)]
pub struct ProposeRevision {
    pub command_id: CommandId,
    pub payload_digest: String,
    pub project_id: DomainProjectId,
    pub take_id: TakeId,
    pub branch_id: BranchId,
    pub expected_head_revision_id: Option<RevisionId>,
    pub revision: ScoreRevision,
    pub evidence: Vec<EvidenceEnvelope>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposalResult {
    pub revision_id: RevisionId,
    pub new_head: RevisionId,
    pub last_sequence: u64,
}

#[derive(Clone, Debug)]
pub struct ProjectCoordinator {
    snapshot: ProjectSnapshot,
    events: Vec<SequencedProjectEvent>,
    commands: BTreeMap<CommandId, (String, ProposalResult)>,
}

impl ProjectCoordinator {
    pub(crate) fn initialize(
        project_id: DomainProjectId,
        score_id: ScoreId,
        take_id: TakeId,
        branch_id: BranchId,
    ) -> Result<Self, DomainError> {
        Self::from_events(vec![SequencedProjectEvent {
            schema_version: SchemaVersion::project_event_v1(),
            sequence: 1,
            event: ProjectEvent::ProjectInitialized {
                project_id,
                score_id,
                default_take_id: take_id,
                default_branch_id: branch_id,
            },
        }])
    }

    pub(crate) fn from_events(events: Vec<SequencedProjectEvent>) -> Result<Self, DomainError> {
        Ok(Self {
            snapshot: replay(&events)?,
            events,
            commands: BTreeMap::new(),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> &ProjectSnapshot {
        &self.snapshot
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn events(&self) -> &[SequencedProjectEvent] {
        &self.events
    }

    #[cfg(test)]
    pub(crate) fn apply_events(&mut self, events: Vec<ProjectEvent>) -> Result<(), DomainError> {
        self.commit(events).map(|_| ())
    }

    pub fn register_artifact(
        &mut self,
        receipt: CommittedArtifactReceipt,
    ) -> Result<(), DomainError> {
        self.commit(vec![ProjectEvent::ArtifactRegistered(
            receipt.into_record()?,
        )])
        .map(|_| ())
    }

    pub fn propose(&mut self, proposal: ProposeRevision) -> Result<ProposalResult, DomainError> {
        validate_digest(&proposal.payload_digest)?;
        if let Some((digest, reply)) = self.commands.get(&proposal.command_id) {
            return if digest == &proposal.payload_digest {
                Ok(reply.clone())
            } else {
                Err(DomainError::IdempotencyConflict)
            };
        }
        if self.snapshot.project_id.as_ref() != Some(&proposal.project_id) {
            return Err(DomainError::CrossProjectReference);
        }
        let branch = self
            .snapshot
            .branches
            .get(&proposal.branch_id)
            .ok_or(DomainError::UnknownBranch)?;
        if branch.take_id != proposal.take_id {
            return Err(DomainError::UnknownTake);
        }
        if branch.head != proposal.expected_head_revision_id {
            return Err(DomainError::CommitConflict {
                expected: proposal.expected_head_revision_id,
                actual: branch.head.clone(),
            });
        }
        let revision_id = proposal.revision.id.clone();
        let mut candidate = vec![ProjectEvent::RevisionCreated(proposal.revision)];
        candidate.extend(
            proposal
                .evidence
                .into_iter()
                .map(ProjectEvent::EvidenceRecorded),
        );
        candidate.push(ProjectEvent::BranchHeadAdvanced {
            branch_id: proposal.branch_id,
            expected: proposal.expected_head_revision_id,
            new_head: revision_id.clone(),
        });
        let last_sequence = self.commit(candidate)?;
        let result = ProposalResult {
            revision_id: revision_id.clone(),
            new_head: revision_id,
            last_sequence,
        };
        self.commands.insert(
            proposal.command_id,
            (proposal.payload_digest, result.clone()),
        );
        Ok(result)
    }

    fn commit(&mut self, events: Vec<ProjectEvent>) -> Result<u64, DomainError> {
        let mut projected = self.snapshot.clone();
        let mut envelopes = Vec::with_capacity(events.len());
        for event in events {
            let envelope = SequencedProjectEvent {
                schema_version: SchemaVersion::project_event_v1(),
                sequence: projected.last_sequence + 1,
                event,
            };
            projected.apply(&envelope)?;
            envelopes.push(envelope);
        }
        self.snapshot = projected;
        self.events.extend(envelopes);
        Ok(self.snapshot.last_sequence)
    }
}

fn validate_digest(value: &str) -> Result<(), DomainError> {
    ArtifactHash::parse(value.to_owned()).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id<T>(value: &str, parse: impl FnOnce(String) -> Result<T, DomainError>) -> T {
        parse(value.to_owned()).expect("valid fixture ID")
    }

    fn hash(byte: char) -> ArtifactHash {
        ArtifactHash::parse(format!("sha256:{}", byte.to_string().repeat(64))).expect("hash")
    }

    fn initial_events() -> Vec<ProjectEvent> {
        vec![
            ProjectEvent::ProjectInitialized {
                project_id: id("project-1", DomainProjectId::parse),
                score_id: id("score-1", ScoreId::parse),
                default_take_id: id("take-1", TakeId::parse),
                default_branch_id: id("branch-1", BranchId::parse),
            },
            ProjectEvent::BriefRevisionCreated(CreativeBrief {
                id: id("brief-1", BriefRevisionId::parse),
                project_id: id("project-1", DomainProjectId::parse),
                previous: None,
                user_description: "Write an etude".to_owned(),
                goals: vec!["clarity".to_owned()],
                instrumentation: vec!["piano".to_owned()],
                open_questions: vec![],
            }),
            ProjectEvent::ConstraintDeclared(Constraint {
                id: id("constraint-1", ConstraintId::parse),
                brief_revision_id: id("brief-1", BriefRevisionId::parse),
                strength: ConstraintStrength::Hard,
                description: "Pass the fixture gate".to_owned(),
                machine_rule: None,
                scope: MusicalScope::WholeScore,
            }),
        ]
    }

    fn coordinator() -> ProjectCoordinator {
        let mut coordinator = ProjectCoordinator::from_events(vec![]).expect("empty projection");
        coordinator
            .apply_events(initial_events())
            .expect("initialize project");
        coordinator
    }

    fn revision(id_value: &str, branch: &str, take: &str, parents: Vec<&str>) -> ScoreRevision {
        ScoreRevision {
            id: id(id_value, RevisionId::parse),
            project_id: id("project-1", DomainProjectId::parse),
            score_id: id("score-1", ScoreId::parse),
            take_id: id(take, TakeId::parse),
            branch_id: id(branch, BranchId::parse),
            parents: parents
                .into_iter()
                .map(|value| id(value, RevisionId::parse))
                .collect(),
            brief_revision_id: id("brief-1", BriefRevisionId::parse),
            source_artifact: hash('a'),
            ir_artifact: None,
            origin: RevisionOrigin::DeterministicFixture,
        }
    }

    fn proposal(command: &str, revision: ScoreRevision, expected: Option<&str>) -> ProposeRevision {
        ProposeRevision {
            command_id: id(command, CommandId::parse),
            payload_digest: hash(if command.ends_with('2') { '2' } else { '1' })
                .as_str()
                .to_owned(),
            project_id: id("project-1", DomainProjectId::parse),
            take_id: revision.take_id.clone(),
            branch_id: revision.branch_id.clone(),
            expected_head_revision_id: expected.map(|value| id(value, RevisionId::parse)),
            revision,
            evidence: vec![],
        }
    }

    fn evidence(id_value: &str, subject: EvidenceSubject) -> ProjectEvent {
        ProjectEvent::EvidenceRecorded(EvidenceEnvelope {
            id: id(id_value, EvidenceId::parse),
            revision_id: id("revision-1", RevisionId::parse),
            subject_hash: hash('a'),
            subject,
            outcome: ConstraintOutcome::Pass,
            producer: EvidenceProducer::DeterministicFixture,
            method: format!("fixture-{id_value}"),
            artifact_refs: vec![],
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        })
    }

    fn accept_event(note: &str, command: &str) -> ProjectEvent {
        ProjectEvent::RevisionAccepted {
            revision_id: id("revision-1", RevisionId::parse),
            decision: HumanDecision::new(
                HumanActor::from_authenticated_client("human-1").expect("human"),
                "2026-01-01T00:01:00Z",
                note,
                id(command, CommandId::parse),
            )
            .expect("decision"),
        }
    }

    #[test]
    fn replay_default_extra_take_branch_and_cross_take_fork() {
        let mut coordinator = coordinator();
        coordinator
            .propose(proposal(
                "command-1",
                revision("revision-1", "branch-1", "take-1", vec![]),
                None,
            ))
            .expect("root revision");
        coordinator
            .apply_events(vec![ProjectEvent::TakeCreated {
                project_id: id("project-1", DomainProjectId::parse),
                score_id: id("score-1", ScoreId::parse),
                take_id: id("take-2", TakeId::parse),
                common_base: Some(id("revision-1", RevisionId::parse)),
                default_branch_id: id("branch-2", BranchId::parse),
            }])
            .expect("fork take");
        coordinator
            .propose(proposal(
                "command-2",
                revision("revision-2", "branch-2", "take-2", vec!["revision-1"]),
                None,
            ))
            .expect("first fork revision");
        coordinator
            .apply_events(vec![ProjectEvent::BranchCreated {
                project_id: id("project-1", DomainProjectId::parse),
                score_id: id("score-1", ScoreId::parse),
                take_id: id("take-2", TakeId::parse),
                branch_id: id("branch-3", BranchId::parse),
                fork_base: Some(id("revision-2", RevisionId::parse)),
            }])
            .expect("extra branch");

        let replayed = replay(coordinator.events()).expect("replay all facts");
        assert_eq!(replayed, *coordinator.snapshot());
        assert_eq!(
            replayed.branches[&id("branch-2", BranchId::parse)].head,
            Some(id("revision-2", RevisionId::parse))
        );
    }

    #[test]
    fn coordinator_is_atomic_idempotent_and_branch_head_cas_is_exclusive() {
        let mut coordinator = coordinator();
        let first = proposal(
            "command-1",
            revision("revision-1", "branch-1", "take-1", vec![]),
            None,
        );
        let result = coordinator.propose(first.clone()).expect("first commit");
        let event_count = coordinator.events().len();
        assert_eq!(coordinator.propose(first).expect("idempotent"), result);
        assert_eq!(coordinator.events().len(), event_count);

        let mut conflicting_digest = proposal(
            "command-1",
            revision("revision-x", "branch-1", "take-1", vec![]),
            None,
        );
        conflicting_digest.payload_digest = hash('f').as_str().to_owned();
        assert_eq!(
            coordinator.propose(conflicting_digest),
            Err(DomainError::IdempotencyConflict)
        );
        let stale = proposal(
            "command-2",
            revision("revision-2", "branch-1", "take-1", vec![]),
            None,
        );
        assert!(matches!(
            coordinator.propose(stale),
            Err(DomainError::CommitConflict { .. })
        ));
        assert_eq!(coordinator.events().len(), event_count);

        let invalid = proposal(
            "command-2",
            revision("revision-2", "branch-1", "take-1", vec!["missing-parent"]),
            Some("revision-1"),
        );
        assert_eq!(
            coordinator.propose(invalid),
            Err(DomainError::UnknownParent)
        );
        assert_eq!(coordinator.events().len(), event_count);
    }

    #[test]
    fn durable_receipt_h0_and_hard_evidence_gate_lifecycle_and_replay() {
        let mut coordinator = coordinator();
        coordinator
            .propose(proposal(
                "command-1",
                revision("revision-1", "branch-1", "take-1", vec![]),
                None,
            ))
            .expect("revision");
        coordinator
            .apply_events(vec![ProjectEvent::FixtureArtifactDeclared(
                ArtifactRecord::fixture(hash('a'), 10),
            )])
            .expect("fixture metadata");
        assert_eq!(
            coordinator.apply_events(vec![ProjectEvent::RevisionPromotedToCandidate {
                revision_id: id("revision-1", RevisionId::parse),
            }]),
            Err(DomainError::HardConstraintUnsatisfied)
        );
        let receipt = CommittedArtifactReceipt::from_test_store(hash('a'), 10);
        coordinator
            .register_artifact(receipt)
            .expect("receipt registration");
        coordinator
            .apply_events(vec![
                evidence("evidence-h0", EvidenceSubject::H0(MusicalScope::WholeScore)),
                evidence(
                    "evidence-narrow",
                    EvidenceSubject::Constraint(
                        id("constraint-1", ConstraintId::parse),
                        MusicalScope::StablePart(
                            StablePartId::parse("violin").expect("stable part"),
                        ),
                    ),
                ),
                ProjectEvent::RevisionPromotedToCandidate {
                    revision_id: id("revision-1", RevisionId::parse),
                },
            ])
            .expect("evidence and candidate");
        let before_failed_accept = coordinator.events().len();
        assert_eq!(
            coordinator.apply_events(vec![accept_event("scope is too narrow", "accept-narrow",)]),
            Err(DomainError::HardConstraintUnsatisfied)
        );
        assert_eq!(coordinator.events().len(), before_failed_accept);
        coordinator
            .apply_events(vec![
                evidence(
                    "evidence-hard",
                    EvidenceSubject::Constraint(
                        id("constraint-1", ConstraintId::parse),
                        MusicalScope::WholeScore,
                    ),
                ),
                accept_event("accept fixture", "accept-1"),
            ])
            .expect("covering evidence and accept");
        assert_eq!(
            coordinator.snapshot().lifecycle[&id("revision-1", RevisionId::parse)],
            RevisionLifecycle::Accepted
        );
        let replayed = replay(coordinator.events()).expect("full replay");
        assert_eq!(replayed, *coordinator.snapshot());
        assert_eq!(
            replayed.canonical_digest().expect("digest"),
            coordinator.snapshot().canonical_digest().expect("digest")
        );

        let without_registration = coordinator
            .events()
            .iter()
            .filter(|event| !matches!(event.event, ProjectEvent::ArtifactRegistered(_)))
            .cloned()
            .enumerate()
            .map(|(index, mut event)| {
                event.sequence = index as u64 + 1;
                event
            })
            .collect::<Vec<_>>();
        assert_eq!(
            replay(&without_registration),
            Err(DomainError::HardConstraintUnsatisfied)
        );
    }

    #[test]
    fn dag_rejections_and_lifecycle_fail_closed_without_pollution() {
        let mut coordinator = coordinator();
        let before = coordinator.snapshot().clone();
        let merge = revision(
            "revision-1",
            "branch-1",
            "take-1",
            vec!["parent-a", "parent-b"],
        );
        assert_eq!(
            coordinator.propose(proposal("command-1", merge, None)),
            Err(DomainError::UnsupportedMerge)
        );
        assert_eq!(coordinator.snapshot(), &before);

        let self_parent = revision("revision-self", "branch-1", "take-1", vec!["revision-self"]);
        assert_eq!(
            coordinator.propose(proposal("command-1", self_parent, None)),
            Err(DomainError::RevisionCycle)
        );
        assert_eq!(coordinator.snapshot(), &before);
    }

    #[test]
    fn draft_can_be_rejected_and_terminal_lifecycle_is_fail_closed() {
        let mut coordinator = coordinator();
        coordinator
            .propose(proposal(
                "command-1",
                revision("revision-1", "branch-1", "take-1", vec![]),
                None,
            ))
            .expect("draft");
        coordinator
            .apply_events(vec![ProjectEvent::RevisionRejected {
                revision_id: id("revision-1", RevisionId::parse),
                decision: HumanDecision::new(
                    HumanActor::from_authenticated_client("human-1").expect("human"),
                    "2026-01-01T00:01:00Z",
                    "reject draft",
                    id("reject-1", CommandId::parse),
                )
                .expect("decision"),
            }])
            .expect("Draft -> Rejected");
        let after_reject = coordinator.snapshot().clone();
        assert_eq!(
            coordinator.apply_events(vec![ProjectEvent::RevisionAborted {
                revision_id: id("revision-1", RevisionId::parse),
                decision: HumanDecision::new(
                    HumanActor::from_authenticated_client("human-1").expect("human"),
                    "2026-01-01T00:02:00Z",
                    "cannot reverse terminal state",
                    id("abort-1", CommandId::parse),
                )
                .expect("decision"),
            }]),
            Err(DomainError::InvalidLifecycleTransition)
        );
        assert_eq!(coordinator.snapshot(), &after_reject);
    }

    #[test]
    fn canonical_projection_digest_has_a_fixed_vector_and_changes_with_ordered_facts() {
        let coordinator = coordinator();
        let digest = coordinator.snapshot().canonical_digest().expect("digest");
        assert_eq!(
            digest,
            "sha256:1b10218f9ff633997b753498671483689e7d588f416a55e6d35125ec9a7e33e7"
        );

        let mut changed = coordinator;
        changed
            .apply_events(vec![ProjectEvent::FixtureArtifactDeclared(
                ArtifactRecord::fixture(hash('b'), 1),
            )])
            .expect("fact");
        assert_ne!(
            digest,
            changed
                .snapshot()
                .canonical_digest()
                .expect("changed digest")
        );
    }
}
