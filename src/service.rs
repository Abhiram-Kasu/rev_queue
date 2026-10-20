use crate::{
    core,
    model::*,
    persistence::{Conflict, GitRepository, TransactionObjects},
    review, source,
};
use fs2::FileExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}
struct Inner {
    repo: GitRepository,
    _lock: File,
    gate: Mutex<()>,
    notifications: broadcast::Sender<Notification>,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct Notification {
    pub goal_id: GoalId,
    pub command_id: CommandId,
    pub version: GoalVersion,
}
type RetainedRecords = (BTreeMap<String, Vec<u8>>, Vec<String>, Vec<String>);
fn storage(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(ApiErrorCode::StorageFailure, error.to_string())
}
fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::InvalidInput, message)
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn digest(value: &impl Serialize) -> ApiResult<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(storage)?)
    ))
}
fn oid(value: String) -> ApiResult<GitOid> {
    GitOid::parse(value).map_err(storage)
}
fn reviewer(actor: &Actor) -> ApiResult<()> {
    if actor.kind != ActorKind::Reviewer {
        return Err(ApiError::new(
            ApiErrorCode::Unauthorized,
            "reviewer capability required",
        ));
    }
    Ok(())
}
fn agent(actor: &Actor) -> ApiResult<()> {
    if actor.kind != ActorKind::Agent {
        return Err(ApiError::new(
            ApiErrorCode::Unauthorized,
            "agent capability required",
        ));
    }
    Ok(())
}

