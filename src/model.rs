use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

macro_rules! ids {
    ($($name:ident),* $(,)?) => {$ (
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name { pub fn new() -> Self { Self(Uuid::new_v4()) } }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) }
        }
    )*};
}
ids!(
    RepositoryId,
    GoalId,
    ThunkId,
    EventId,
    ActorId,
    DraftId,
    FeedbackId,
    BatchId,
    CheckpointId,
    CommandId,
    ValidationId
);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct GitOid(pub String);
impl GitOid {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if !matches!(value.len(), 40 | 64)
            || !value
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err("expected lowercase SHA-1 or SHA-256 object ID".into());
        }
        Ok(Self(value))
    }
}
impl<'de> Deserialize<'de> for GitOid {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RepoPath(pub String);
impl RepoPath {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty()
            || value.starts_with('/')
            || value.contains(['\0', '\\'])
            || value
                .split('/')
                .any(|s| s.is_empty() || matches!(s, "." | ".." | ".git"))
        {
            return Err(
                "expected normalized repository-relative path without .git or traversal".into(),
            );
        }
        Ok(Self(value))
    }
}
impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
pub type Epoch = u64;
pub type Sequence = u64;
pub type Timestamp = String;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalVersion {
    pub head: GitOid,
    pub sequence: Sequence,
    pub epoch: Epoch,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Reviewer,
    Agent,
    System,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub id: ActorId,
    pub kind: ActorKind,
    pub display_name: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub tree: GitOid,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextRange {
    pub start: u64,
    pub end: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub id: GoalId,
    pub repository: RepositoryId,
    pub title: String,
    pub objective: String,
    pub base: SourceSnapshot,
    pub created_by: ActorId,
    pub created_at: Timestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confidence {
    pub basis_points: u16,
    pub rationale: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThunkSubmission {
    pub id: ThunkId,
    pub based_on_epoch: Epoch,
    pub title: String,
    pub description: String,
    pub supersedes: Option<ThunkId>,
    pub depends_on: Vec<ThunkId>,
    pub related_to: Vec<ThunkId>,
    pub provenance: Vec<ThunkId>,
    pub before: SourceSnapshot,
    pub after: SourceSnapshot,
    pub confidence: Confidence,
    pub validation: Vec<ValidationEvidence>,
    pub responds_to: Vec<FeedbackId>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thunk {
    #[serde(flatten)]
    pub submission: ThunkSubmission,
    pub goal_id: GoalId,
    pub author: ActorId,
    pub created_at: Timestamp,
    pub iteration: u32,
}
impl std::ops::Deref for Thunk {
    type Target = ThunkSubmission;
    fn deref(&self) -> &Self::Target {
        &self.submission
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationOutcome {
    Passed,
    Failed,
    NotRun,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationEvidence {
    pub id: ValidationId,
    pub source: SourceSnapshot,
    pub reported_by: ActorId,
    pub command_display: String,
    pub outcome: ValidationOutcome,
    pub output_blob: Option<GitOid>,
    pub recorded_at: Timestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    Before,
    After,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeAnchor {
    pub snapshot: SourceSnapshot,
    pub path: RepoPath,
    pub blob: GitOid,
    pub side: DiffSide,
    pub range: TextRange,
    pub context_before: String,
    pub selected_text: String,
    pub context_after: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FeedbackTarget {
    Goal {
        goal_id: GoalId,
    },
    Thunk {
        thunk_id: ThunkId,
    },
    Code {
        thunk_id: ThunkId,
        anchor: CodeAnchor,
    },
}
impl FeedbackTarget {
    pub fn thunk_id(&self) -> Option<&ThunkId> {
        match self {
            Self::Goal { .. } => None,
            Self::Thunk { thunk_id } | Self::Code { thunk_id, .. } => Some(thunk_id),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind {
    Question,
    ChangeRequest,
    Note,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FeedbackOrigin {
    Sidebar,
    Inline {
        draft_id: DraftId,
        source_range: TextRange,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    pub id: FeedbackId,
    pub author: ActorId,
    pub target: FeedbackTarget,
    pub body: String,
    pub kind: FeedbackKind,
    pub origin: FeedbackOrigin,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackInput {
    pub id: FeedbackId,
    pub target: FeedbackTarget,
    pub body: String,
    pub kind: FeedbackKind,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftExtraction {
    pub draft: DraftEditionRef,
    pub base: SourceSnapshot,
    pub annotated: SourceSnapshot,
    pub cleaned: SourceSnapshot,
    pub source_map_blob: GitOid,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerEdit {
    pub thunk_id: ThunkId,
    pub base: SourceSnapshot,
    pub edited: SourceSnapshot,
    pub annotated: SourceSnapshot,
    pub source_map_blob: GitOid,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImpactScope {
    Selected { thunks: Vec<ThunkId> },
    GoalWide,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackBatch {
    pub extractions: Vec<DraftExtraction>,
    pub id: BatchId,
    pub goal_id: GoalId,
    pub author: ActorId,
    pub reviewed_at: GoalVersion,
    pub epoch_before: Epoch,
    pub epoch_after: Epoch,
    pub message: String,
    pub feedback: Vec<Feedback>,
    pub edits: Vec<ReviewerEdit>,
    pub scope: ImpactScope,
    pub invalidation_roots: Vec<ThunkId>,
    pub stale_thunks: Vec<ThunkId>,
    pub consumed_drafts: Vec<DraftEditionRef>,
    pub published_at: Timestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftEditionRef {
    pub draft_id: DraftId,
    pub edition: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerIntent {
    pub path: RepoPath,
    pub range: TextRange,
    pub enabled: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDraftInput {
    pub id: DraftId,
    pub thunk_id: Option<ThunkId>,
    pub base: Option<SourceSnapshot>,
    pub annotated: Option<SourceSnapshot>,
    pub sidebar_feedback: Vec<FeedbackInput>,
    pub marker_intents: Vec<MarkerIntent>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDraft {
    pub id: DraftId,
    pub goal_id: GoalId,
    pub reviewer: ActorId,
    pub edition: u64,
    pub opened_at: GoalVersion,
    pub thunk_id: Option<ThunkId>,
    pub base: Option<SourceSnapshot>,
    pub annotated: Option<SourceSnapshot>,
    pub sidebar_feedback: Vec<Feedback>,
    pub marker_intents: Vec<MarkerIntent>,
    pub updated_at: Timestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested { reason: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StaleCause {
    Feedback { batch_id: BatchId },
    ReplacedDependency { old: ThunkId, replacement: ThunkId },
    WithdrawnDependency { thunk_id: ThunkId },
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThunkReviewState {
    pub decision: Option<ReviewDecision>,
    pub stale_causes: Vec<StaleCause>,
    pub withdrawn: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub name: String,
    pub state_at: GoalVersion,
    pub source: SourceSnapshot,
    pub created_by: ActorId,
    pub created_at: Timestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoredState {
    pub active_feedback: BTreeSet<FeedbackId>,
    pub current_thunks: BTreeSet<ThunkId>,
    pub reviews: BTreeMap<ThunkId, ThunkReviewState>,
    pub resolved_feedback: BTreeSet<FeedbackId>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRecord {
    pub checkpoint_id: CheckpointId,
    pub restored_from: GitOid,
    pub previous_head: GitOid,
    pub new_epoch: Epoch,
    pub reason: String,
    pub state: RestoredState,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: EventId,
    pub payload: Event,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    GoalCreated {
        goal: Goal,
    },
    ThunkSubmitted {
        thunk: Thunk,
    },
    ThunkWithdrawn {
        thunk_id: ThunkId,
        reason: String,
    },
    DraftSaved {
        draft: ReviewDraft,
    },
    DraftDiscarded {
        draft_id: DraftId,
    },
    FeedbackPublished {
        batch: FeedbackBatch,
    },
    DecisionRecorded {
        thunk_id: ThunkId,
        decision: ReviewDecision,
    },
    FeedbackResolved {
        feedback_id: FeedbackId,
        resolution: String,
    },
    BatchAcknowledged {
        batch_id: BatchId,
        agent: ActorId,
    },
    ControlAcknowledged {
        event_id: EventId,
        agent: ActorId,
    },
    CheckpointCreated {
        checkpoint: Checkpoint,
    },
    GoalRestored {
        restore: RestoreRecord,
    },
    GoalClosed {
        reason: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    pub schema_version: u32,
    pub command_id: CommandId,
    pub request_digest: String,
    pub goal_id: GoalId,
    pub sequence: Sequence,
    pub actor: ActorId,
    pub at: Timestamp,
    pub events: Vec<EventRecord>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalProjection {
    pub active_feedback: BTreeSet<FeedbackId>,
    pub draft_editions: BTreeMap<DraftId, u64>,
    pub goal: Goal,
    pub sequence: Sequence,
    pub epoch: Epoch,
    pub closed: bool,
    pub thunks: BTreeMap<ThunkId, Thunk>,
    pub current_thunks: BTreeSet<ThunkId>,
    pub reviews: BTreeMap<ThunkId, ThunkReviewState>,
    pub drafts: BTreeMap<DraftId, ReviewDraft>,
    pub published_batches: BTreeMap<BatchId, FeedbackBatch>,
    pub resolved_feedback: BTreeSet<FeedbackId>,
    pub acknowledgments: BTreeSet<(BatchId, ActorId)>,
    pub control_acknowledgments: BTreeSet<(EventId, ActorId)>,
    pub checkpoints: BTreeMap<CheckpointId, Checkpoint>,
    pub reverse_dependencies: BTreeMap<ThunkId, BTreeSet<ThunkId>>,
    pub submission_sequences: BTreeMap<ThunkId, Sequence>,
    pub deliveries: Vec<Delivery>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub event_id: EventId,
    pub sequence: Sequence,
    pub epoch: Epoch,
    pub payload: DeliveryPayload,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeliveryPayload {
    Feedback { batch: FeedbackBatch },
    Restore { restore: RestoreRecord },
    Closed { reason: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEnvelope<T> {
    pub command_id: CommandId,
    pub expected_head: Option<GitOid>,
    pub expected_epoch: Option<Epoch>,
    pub payload: T,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub command_id: CommandId,
    pub version: GoalVersion,
    pub event_ids: Vec<EventId>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateGoal {
    pub id: GoalId,
    pub repository: RepositoryId,
    pub title: String,
    pub objective: String,
    pub base: SourceSnapshot,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GoalCommand {
    SubmitThunks {
        thunks: Vec<ThunkSubmission>,
    },
    WithdrawThunk {
        thunk_id: ThunkId,
        reason: String,
    },
    SaveDraft {
        draft: ReviewDraftInput,
        expected_edition: Option<u64>,
    },
    DiscardDraft {
        draft_id: DraftId,
    },
    PublishFeedback {
        preview_digest: String,
        request: PreviewPublish,
    },
    RecordDecision {
        thunk_id: ThunkId,
        decision: ReviewDecision,
    },
    ResolveFeedback {
        feedback_id: FeedbackId,
        resolution: String,
    },
    AcknowledgeBatch {
        batch_id: BatchId,
    },
    AcknowledgeControl {
        event_id: EventId,
    },
    CreateCheckpoint {
        id: CheckpointId,
        name: String,
        source: SourceSnapshot,
    },
    RestoreCheckpoint {
        checkpoint_id: CheckpointId,
        reason: String,
    },
    CloseGoal {
        reason: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewPublish {
    pub drafts: Vec<DraftEditionRef>,
    pub message: String,
    pub scope: ImpactScope,
    pub invalidation_roots: Vec<ThunkId>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishPreview {
    pub extractions: Vec<DraftExtraction>,
    pub version: GoalVersion,
    pub digest: String,
    pub feedback: Vec<Feedback>,
    pub edits: Vec<ReviewerEdit>,
    pub affected_thunks: Vec<ThunkId>,
    pub diagnostics: Vec<PublishDiagnostic>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishDiagnostic {
    pub message: String,
    pub blocks_publication: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    NotFound,
    InvalidInput,
    VersionConflict,
    EpochConflict,
    DraftConflict,
    DependencyCycle,
    DependencyNotCurrent,
    DependencyNotApproved,
    CompositionConflict,
    UnsupportedFile,
    PreviewExpired,
    IdempotencyConflict,
    StorageFailure,
    Unauthorized,
    UpgradeRequired,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
    pub current_version: Option<GoalVersion>,
    pub fields: BTreeMap<String, String>,
    pub retryable: bool,
}
impl ApiError {
    pub fn new(code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            current_version: None,
            fields: BTreeMap::new(),
            retryable: false,
        }
    }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}
impl std::error::Error for ApiError {}
pub type ApiResult<T> = Result<T, ApiError>;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueStatus {
    Pending,
    ChangesRequested,
    Approved,
    Stale,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub thunk: Thunk,
    pub status: QueueStatus,
    pub blocked_by: Vec<ThunkId>,
    pub unresolved_feedback: u32,
    pub unpublished_feedback: u32,
    pub submission_sequence: Sequence,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalDetail {
    pub actionable_feedback: BTreeSet<FeedbackId>,
    pub goal: Goal,
    pub version: GoalVersion,
    pub counts: QueueCounts,
    pub active_source: Option<SourceSnapshot>,
    pub closed: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueCounts {
    pub pending: usize,
    pub approved: usize,
    pub stale: usize,
    pub drafts: usize,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versioned<T> {
    pub version: GoalVersion,
    pub data: T,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub version: GoalVersion,
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThunkDetail {
    pub version: GoalVersion,
    pub item: QueueItem,
    pub predecessors: Vec<ThunkId>,
    pub dependents: Vec<ThunkId>,
    pub feedback: Vec<Feedback>,
    pub files: Vec<FileDiff>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    ModeChanged,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub old_path: Option<RepoPath>,
    pub new_path: Option<RepoPath>,
    pub old_blob: Option<GitOid>,
    pub new_blob: Option<GitOid>,
    pub kind: FileChangeKind,
    pub binary: bool,
    pub hunks: Vec<DiffHunk>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_range: Option<TextRange>,
    pub new_range: Option<TextRange>,
    pub text: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantRole {
    Supervisor,
    Worker,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionDescriptor {
    pub api_version: String,
    pub endpoint: String,
    pub credential_handle: String,
    pub repository_id: RepositoryId,
    pub goal_id: GoalId,
    pub actor_id: ActorId,
    pub worktree: String,
    pub version: GoalVersion,
    pub role: ParticipantRole,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAssignment {
    pub task_key: String,
    pub goal_id: GoalId,
    pub version: GoalVersion,
    pub baseline: SourceSnapshot,
    pub scope: String,
    pub allowed_paths: Vec<RepoPath>,
    pub prerequisites: Vec<ThunkId>,
    pub feedback_ids: Vec<FeedbackId>,
    pub expected_checks: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProposalDependency {
    Existing { thunk_id: ThunkId },
    Proposed { task_key: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerProposal {
    pub task_key: String,
    pub based_on_epoch: Epoch,
    pub before: SourceSnapshot,
    pub patch_artifact: String,
    pub title: String,
    pub rationale: String,
    pub depends_on: Vec<ProposalDependency>,
    pub confidence: Confidence,
    pub validation: Vec<ValidationEvidence>,
    pub limitations: Vec<String>,
    pub responds_to: Vec<FeedbackId>,
}