impl Service {
    pub fn open(path: impl AsRef<Path>) -> ApiResult<Self> {
        let repo = GitRepository::open(path).map_err(storage)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(repo.common_dir().join("revqueue-writer.lock"))
            .map_err(storage)?;
        lock.try_lock_exclusive()
            .map_err(|_| storage("another Rev Queue writer holds this repository"))?;
        let (notifications, _) = broadcast::channel(128);
        let service = Self {
            inner: Arc::new(Inner {
                repo,
                _lock: lock,
                gate: Mutex::new(()),
                notifications,
            }),
        };
        for (_, head) in service.inner.repo.list_goals().map_err(storage)? {
            service.validate_head(&head)?;
        }
        Ok(service)
    }
    pub fn repository(&self) -> &GitRepository {
        &self.inner.repo
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.inner.notifications.subscribe()
    }
    fn validate_head(&self, head: &str) -> ApiResult<GoalProjection> {
        let tx: Transaction = self
            .inner
            .repo
            .read_json(head, "transaction.json")
            .map_err(storage)?;
        if tx.schema_version != 1 {
            return Err(ApiError::new(
                ApiErrorCode::UpgradeRequired,
                "unsupported transaction schema",
            ));
        }
        let state: GoalProjection = self
            .inner
            .repo
            .read_json(head, "state.json")
            .map_err(storage)?;
        if state.sequence != tx.sequence || state.goal.id != tx.goal_id {
            return Err(storage("projection does not match transaction"));
        }
        Ok(state)
    }
    pub fn load(&self, goal: &GoalId) -> ApiResult<(GoalVersion, GoalProjection)> {
        let head = self
            .inner
            .repo
            .read_ref(&goal.to_string())
            .map_err(storage)?
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "goal not found"))?;
        let state = self.validate_head(&head)?;
        Ok((
            GoalVersion {
                head: oid(head)?,
                sequence: state.sequence,
                epoch: state.epoch,
            },
            state,
        ))
    }
    pub fn at(&self, goal: &GoalId, head: &GitOid) -> ApiResult<GoalProjection> {
        let (current, _) = self.load(goal)?;
        if !self
            .inner
            .repo
            .history(&current.head.0)
            .map_err(storage)?
            .contains(&head.0)
        {
            return Err(ApiError::new(
                ApiErrorCode::NotFound,
                "head is not in goal history",
            ));
        }
        self.validate_head(&head.0)
    }
    pub fn repair(&self, goal: &GoalId) -> ApiResult<GoalProjection> {
        let (version, state) = self.load(goal)?;
        let mut replay = None;
        for head in self.inner.repo.history(&version.head.0).map_err(storage)? {
            let tx: Transaction = self
                .inner
                .repo
                .read_json(&head, "transaction.json")
                .map_err(storage)?;
            replay = Some(core::reduce(replay.as_ref(), &tx)?);
            let expected: GoalProjection = self
                .inner
                .repo
                .read_json(&head, "state.json")
                .map_err(storage)?;
            if replay.as_ref() != Some(&expected) {
                return Err(storage(format!("projection diverges at {head}")));
            }
            let (_, trees, blobs) = self.records(&expected, &tx)?;
            let entries = self.inner.repo.tree_entries(&head).map_err(storage)?;
            let snapshots = entries
                .iter()
                .find(|e| e.name == "snapshots")
                .ok_or_else(|| storage("missing snapshot retention tree"))?;
            let artifacts = entries
                .iter()
                .find(|e| e.name == "artifacts")
                .ok_or_else(|| storage("missing artifact retention tree"))?;
            let retained_trees: BTreeSet<_> = self
                .inner
                .repo
                .tree_entries(&snapshots.oid)
                .map_err(storage)?
                .into_iter()
                .map(|e| e.oid)
                .collect();
            let retained_blobs: BTreeSet<_> = self
                .inner
                .repo
                .tree_entries(&artifacts.oid)
                .map_err(storage)?
                .into_iter()
                .map(|e| e.oid)
                .collect();
            if !trees.iter().all(|t| retained_trees.contains(t))
                || !blobs.iter().all(|b| retained_blobs.contains(b))
            {
                return Err(storage("referenced object is not retained"));
            }
        }
        if replay.as_ref() != Some(&state) {
            return Err(storage("replay mismatch"));
        }
        Ok(state)
    }
    fn retry(
        &self,
        goal: &GoalId,
        command: &CommandId,
        request_digest: &str,
    ) -> ApiResult<Option<CommandReceipt>> {
        let Some(head) = self
            .inner
            .repo
            .read_ref(&goal.to_string())
            .map_err(storage)?
        else {
            return Ok(None);
        };
        for head in self
            .inner
            .repo
            .history(&head)
            .map_err(storage)?
            .into_iter()
            .rev()
        {
            let tx: Transaction = self
                .inner
                .repo
                .read_json(&head, "transaction.json")
                .map_err(storage)?;
            if tx.command_id == *command {
                if tx.request_digest != request_digest {
                    return Err(ApiError::new(
                        ApiErrorCode::IdempotencyConflict,
                        "command ID was used for different content or actor",
                    ));
                }
                let state = self.validate_head(&head)?;
                return Ok(Some(CommandReceipt {
                    command_id: command.clone(),
                    version: GoalVersion {
                        head: oid(head)?,
                        sequence: tx.sequence,
                        epoch: state.epoch,
                    },
                    event_ids: tx.events.iter().map(|e| e.id.clone()).collect(),
                }));
            }
        }
        Ok(None)
    }
    fn records(&self, state: &GoalProjection, tx: &Transaction) -> ApiResult<RetainedRecords> {
        let mut records = BTreeMap::new();
        for (id, value) in &state.thunks {
            records.insert(
                format!("objects/thunks/{id}.json"),
                serde_json::to_vec(value).map_err(storage)?,
            );
        }
        for (id, value) in &state.published_batches {
            records.insert(
                format!("objects/batches/{id}.json"),
                serde_json::to_vec(value).map_err(storage)?,
            );
        }
        for (id, value) in &state.checkpoints {
            records.insert(
                format!("objects/checkpoints/{id}.json"),
                serde_json::to_vec(value).map_err(storage)?,
            );
        }
        let mut trees = BTreeSet::new();
        let mut blobs = BTreeSet::new();
        fn visit(
            value: &serde_json::Value,
            trees: &mut BTreeSet<String>,
            blobs: &mut BTreeSet<String>,
        ) {
            match value {
                serde_json::Value::Object(fields) => {
                    for (key, value) in fields {
                        if let Some(s) = value.as_str() {
                            if key == "tree" {
                                trees.insert(s.to_owned());
                            } else if matches!(
                                key.as_str(),
                                "blob" | "output_blob" | "source_map_blob"
                            ) {
                                blobs.insert(s.to_owned());
                            }
                        }
                        visit(value, trees, blobs);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        visit(value, trees, blobs);
                    }
                }
                _ => {}
            }
        }
        visit(
            &serde_json::to_value(state).map_err(storage)?,
            &mut trees,
            &mut blobs,
        );
        visit(
            &serde_json::to_value(tx).map_err(storage)?,
            &mut trees,
            &mut blobs,
        );
        Ok((
            records,
            trees.into_iter().collect(),
            blobs.into_iter().collect(),
        ))
    }
    fn commit(
        &self,
        goal: &GoalId,
        expected: Option<&GitOid>,
        tx: Transaction,
        previous: Option<&GoalProjection>,
    ) -> ApiResult<CommandReceipt> {
        let state = core::reduce(previous, &tx)?;
        let (records, trees, blobs) = self.records(&state, &tx)?;
        let head = self
            .inner
            .repo
            .write_transaction(
                &goal.to_string(),
                expected.map(|o| o.0.as_str()),
                &tx,
                &state,
                TransactionObjects {
                    records: &records,
                    trees: &trees,
                    blobs: &blobs,
                },
            )
            .map_err(|e| {
                if e.downcast_ref::<Conflict>().is_some() {
                    ApiError::new(ApiErrorCode::VersionConflict, "goal changed before commit")
                } else {
                    storage(e)
                }
            })?;
        let receipt = CommandReceipt {
            command_id: tx.command_id.clone(),
            version: GoalVersion {
                head: oid(head)?,
                sequence: state.sequence,
                epoch: state.epoch,
            },
            event_ids: tx.events.iter().map(|e| e.id.clone()).collect(),
        };
        let _ = self.inner.notifications.send(Notification {
            goal_id: goal.clone(),
            command_id: tx.command_id,
            version: receipt.version.clone(),
        });
        Ok(receipt)
    }
    pub fn create_goal(
        &self,
        actor: &Actor,
        command: CommandEnvelope<CreateGoal>,
    ) -> ApiResult<CommandReceipt> {
        reviewer(actor)?;
        let _guard = self.inner.gate.lock().map_err(storage)?;
        let request_digest = digest(&(actor, &command))?;
        let goal_id = command.payload.id.clone();
        if let Some(receipt) = self.retry(&goal_id, &command.command_id, &request_digest)? {
            return Ok(receipt);
        }
        if command.expected_head.is_some() || command.expected_epoch.is_some() {
            return Err(invalid("goal creation expects an absent ref"));
        }
        if self
            .inner
            .repo
            .read_ref(&goal_id.to_string())
            .map_err(storage)?
            .is_some()
        {
            return Err(ApiError::new(
                ApiErrorCode::VersionConflict,
                "goal already exists",
            ));
        }
        let input = command.payload;
        if input.title.trim().is_empty() || input.objective.trim().is_empty() {
            return Err(invalid("goal title and objective are required"));
        }
        self.validate_tree(&input.base)?;
        let at = now();
        let goal = Goal {
            id: goal_id.clone(),
            repository: input.repository,
            title: input.title,
            objective: input.objective,
            base: input.base,
            created_by: actor.id.clone(),
            created_at: at.clone(),
        };
        let tx = Transaction {
            schema_version: 1,
            command_id: command.command_id,
            request_digest,
            goal_id: goal_id.clone(),
            sequence: 1,
            actor: actor.id.clone(),
            at,
            events: vec![EventRecord {
                id: EventId::new(),
                payload: Event::GoalCreated { goal },
            }],
        };
        self.commit(&goal_id, None, tx, None)
    }
    fn validate_tree(&self, source: &SourceSnapshot) -> ApiResult<()> {
        if self
            .inner
            .repo
            .object_kind(&source.tree.0)
            .map_err(|e| invalid(e.to_string()))?
            != "tree"
        {
            return Err(invalid("source snapshot must name a tree"));
        }
        self.inner
            .repo
            .flatten_tree(&source.tree.0)
            .map_err(|e| invalid(e.to_string()))?;
        Ok(())
    }
    fn validate_anchor(&self, state: &GoalProjection, target: &FeedbackTarget) -> ApiResult<()> {
        match target {
            FeedbackTarget::Goal { goal_id } if *goal_id != state.goal.id => {
                return Err(invalid("feedback targets another goal"));
            }
            FeedbackTarget::Thunk { thunk_id } => {
                core::require_current(state, thunk_id)?;
            }
            FeedbackTarget::Code { thunk_id, anchor } => {
                let thunk = core::require_current(state, thunk_id)?;
                let expected = match anchor.side {
                    DiffSide::Before => &thunk.before,
                    DiffSide::After => &thunk.after,
                };
                if &anchor.snapshot != expected {
                    return Err(invalid("anchor must use exact thunk snapshot"));
                }
                self.validate_code_anchor(anchor)?;
            }
            _ => {}
        }
        Ok(())
    }
    fn validate_code_anchor(&self, anchor: &CodeAnchor) -> ApiResult<()> {
        let files = self
            .inner
            .repo
            .flatten_tree(&anchor.snapshot.tree.0)
            .map_err(storage)?;
        let file = files
            .get(&anchor.path.0)
            .ok_or_else(|| invalid("anchor path does not exist"))?;
        if file.oid != anchor.blob.0 || file.mode == "120000" {
            return Err(invalid("anchor blob mismatch or symlink"));
        }
        let bytes = self.inner.repo.read_blob(&file.oid).map_err(storage)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| invalid("code anchors require UTF-8 source"))?;
        let start =
            usize::try_from(anchor.range.start).map_err(|_| invalid("anchor offset overflow"))?;
        let end =
            usize::try_from(anchor.range.end).map_err(|_| invalid("anchor offset overflow"))?;
        if start > end || text.get(start..end) != Some(anchor.selected_text.as_str()) {
            return Err(invalid("anchor range/text mismatch"));
        }
        if !text[..start].ends_with(&anchor.context_before)
            || !text[end..].starts_with(&anchor.context_after)
        {
            return Err(invalid("anchor context mismatch"));
        }
        Ok(())
    }
    pub fn execute(
        &self,
        actor: &Actor,
        goal: &GoalId,
        command: CommandEnvelope<GoalCommand>,
    ) -> ApiResult<CommandReceipt> {
        let _guard = self.inner.gate.lock().map_err(storage)?;
        let request_digest = digest(&(actor, &command))?;
        if let Some(receipt) = self.retry(goal, &command.command_id, &request_digest)? {
            return Ok(receipt);
        }
        let (version, state) = self.load(goal)?;
        if command.expected_head.as_ref() != Some(&version.head) {
            let mut e = ApiError::new(
                ApiErrorCode::VersionConflict,
                "expected head does not match",
            );
            e.current_version = Some(version);
            return Err(e);
        }
        if command.expected_epoch != Some(state.epoch) {
            let mut e = ApiError::new(ApiErrorCode::EpochConflict, "expected current epoch");
            e.current_version = Some(version);
            return Err(e);
        }
        if state.closed
            && !matches!(
                command.payload,
                GoalCommand::RestoreCheckpoint { .. }
                    | GoalCommand::AcknowledgeBatch { .. }
                    | GoalCommand::AcknowledgeControl { .. }
            )
        {
            return Err(invalid("goal is closed"));
        }
        let at = now();
        let events = self.plan(actor, &version, &state, &command.payload, &at)?;
        let tx = Transaction {
            schema_version: 1,
            command_id: command.command_id,
            request_digest,
            goal_id: goal.clone(),
            sequence: state.sequence + 1,
            actor: actor.id.clone(),
            at,
            events: events
                .into_iter()
                .map(|payload| EventRecord {
                    id: EventId::new(),
                    payload,
                })
                .collect(),
        };
        self.commit(goal, Some(&version.head), tx, Some(&state))
    }
    fn plan(
        &self,
        actor: &Actor,
        version: &GoalVersion,
        state: &GoalProjection,
        command: &GoalCommand,
        at: &str,
    ) -> ApiResult<Vec<Event>> {
        let event = match command {
            GoalCommand::SubmitThunks { thunks } => {
                agent(actor)?;
                let ordered = core::submission_order(state, thunks)?;
                let mut result = Vec::new();
                let mut known = state.thunks.clone();
                for mut submission in ordered {
                    self.validate_tree(&submission.before)?;
                    self.validate_tree(&submission.after)?;
                    for dep in &submission.depends_on {
                        let prerequisite = &known[dep];
                        if !source::contains_change(
                            &self.inner.repo,
                            &prerequisite.before.tree.0,
                            &prerequisite.after.tree.0,
                            &submission.before.tree.0,
                        )
                        .map_err(storage)?
                        {
                            return Err(ApiError::new(
                                ApiErrorCode::CompositionConflict,
                                "before tree does not contain declared prerequisite change",
                            ));
                        }
                    }
                    for evidence in &mut submission.validation {
                        self.validate_tree(&evidence.source)?;
                        if evidence.reported_by != actor.id {
                            return Err(invalid("validation reporter must be authenticated actor"));
                        }
                        if let Some(blob) = &evidence.output_blob
                            && self.inner.repo.object_kind(&blob.0).map_err(storage)? != "blob"
                        {
                            return Err(invalid("validation output must be a blob"));
                        }
                        evidence.recorded_at = at.into();
                    }
                    let iteration = match &submission.supersedes {
                        Some(old) => state.thunks[old]
                            .iteration
                            .checked_add(1)
                            .ok_or_else(|| invalid("iteration overflow"))?,
                        None => 1,
                    };
                    let thunk = Thunk {
                        submission,
                        goal_id: state.goal.id.clone(),
                        author: actor.id.clone(),
                        created_at: at.into(),
                        iteration,
                    };
                    known.insert(thunk.id.clone(), thunk.clone());
                    result.push(Event::ThunkSubmitted { thunk });
                }
                return Ok(result);
            }
            GoalCommand::WithdrawThunk { thunk_id, reason } => {
                agent(actor)?;
                core::require_current(state, thunk_id)?;
                Event::ThunkWithdrawn {
                    thunk_id: thunk_id.clone(),
                    reason: reason.clone(),
                }
            }
            GoalCommand::SaveDraft {
                draft,
                expected_edition,
            } => {
                reviewer(actor)?;
                let old = state.drafts.get(&draft.id);
                if old.is_none() && state.draft_editions.contains_key(&draft.id) {
                    return Err(invalid("consumed or discarded draft IDs cannot be reused"));
                }
                if old.map(|d| d.edition) != *expected_edition {
                    return Err(ApiError::new(
                        ApiErrorCode::DraftConflict,
                        "draft edition changed",
                    ));
                }
                if old.is_some_and(|d| d.reviewer != actor.id) {
                    return Err(ApiError::new(
                        ApiErrorCode::Unauthorized,
                        "draft belongs to another reviewer",
                    ));
                }
                match (&draft.thunk_id, &draft.base, &draft.annotated) {
                    (Some(id), Some(base), Some(annotated)) => {
                        let thunk = core::require_current(state, id)?;
                        if base != &thunk.after {
                            return Err(invalid(
                                "review draft base must equal exact thunk after tree",
                            ));
                        }
                        self.validate_tree(annotated)?;
                    }
                    (None, None, None) => {}
                    _ => {
                        return Err(invalid(
                            "code draft requires thunk, base and annotated snapshots together",
                        ));
                    }
                }
                let mut seen = BTreeSet::new();
                let mut sidebar = Vec::new();
                for feedback in &draft.sidebar_feedback {
                    if feedback.body.trim().is_empty() || !seen.insert(feedback.id.clone()) {
                        return Err(invalid("feedback text required; IDs must be unique"));
                    }
                    self.validate_anchor(state, &feedback.target)?;
                    if state
                        .published_batches
                        .values()
                        .any(|b| b.feedback.iter().any(|f| f.id == feedback.id))
                    {
                        return Err(invalid("published feedback ID cannot be reused"));
                    }
                    sidebar.push(Feedback {
                        id: feedback.id.clone(),
                        author: actor.id.clone(),
                        target: feedback.target.clone(),
                        body: feedback.body.clone(),
                        kind: feedback.kind.clone(),
                        origin: FeedbackOrigin::Sidebar,
                    });
                }
                if old.is_some_and(|old| old.thunk_id != draft.thunk_id || old.base != draft.base) {
                    return Err(invalid("draft identity/base cannot change"));
                }
                Event::DraftSaved {
                    draft: ReviewDraft {
                        id: draft.id.clone(),
                        goal_id: state.goal.id.clone(),
                        reviewer: actor.id.clone(),
                        edition: old.map_or(1, |d| d.edition + 1),
                        opened_at: old.map_or_else(|| version.clone(), |d| d.opened_at.clone()),
                        thunk_id: draft.thunk_id.clone(),
                        base: draft.base.clone(),
                        annotated: draft.annotated.clone(),
                        sidebar_feedback: sidebar,
                        marker_intents: draft.marker_intents.clone(),
                        updated_at: at.into(),
                    },
                }
            }
            GoalCommand::DiscardDraft { draft_id } => {
                reviewer(actor)?;
                let draft = state
                    .drafts
                    .get(draft_id)
                    .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "draft not found"))?;
                if draft.reviewer != actor.id {
                    return Err(ApiError::new(
                        ApiErrorCode::Unauthorized,
                        "draft owner mismatch",
                    ));
                }
                Event::DraftDiscarded {
                    draft_id: draft_id.clone(),
                }
            }
            GoalCommand::PublishFeedback {
                preview_digest,
                request,
            } => {
                reviewer(actor)?;
                let preview = self.preview_at(actor, version, state, request)?;
                if preview.digest != *preview_digest {
                    return Err(ApiError::new(
                        ApiErrorCode::PreviewExpired,
                        "publish inputs no longer match preview",
                    ));
                }
                if preview.diagnostics.iter().any(|d| d.blocks_publication) {
                    return Err(invalid("preview contains blocking diagnostics"));
                }
                Event::FeedbackPublished {
                    batch: FeedbackBatch {
                        id: BatchId::new(),
                        goal_id: state.goal.id.clone(),
                        author: actor.id.clone(),
                        reviewed_at: version.clone(),
                        epoch_before: state.epoch,
                        epoch_after: state.epoch + 1,
                        message: request.message.clone(),
                        extractions: preview.extractions,
                        feedback: preview.feedback,
                        edits: preview.edits,
                        scope: request.scope.clone(),
                        invalidation_roots: match request.scope {
                            ImpactScope::GoalWide => state.current_thunks.iter().cloned().collect(),
                            _ => request.invalidation_roots.clone(),
                        },
                        stale_thunks: preview.affected_thunks,
                        consumed_drafts: request.drafts.clone(),
                        published_at: at.into(),
                    },
                }
            }
            GoalCommand::RecordDecision { thunk_id, decision } => {
                reviewer(actor)?;
                core::require_current(state, thunk_id)?;
                if *decision == ReviewDecision::Approved {
                    core::check_approval(state, thunk_id)?;
                }
                Event::DecisionRecorded {
                    thunk_id: thunk_id.clone(),
                    decision: decision.clone(),
                }
            }
            GoalCommand::ResolveFeedback {
                feedback_id,
                resolution,
            } => {
                reviewer(actor)?;
                if resolution.trim().is_empty()
                    || !state
                        .published_batches
                        .values()
                        .any(|b| b.feedback.iter().any(|f| f.id == *feedback_id))
                {
                    return Err(invalid("known feedback and nonempty resolution required"));
                }
                Event::FeedbackResolved {
                    feedback_id: feedback_id.clone(),
                    resolution: resolution.clone(),
                }
            }
            GoalCommand::AcknowledgeBatch { batch_id } => {
                agent(actor)?;
                if !state.published_batches.contains_key(batch_id) {
                    return Err(ApiError::new(ApiErrorCode::NotFound, "batch not found"));
                }
                Event::BatchAcknowledged {
                    batch_id: batch_id.clone(),
                    agent: actor.id.clone(),
                }
            }
            GoalCommand::AcknowledgeControl { event_id } => {
                agent(actor)?;
                if !state.deliveries.iter().any(|d| {
                    d.event_id == *event_id
                        && !matches!(d.payload, DeliveryPayload::Feedback { .. })
                }) {
                    return Err(ApiError::new(
                        ApiErrorCode::NotFound,
                        "control event not found",
                    ));
                }
                Event::ControlAcknowledged {
                    event_id: event_id.clone(),
                    agent: actor.id.clone(),
                }
            }
            GoalCommand::CreateCheckpoint { id, name, source } => {
                reviewer(actor)?;
                self.validate_tree(source)?;
                if state.checkpoints.contains_key(id) || name.trim().is_empty() {
                    return Err(invalid("checkpoint requires new ID and name"));
                }
                let composed = self.compose_active(state)?;
                if composed != *source {
                    return Err(ApiError::new(
                        ApiErrorCode::CompositionConflict,
                        "checkpoint source differs from composed active thunks",
                    ));
                }
                Event::CheckpointCreated {
                    checkpoint: Checkpoint {
                        id: id.clone(),
                        name: name.clone(),
                        state_at: version.clone(),
                        source: source.clone(),
                        created_by: actor.id.clone(),
                        created_at: at.into(),
                    },
                }
            }
            GoalCommand::RestoreCheckpoint {
                checkpoint_id,
                reason,
            } => {
                reviewer(actor)?;
                let checkpoint = state
                    .checkpoints
                    .get(checkpoint_id)
                    .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "checkpoint not found"))?;
                let restored = self.at(&state.goal.id, &checkpoint.state_at.head)?;
                Event::GoalRestored {
                    restore: RestoreRecord {
                        checkpoint_id: checkpoint_id.clone(),
                        restored_from: checkpoint.state_at.head.clone(),
                        previous_head: version.head.clone(),
                        new_epoch: state.epoch + 1,
                        reason: reason.clone(),
                        state: RestoredState {
                            active_feedback: restored.active_feedback,
                            current_thunks: restored.current_thunks,
                            reviews: restored.reviews,
                            resolved_feedback: restored.resolved_feedback,
                        },
                    },
                }
            }
            GoalCommand::CloseGoal { reason } => {
                reviewer(actor)?;
                Event::GoalClosed {
                    reason: reason.clone(),
                }
            }
        };
        Ok(vec![event])
    }
    pub fn compose_active(&self, state: &GoalProjection) -> ApiResult<SourceSnapshot> {
        let changes: Vec<_> = core::queue(state, false)
            .iter()
            .map(|q| (q.thunk.before.tree.0.clone(), q.thunk.after.tree.0.clone()))
            .collect();
        Ok(SourceSnapshot {
            tree: oid(
                source::compose(&self.inner.repo, &state.goal.base.tree.0, &changes)
                    .map_err(|e| ApiError::new(ApiErrorCode::CompositionConflict, e.to_string()))?,
            )?,
        })
    }
    pub fn preview(
        &self,
        actor: &Actor,
        goal: &GoalId,
        request: &PreviewPublish,
    ) -> ApiResult<PublishPreview> {
        reviewer(actor)?;
        let _guard = self.inner.gate.lock().map_err(storage)?;
        let (version, state) = self.load(goal)?;
        self.preview_at(actor, &version, &state, request)
    }
    fn preview_at(
        &self,
        actor: &Actor,
        version: &GoalVersion,
        state: &GoalProjection,
        request: &PreviewPublish,
    ) -> ApiResult<PublishPreview> {
        if state.closed {
            return Err(invalid("goal is closed"));
        }
        let mut feedback = Vec::new();
        let mut edits = Vec::new();
        let mut extractions = Vec::new();
        let mut seen = BTreeSet::new();
        let mut feedback_ids = BTreeSet::new();
        let mut diagnostics = Vec::new();
        let mut changed_paths: BTreeMap<String, DraftId> = BTreeMap::new();
        let mut roots: BTreeSet<_> = request.invalidation_roots.iter().cloned().collect();
        for root in &roots {
            core::require_current(state, root)?;
        }
        let selected = match &request.scope {
            ImpactScope::GoalWide => {
                roots = state.current_thunks.clone();
                state.current_thunks.clone()
            }
            ImpactScope::Selected { thunks } => {
                for id in thunks {
                    core::require_current(state, id)?;
                }
                thunks.iter().cloned().collect()
            }
        };
        if !roots.is_subset(&selected) {
            return Err(invalid("invalidation roots must be in publish scope"));
        }
        for reference in &request.drafts {
            if !seen.insert(reference.draft_id.clone()) {
                return Err(invalid("duplicate draft"));
            }
            let draft = state.drafts.get(&reference.draft_id).ok_or_else(|| {
                ApiError::new(ApiErrorCode::DraftConflict, "draft no longer exists")
            })?;
            if draft.edition != reference.edition {
                return Err(ApiError::new(
                    ApiErrorCode::DraftConflict,
                    "draft edition changed",
                ));
            }
            if draft.reviewer != actor.id {
                return Err(ApiError::new(
                    ApiErrorCode::Unauthorized,
                    "draft owner mismatch",
                ));
            }
            if let Some(id) = &draft.thunk_id {
                core::require_current(state, id)?;
                if !selected.contains(id) {
                    return Err(invalid("draft target outside publish scope"));
                }
            }
            for item in &draft.sidebar_feedback {
                self.validate_anchor(state, &item.target)?;
                if let Some(id) = item.target.thunk_id()
                    && !selected.contains(id)
                {
                    return Err(invalid("feedback target outside publish scope"));
                }
                if !feedback_ids.insert(item.id.clone()) {
                    return Err(invalid("duplicate feedback ID across drafts"));
                }
                feedback.push(item.clone());
            }
            if let (Some(thunk_id), Some(base), Some(annotated)) =
                (&draft.thunk_id, &draft.base, &draft.annotated)
            {
                let base_files = self
                    .inner
                    .repo
                    .flatten_tree(&base.tree.0)
                    .map_err(storage)?;
                let annotated_files = self
                    .inner
                    .repo
                    .flatten_tree(&annotated.tree.0)
                    .map_err(storage)?;
                let mut updates = BTreeMap::new();
                let mut maps = BTreeMap::new();
                let mut pending = Vec::new();
                for (path, file) in &annotated_files {
                    if !path.ends_with(".rs") || file.mode == "120000" {
                        continue;
                    }
                    let annotated_bytes = self.inner.repo.read_blob(&file.oid).map_err(storage)?;
                    let base_bytes = match base_files.get(path) {
                        Some(file) => self.inner.repo.read_blob(&file.oid).map_err(storage)?,
                        None => vec![],
                    };
                    let choices: Vec<_> = draft
                        .marker_intents
                        .iter()
                        .filter(|m| m.path.0 == *path)
                        .map(|m| review::MarkerChoice {
                            start: m.range.start as usize,
                            end: m.range.end as usize,
                            enabled: m.enabled,
                        })
                        .collect();
                    let extraction = review::extract(&base_bytes, &annotated_bytes, &choices)
                        .map_err(invalid)?;
                    let blob = oid(self
                        .inner
                        .repo
                        .hash_blob(&extraction.cleaned)
                        .map_err(storage)?)?;
                    if extraction.cleaned != annotated_bytes {
                        updates.insert(path.clone(), Some((file.mode.clone(), blob.0.clone())));
                    }
                    for annotation in &extraction.annotations {
                        pending.push((
                            path.clone(),
                            blob.clone(),
                            annotation.clone(),
                            extraction.cleaned.clone(),
                        ));
                    }
                    maps.insert(path.clone(), extraction.source_map);
                }
                for marker in &draft.marker_intents {
                    if !annotated_files.contains_key(&marker.path.0)
                        || !marker.path.0.ends_with(".rs")
                    {
                        return Err(invalid("marker choices must identify existing Rust files"));
                    }
                }
                let cleaned = SourceSnapshot {
                    tree: oid(self
                        .inner
                        .repo
                        .tree_with_updates(&annotated.tree.0, &updates)
                        .map_err(storage)?)?,
                };
                for (path, blob, annotation, bytes) in pending {
                    let text = std::str::from_utf8(&bytes)
                        .map_err(|_| invalid("inline source must be UTF-8"))?;
                    let start = annotation.target.start;
                    let end = annotation.target.end;
                    let context_start = text[..start]
                        .char_indices()
                        .rev()
                        .nth(79)
                        .map_or(0, |(i, _)| i);
                    let context_end = text[end..]
                        .char_indices()
                        .nth(80)
                        .map_or(text.len(), |(i, _)| end + i);
                    let stable = digest(&(
                        &draft.id,
                        draft.edition,
                        &path,
                        &annotation.marker,
                        &annotation.body,
                    ))?;
                    let hash = Sha256::digest(stable.as_bytes());
                    let mut id = [0u8; 16];
                    id.copy_from_slice(&hash[..16]);
                    let id = FeedbackId(uuid::Uuid::from_bytes(id));
                    if !feedback_ids.insert(id.clone()) {
                        return Err(invalid("duplicate inline feedback ID"));
                    }
                    feedback.push(Feedback {
                        id,
                        author: actor.id.clone(),
                        target: FeedbackTarget::Code {
                            thunk_id: thunk_id.clone(),
                            anchor: CodeAnchor {
                                snapshot: cleaned.clone(),
                                path: RepoPath::parse(path).map_err(invalid)?,
                                blob,
                                side: DiffSide::After,
                                range: TextRange {
                                    start: start as u64,
                                    end: end as u64,
                                },
                                context_before: text[context_start..start].into(),
                                selected_text: text[start..end].into(),
                                context_after: text[end..context_end].into(),
                            },
                        },
                        body: annotation.body,
                        kind: FeedbackKind::ChangeRequest,
                        origin: FeedbackOrigin::Inline {
                            draft_id: draft.id.clone(),
                            source_range: TextRange {
                                start: annotation.marker.start as u64,
                                end: annotation.marker.end as u64,
                            },
                        },
                    });
                    if annotation.file_level {
                        diagnostics.push(PublishDiagnostic {
                            message: "instruction at EOF is anchored to the file".into(),
                            blocks_publication: false,
                        });
                    }
                }
                let source_map_blob = oid(self
                    .inner
                    .repo
                    .hash_blob(&serde_json::to_vec(&maps).map_err(storage)?)
                    .map_err(storage)?)?;
                extractions.push(DraftExtraction {
                    draft: reference.clone(),
                    base: base.clone(),
                    annotated: annotated.clone(),
                    cleaned: cleaned.clone(),
                    source_map_blob: source_map_blob.clone(),
                });
                if cleaned != *base {
                    if !roots.contains(thunk_id) {
                        return Err(invalid(
                            "reviewer edits require their thunk as an invalidation root",
                        ));
                    }
                    let clean_files = self
                        .inner
                        .repo
                        .flatten_tree(&cleaned.tree.0)
                        .map_err(storage)?;
                    let paths: BTreeSet<_> = base_files
                        .keys()
                        .chain(clean_files.keys())
                        .cloned()
                        .collect();
                    for path in paths {
                        if base_files.get(&path) != clean_files.get(&path)
                            && changed_paths
                                .insert(path.clone(), draft.id.clone())
                                .is_some()
                        {
                            return Err(ApiError::new(
                                ApiErrorCode::CompositionConflict,
                                format!("multiple draft edits overlap at {path}; consolidate them"),
                            ));
                        }
                    }
                    edits.push(ReviewerEdit {
                        thunk_id: thunk_id.clone(),
                        base: base.clone(),
                        edited: cleaned,
                        annotated: annotated.clone(),
                        source_map_blob,
                    });
                }
            }
        }
        if feedback.is_empty() && edits.is_empty() && request.message.trim().is_empty() {
            return Err(invalid("nothing to publish"));
        }
        let affected_thunks = core::dependent_closure(state, roots).into_iter().collect();
        let mut preview = PublishPreview {
            extractions,
            version: version.clone(),
            digest: String::new(),
            feedback,
            edits,
            affected_thunks,
            diagnostics,
        };
        preview.digest = digest(&(actor, request, &preview))?;
        Ok(preview)
    }
    pub fn list_goals(&self, actor: &Actor) -> ApiResult<Vec<GoalDetail>> {
        self.inner
            .repo
            .list_goals()
            .map_err(storage)?
            .into_iter()
            .map(|(id, _)| {
                let goal = GoalId(uuid::Uuid::parse_str(&id).map_err(storage)?);
                self.detail(actor, &goal)
            })
            .collect()
    }
    pub fn detail(&self, actor: &Actor, goal: &GoalId) -> ApiResult<GoalDetail> {
        let (version, state) = self.load(goal)?;
        let queue = core::queue(&state, actor.kind == ActorKind::Reviewer);
        let counts = QueueCounts {
            pending: queue
                .iter()
                .filter(|q| {
                    matches!(
                        q.status,
                        QueueStatus::Pending | QueueStatus::ChangesRequested
                    )
                })
                .count(),
            approved: queue
                .iter()
                .filter(|q| q.status == QueueStatus::Approved)
                .count(),
            stale: queue
                .iter()
                .filter(|q| q.status == QueueStatus::Stale)
                .count(),
            drafts: if actor.kind == ActorKind::Reviewer {
                state
                    .drafts
                    .values()
                    .filter(|d| d.reviewer == actor.id)
                    .count()
            } else {
                0
            },
        };
        Ok(GoalDetail {
            actionable_feedback: state.active_feedback,
            goal: state.goal,
            version,
            counts,
            active_source: None,
            closed: state.closed,
        })
    }
    pub fn inbox(&self, actor: &Actor, goal: &GoalId) -> ApiResult<Versioned<Vec<Delivery>>> {
        agent(actor)?;
        let (version, state) = self.load(goal)?;
        let items = state
            .deliveries
            .into_iter()
            .filter(|d| match &d.payload {
                DeliveryPayload::Feedback { batch } => !state
                    .acknowledgments
                    .contains(&(batch.id.clone(), actor.id.clone())),
                _ => !state
                    .control_acknowledgments
                    .contains(&(d.event_id.clone(), actor.id.clone())),
            })
            .collect();
        Ok(Versioned {
            version,
            data: items,
        })
    }
    pub fn blob(&self, actor: &Actor, goal: &GoalId, object: &GitOid) -> ApiResult<Vec<u8>> {
        let (version, state) = self.load(goal)?;
        let mut allowed = BTreeSet::new();
        let mut add_tree = |tree: &SourceSnapshot| -> ApiResult<()> {
            for e in self
                .inner
                .repo
                .flatten_tree(&tree.tree.0)
                .map_err(storage)?
                .values()
            {
                allowed.insert(e.oid.clone());
            }
            Ok(())
        };
        add_tree(&state.goal.base)?;
        for thunk in state.thunks.values() {
            add_tree(&thunk.before)?;
            add_tree(&thunk.after)?;
            for evidence in &thunk.validation {
                add_tree(&evidence.source)?;
            }
        }
        for checkpoint in state.checkpoints.values() {
            add_tree(&checkpoint.source)?;
        }
        for batch in state.published_batches.values() {
            for extraction in &batch.extractions {
                add_tree(&extraction.base)?;
                add_tree(&extraction.annotated)?;
                add_tree(&extraction.cleaned)?;
            }
            for edit in &batch.edits {
                add_tree(&edit.base)?;
                add_tree(&edit.edited)?;
                add_tree(&edit.annotated)?;
            }
            for feedback in &batch.feedback {
                if let FeedbackTarget::Code { anchor, .. } = &feedback.target {
                    add_tree(&anchor.snapshot)?;
                }
            }
        }
        if actor.kind == ActorKind::Reviewer {
            for head in self.inner.repo.history(&version.head.0).map_err(storage)? {
                let tx: Transaction = self
                    .inner
                    .repo
                    .read_json(&head, "transaction.json")
                    .map_err(storage)?;
                for event in tx.events {
                    if let Event::DraftSaved { draft } = event.payload
                        && draft.reviewer == actor.id
                        && let Some(tree) = &draft.annotated
                    {
                        add_tree(tree)?;
                    }
                }
            }
        }
        for thunk in state.thunks.values() {
            for evidence in &thunk.validation {
                if let Some(blob) = &evidence.output_blob {
                    allowed.insert(blob.0.clone());
                }
            }
        }
        for batch in state.published_batches.values() {
            for extraction in &batch.extractions {
                allowed.insert(extraction.source_map_blob.0.clone());
            }
            for edit in &batch.edits {
                allowed.insert(edit.source_map_blob.0.clone());
            }
        }
        if !allowed.contains(&object.0) {
            return Err(ApiError::new(
                ApiErrorCode::NotFound,
                "blob is not visible in this goal",
            ));
        }
        if self.inner.repo.object_size(&object.0).map_err(storage)? > 16 * 1024 * 1024 {
            return Err(invalid("blob exceeds 16 MiB response limit"));
        }
        self.inner.repo.read_blob(&object.0).map_err(storage)
    }
    pub fn history(
        &self,
        actor: &Actor,
        goal: &GoalId,
        head: Option<&GitOid>,
    ) -> ApiResult<Versioned<Vec<Transaction>>> {
        let (mut version, _) = self.load(goal)?;
        if let Some(head) = head {
            let state = self.at(goal, head)?;
            version = GoalVersion {
                head: head.clone(),
                sequence: state.sequence,
                epoch: state.epoch,
            };
        }
        let mut data = Vec::new();
        for head in self.inner.repo.history(&version.head.0).map_err(storage)? {
            let mut tx: Transaction = self
                .inner
                .repo
                .read_json(&head, "transaction.json")
                .map_err(storage)?;
            if actor.kind != ActorKind::Reviewer || tx.actor != actor.id {
                tx.events.retain(|e| {
                    !matches!(
                        e.payload,
                        Event::DraftSaved { .. } | Event::DraftDiscarded { .. }
                    )
                });
            }
            data.push(tx);
        }
        Ok(Versioned { version, data })
    }
    pub fn materialize_checkpoint(
        &self,
        actor: &Actor,
        goal: &GoalId,
        id: &CheckpointId,
        destination: &Path,
    ) -> ApiResult<String> {
        reviewer(actor)?;
        let (_, state) = self.load(goal)?;
        let checkpoint = state
            .checkpoints
            .get(id)
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "checkpoint not found"))?;
        source::materialize(&self.inner.repo, &checkpoint.source.tree.0, destination)
            .map_err(storage)
    }
}
