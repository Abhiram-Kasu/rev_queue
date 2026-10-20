use crate::{core, diff, model::*, service::Service};
use axum::{
    Json as AxumJson, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, FromRequest, FromRequestParts, Path as AxumPath, Query as AxumQuery,
        Request, State,
    },
    http::{HeaderMap, StatusCode, request::Parts},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post, put},
};
use futures_util::stream;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{collections::BTreeMap, convert::Infallible, sync::Arc, time::Duration};

struct Json<T>(T);
impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        AxumJson(self.0).into_response()
    }
}
impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Json<T> {
    type Rejection = ApiError;
    async fn from_request(request: Request, state: &S) -> ApiResult<Self> {
        AxumJson::<T>::from_request(request, state)
            .await
            .map(|value| Self(value.0))
            .map_err(|error| ApiError::new(ApiErrorCode::InvalidInput, error.body_text()))
    }
}
struct Path<T>(T);
impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for Path<T> {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> ApiResult<Self> {
        AxumPath::<T>::from_request_parts(parts, state)
            .await
            .map(|value| Self(value.0))
            .map_err(|error| ApiError::new(ApiErrorCode::InvalidInput, error.body_text()))
    }
}
struct Query<T>(T);
impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Query<T> {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> ApiResult<Self> {
        AxumQuery::<T>::from_request_parts(parts, state)
            .await
            .map(|value| Self(value.0))
            .map_err(|error| ApiError::new(ApiErrorCode::InvalidInput, error.body_text()))
    }
}
async fn authenticate(
    State(state): State<ApiState>,
    request: Request,
    next: axum::middleware::Next,
) -> ApiResult<Response> {
    auth(&state, request.headers())?;
    Ok(next.run(request).await)
}

#[derive(Clone)]
pub struct ApiState {
    pub service: Service,
    pub capabilities: Arc<BTreeMap<String, Actor>>,
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.code {
            ApiErrorCode::NotFound => StatusCode::NOT_FOUND,
            ApiErrorCode::Unauthorized => StatusCode::FORBIDDEN,
            ApiErrorCode::VersionConflict
            | ApiErrorCode::EpochConflict
            | ApiErrorCode::DraftConflict
            | ApiErrorCode::PreviewExpired
            | ApiErrorCode::IdempotencyConflict => StatusCode::CONFLICT,
            ApiErrorCode::StorageFailure => StatusCode::INTERNAL_SERVER_ERROR,
            ApiErrorCode::UpgradeRequired => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        (status, Json(self)).into_response()
    }
}
fn auth(state: &ApiState, headers: &HeaderMap) -> ApiResult<Actor> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::new(ApiErrorCode::Unauthorized, "bearer capability required"))?;
    state
        .capabilities
        .get(token)
        .cloned()
        .ok_or_else(|| ApiError::new(ApiErrorCode::Unauthorized, "invalid capability"))
}
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::new(ApiErrorCode::StorageFailure, e.to_string()))?
}
fn map_command<T>(
    envelope: CommandEnvelope<T>,
    f: impl FnOnce(T) -> GoalCommand,
) -> CommandEnvelope<GoalCommand> {
    CommandEnvelope {
        command_id: envelope.command_id,
        expected_head: envelope.expected_head,
        expected_epoch: envelope.expected_epoch,
        payload: f(envelope.payload),
    }
}
async fn run(
    state: ApiState,
    headers: HeaderMap,
    goal: GoalId,
    command: CommandEnvelope<GoalCommand>,
) -> ApiResult<Json<CommandReceipt>> {
    let actor = auth(&state, &headers)?;
    blocking(move || state.service.execute(&actor, &goal, command))
        .await
        .map(Json)
}
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1/goals", get(goals).post(create_goal))
        .route("/v1/goals/{goal}", get(goal_detail))
        .route("/v1/goals/{goal}/queue", get(queue))
        .route("/v1/goals/{goal}/commands", post(command))
        .route("/v1/goals/{goal}/thunks", post(submit))
        .route("/v1/goals/{goal}/thunks/{thunk}", get(thunk_detail))
        .route("/v1/goals/{goal}/thunks/{thunk}/diff", get(thunk_diff))
        .route("/v1/goals/{goal}/thunks/{thunk}/decisions", post(decision))
        .route(
            "/v1/goals/{goal}/thunks/{thunk}/withdrawals",
            post(withdraw),
        )
        .route("/v1/goals/{goal}/drafts", get(drafts))
        .route("/v1/goals/{goal}/drafts/{draft}", put(save_draft))
        .route(
            "/v1/goals/{goal}/drafts/{draft}/discard",
            post(discard_draft),
        )
        .route("/v1/goals/{goal}/publish-preview", post(preview))
        .route(
            "/v1/goals/{goal}/feedback-batches",
            get(inbox).post(publish),
        )
        .route("/v1/goals/{goal}/feedback-batches/{batch}/ack", post(ack))
        .route(
            "/v1/goals/{goal}/feedback/{feedback}/resolve",
            post(resolve),
        )
        .route("/v1/goals/{goal}/checkpoints", post(checkpoint))
        .route("/v1/goals/{goal}/restores", post(restore))
        .route("/v1/goals/{goal}/history", get(history))
        .route("/v1/goals/{goal}/events", get(events))
        .route("/v1/goals/{goal}/blobs/{oid}", get(blob))
        .route("/v1/goals/{goal}/source/patch", post(patch))
        .route("/v1/goals/{goal}/source/capture", post(capture))
        .route("/v1/goals/{goal}/source/compose", post(compose))
        .route("/v1/goals/{goal}/anchors/reattach", post(reattach))
        .route(
            "/v1/goals/{goal}/checkpoints/{checkpoint}/workspace",
            post(workspace),
        )
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate,
        ))
        .with_state(state)
}
async fn goals(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<GoalDetail>>> {
    let actor = auth(&state, &headers)?;
    blocking(move || state.service.list_goals(&actor))
        .await
        .map(Json)
}
async fn create_goal(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(command): Json<CommandEnvelope<CreateGoal>>,
) -> ApiResult<Json<CommandReceipt>> {
    let actor = auth(&state, &headers)?;
    blocking(move || state.service.create_goal(&actor, command))
        .await
        .map(Json)
}
async fn goal_detail(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
) -> ApiResult<Json<GoalDetail>> {
    let actor = auth(&state, &headers)?;
    blocking(move || state.service.detail(&actor, &goal))
        .await
        .map(Json)
}
async fn command(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Json(command): Json<CommandEnvelope<GoalCommand>>,
) -> ApiResult<Json<CommandReceipt>> {
    run(state, headers, goal, command).await
}
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PageQuery {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub head: Option<GitOid>,
}
#[derive(Serialize, Deserialize)]
struct Cursor {
    head: GitOid,
    offset: usize,
}
fn page_state(
    service: &Service,
    goal: &GoalId,
    query: &PageQuery,
) -> ApiResult<(GoalVersion, GoalProjection, usize, usize)> {
    let cursor: Option<Cursor> = query
        .cursor
        .as_ref()
        .map(|s| {
            serde_json::from_str(s)
                .map_err(|_| ApiError::new(ApiErrorCode::InvalidInput, "invalid page cursor"))
        })
        .transpose()?;
    let (version, state) = match cursor.as_ref().map(|c| &c.head).or(query.head.as_ref()) {
        Some(head) => {
            let state = service.at(goal, head)?;
            (
                GoalVersion {
                    head: head.clone(),
                    sequence: state.sequence,
                    epoch: state.epoch,
                },
                state,
            )
        }
        None => service.load(goal)?,
    };
    Ok((
        version,
        state,
        cursor.map_or(0, |c| c.offset),
        query.limit.unwrap_or(100).clamp(1, 200),
    ))
}
fn paginate<T>(
    version: GoalVersion,
    items: Vec<T>,
    offset: usize,
    limit: usize,
) -> ApiResult<Page<T>> {
    if offset > items.len() {
        return Err(ApiError::new(
            ApiErrorCode::InvalidInput,
            "cursor offset outside result",
        ));
    }
    let next = offset.saturating_add(limit).min(items.len());
    let next_cursor = if next < items.len() {
        Some(
            serde_json::to_string(&Cursor {
                head: version.head.clone(),
                offset: next,
            })
            .map_err(|e| ApiError::new(ApiErrorCode::StorageFailure, e.to_string()))?,
        )
    } else {
        None
    };
    Ok(Page {
        version,
        items: items.into_iter().skip(offset).take(limit).collect(),
        next_cursor,
    })
}
async fn queue(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Page<QueueItem>>> {
    let actor = auth(&state, &headers)?;
    blocking(move || {
        let (version, mut state, offset, limit) = page_state(&state.service, &goal, &query)?;
        state.drafts.retain(|_, d| d.reviewer == actor.id);
        paginate(
            version,
            core::queue(&state, actor.kind == ActorKind::Reviewer),
            offset,
            limit,
        )
    })
    .await
    .map(Json)
}
#[derive(Deserialize, Serialize)]
pub struct SubmitThunks {
    pub thunks: Vec<ThunkSubmission>,
}
async fn submit(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Json(command): Json<CommandEnvelope<SubmitThunks>>,
) -> ApiResult<Json<CommandReceipt>> {
    run(
        state,
        headers,
        goal,
        map_command(command, |p| GoalCommand::SubmitThunks { thunks: p.thunks }),
    )
    .await
}
async fn thunk_detail(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((goal, thunk)): Path<(GoalId, ThunkId)>,
) -> ApiResult<Json<ThunkDetail>> {
    let actor = auth(&state, &headers)?;
    blocking(move || {
        let (version, projection) = state.service.load(&goal)?;
        let record = projection
            .thunks
            .get(&thunk)
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "thunk not found"))?;
        let files = diff::files(state.service.repository(), &record.before, &record.after)?;
        let mut view = projection.clone();
        view.drafts.retain(|_, d| d.reviewer == actor.id);
        view.current_thunks.insert(thunk.clone());
        let item = core::queue(&view, actor.kind == ActorKind::Reviewer)
            .into_iter()
            .find(|i| i.thunk.id == thunk)
            .ok_or_else(|| {
                ApiError::new(ApiErrorCode::StorageFailure, "invalid dependency graph")
            })?;
        let mut predecessors = Vec::new();
        let mut prior = record.supersedes.clone();
        while let Some(id) = prior {
            predecessors.push(id.clone());
            prior = projection.thunks[&id].supersedes.clone();
        }
        let dependents = projection
            .thunks
            .values()
            .filter(|t| t.depends_on.contains(&thunk))
            .map(|t| t.id.clone())
            .collect();
        let feedback = projection
            .published_batches
            .values()
            .flat_map(|b| &b.feedback)
            .filter(|f| f.target.thunk_id() == Some(&thunk))
            .cloned()
            .collect();
        Ok(ThunkDetail {
            version,
            item,
            predecessors,
            dependents,
            feedback,
            files,
        })
    })
    .await
    .map(Json)
}
async fn thunk_diff(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((goal, thunk)): Path<(GoalId, ThunkId)>,
) -> ApiResult<Json<Versioned<Vec<FileDiff>>>> {
    auth(&state, &headers)?;
    blocking(move || {
        let (version, projection) = state.service.load(&goal)?;
        let thunk = projection
            .thunks
            .get(&thunk)
            .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "thunk not found"))?;
        Ok(Versioned {
            version,
            data: diff::files(state.service.repository(), &thunk.before, &thunk.after)?,
        })
    })
    .await
    .map(Json)
}
#[derive(Serialize, Deserialize)]
pub struct DecisionInput {
    pub decision: ReviewDecision,
}
async fn decision(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((goal, thunk_id)): Path<(GoalId, ThunkId)>,
    Json(command): Json<CommandEnvelope<DecisionInput>>,
) -> ApiResult<Json<CommandReceipt>> {
    run(
        state,
        headers,
        goal,
        map_command(command, |p| GoalCommand::RecordDecision {
            thunk_id,
            decision: p.decision,
        }),
    )
    .await
}
#[derive(Serialize, Deserialize)]
pub struct ReasonInput {
    pub reason: String,
}
async fn withdraw(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((goal, thunk_id)): Path<(GoalId, ThunkId)>,
    Json(command): Json<CommandEnvelope<ReasonInput>>,
) -> ApiResult<Json<CommandReceipt>> {
    run(
        state,
        headers,
        goal,
        map_command(command, |p| GoalCommand::WithdrawThunk {
            thunk_id,
            reason: p.reason,
        }),
    )
    .await
}
#[derive(Serialize, Deserialize)]
pub struct SaveDraftInput {
    pub draft: ReviewDraftInput,
    pub expected_edition: Option<u64>,
}
async fn save_draft(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((goal, draft_id)): Path<(GoalId, DraftId)>,
    Json(command): Json<CommandEnvelope<SaveDraftInput>>,
) -> ApiResult<Json<CommandReceipt>> {
    if command.payload.draft.id != draft_id {
        return Err(ApiError::new(
            ApiErrorCode::InvalidInput,
            "draft ID differs from route",
        ));
    }
    run(
        state,
        headers,
        goal,
        map_command(command, |p| GoalCommand::SaveDraft {
            draft: p.draft,
            expected_edition: p.expected_edition,
        }),
    )
    .await
}
async fn discard_draft(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((goal, draft_id)): Path<(GoalId, DraftId)>,
    Json(command): Json<CommandEnvelope<()>>,
) -> ApiResult<Json<CommandReceipt>> {
    run(
        state,
        headers,
        goal,
        map_command(command, |()| GoalCommand::DiscardDraft { draft_id }),
    )
    .await
}
async fn drafts(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Versioned<Vec<ReviewDraft>>>> {
    let actor = auth(&state, &headers)?;
    if actor.kind != ActorKind::Reviewer {
        return Err(ApiError::new(
            ApiErrorCode::Unauthorized,
            "reviewer capability required",
        ));
    }
    blocking(move || {
        let (version, projection, _, _) = page_state(&state.service, &goal, &query)?;
        Ok(Versioned {
            version,
            data: projection
                .drafts
                .into_values()
                .filter(|d| d.reviewer == actor.id)
                .collect(),
        })
    })
    .await
    .map(Json)
}
async fn preview(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Json(request): Json<PreviewPublish>,
) -> ApiResult<Json<PublishPreview>> {
    let actor = auth(&state, &headers)?;
    blocking(move || state.service.preview(&actor, &goal, &request))
        .await
        .map(Json)
}
#[derive(Serialize, Deserialize)]
pub struct PublishInput {
    pub preview_digest: String,
    pub request: PreviewPublish,
}
async fn publish(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Json(command): Json<CommandEnvelope<PublishInput>>,
) -> ApiResult<Json<CommandReceipt>> {
    run(
        state,
        headers,
        goal,
        map_command(command, |p| GoalCommand::PublishFeedback {
            preview_digest: p.preview_digest,
            request: p.request,
        }),
    )
    .await
}
#[derive(Deserialize, Default)]
pub struct InboxQuery {
    pub after: Option<u64>,
    pub include_acknowledged: Option<bool>,
}
async fn inbox(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Query(query): Query<InboxQuery>,
) -> ApiResult<Json<Versioned<Vec<Delivery>>>> {
    let actor = auth(&state, &headers)?;
    blocking(move || {
        let mut inbox =
            if actor.kind == ActorKind::Reviewer || query.include_acknowledged == Some(true) {
                let (version, projection) = state.service.load(&goal)?;
                Versioned {
                    version,
                    data: projection.deliveries,
                }
            } else {
                state.service.inbox(&actor, &goal)?
            };
        inbox
            .data
            .retain(|d| query.after.is_none_or(|seq| d.sequence > seq));
        Ok(inbox)
    })
    .await
    .map(Json)
}
async fn ack(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((goal, batch_id)): Path<(GoalId, BatchId)>,
    Json(command): Json<CommandEnvelope<()>>,
) -> ApiResult<Json<CommandReceipt>> {
    run(
        state,
        headers,
        goal,
        map_command(command, |()| GoalCommand::AcknowledgeBatch { batch_id }),
    )
    .await
}
#[derive(Serialize, Deserialize)]
pub struct ResolutionInput {
    pub resolution: String,
}
async fn resolve(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((goal, feedback_id)): Path<(GoalId, FeedbackId)>,
    Json(command): Json<CommandEnvelope<ResolutionInput>>,
) -> ApiResult<Json<CommandReceipt>> {
    run(
        state,
        headers,
        goal,
        map_command(command, |p| GoalCommand::ResolveFeedback {
            feedback_id,
            resolution: p.resolution,
        }),
    )
    .await
}
#[derive(Serialize, Deserialize)]
pub struct CheckpointInput {
    pub id: CheckpointId,
    pub name: String,
    pub source: SourceSnapshot,
}
async fn checkpoint(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Json(command): Json<CommandEnvelope<CheckpointInput>>,
) -> ApiResult<Json<CommandReceipt>> {
    run(
        state,
        headers,
        goal,
        map_command(command, |p| GoalCommand::CreateCheckpoint {
            id: p.id,
            name: p.name,
            source: p.source,
        }),
    )
    .await
}
#[derive(Serialize, Deserialize)]
pub struct RestoreInput {
    pub checkpoint_id: CheckpointId,
    pub reason: String,
}
async fn restore(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Json(command): Json<CommandEnvelope<RestoreInput>>,
) -> ApiResult<Json<CommandReceipt>> {
    run(
        state,
        headers,
        goal,
        map_command(command, |p| GoalCommand::RestoreCheckpoint {
            checkpoint_id: p.checkpoint_id,
            reason: p.reason,
        }),
    )
    .await
}
async fn history(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<Page<Transaction>>> {
    let actor = auth(&state, &headers)?;
    blocking(move || {
        let (version, _, offset, limit) = page_state(&state.service, &goal, &query)?;
        let history = state.service.history(&actor, &goal, Some(&version.head))?;
        paginate(
            version,
            history.data.into_iter().rev().collect(),
            offset,
            limit,
        )
    })
    .await
    .map(Json)
}
async fn blob(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((goal, object)): Path<(GoalId, GitOid)>,
) -> ApiResult<Response> {
    let actor = auth(&state, &headers)?;
    let bytes = blocking(move || state.service.blob(&actor, &goal, &object)).await?;
    Ok((
        [("content-type", "application/octet-stream")],
        Body::from(bytes),
    )
        .into_response())
}
async fn events(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
) -> ApiResult<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>> {
    auth(&state, &headers)?;
    let receiver = state.service.subscribe();
    let check = state.service.clone();
    let check_goal = goal.clone();
    blocking(move || check.load(&check_goal)).await?;
    let stream = stream::unfold((receiver, goal), |(mut receiver, goal)| async move {
        loop {
            match receiver.recv().await {
                Ok(notification) if notification.goal_id == goal => {
                    return Some((
                        Ok(Event::default()
                            .event("goal_changed")
                            .id(notification.version.sequence.to_string())
                            .json_data(&notification)
                            .unwrap_or_else(|_| Event::default().event("refresh"))),
                        (receiver, goal),
                    ));
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    return Some((
                        Ok(Event::default().event("refresh").data("refetch")),
                        (receiver, goal),
                    ));
                }
                Err(_) => return None,
            }
        }
    });
    Ok(Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15))))
}
#[derive(Serialize, Deserialize)]
pub struct PatchInput {
    pub base: SourceSnapshot,
    pub patch: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
pub struct CaptureInput {
    pub base: SourceSnapshot,
    pub worktree: String,
    pub paths: Vec<RepoPath>,
}
fn require_source(state: &GoalProjection, snapshot: &SourceSnapshot) -> ApiResult<()> {
    if state.goal.base == *snapshot
        || state
            .thunks
            .values()
            .any(|t| t.before == *snapshot || t.after == *snapshot)
        || state.checkpoints.values().any(|c| c.source == *snapshot)
    {
        return Ok(());
    }
    Err(ApiError::new(
        ApiErrorCode::NotFound,
        "base snapshot is not owned by this goal",
    ))
}
async fn patch(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Json(request): Json<PatchInput>,
) -> ApiResult<Json<SourceSnapshot>> {
    auth(&state, &headers)?;
    blocking(move || {
        let (_, projection) = state.service.load(&goal)?;
        require_source(&projection, &request.base)?;
        let tree = crate::source::apply_patch(
            state.service.repository(),
            &request.base.tree.0,
            &request.patch,
        )
        .map_err(|e| ApiError::new(ApiErrorCode::CompositionConflict, e.to_string()))?;
        Ok(SourceSnapshot {
            tree: GitOid::parse(tree)
                .map_err(|e| ApiError::new(ApiErrorCode::StorageFailure, e))?,
        })
    })
    .await
    .map(Json)
}
async fn capture(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Json(request): Json<CaptureInput>,
) -> ApiResult<Json<SourceSnapshot>> {
    auth(&state, &headers)?;
    blocking(move || {
        let (_, projection) = state.service.load(&goal)?;
        require_source(&projection, &request.base)?;
        let tree = crate::source::capture(
            state.service.repository(),
            std::path::Path::new(&request.worktree),
            &request.base.tree.0,
            &request.paths.into_iter().map(|p| p.0).collect::<Vec<_>>(),
        )
        .map_err(|e| ApiError::new(ApiErrorCode::InvalidInput, e.to_string()))?;
        Ok(SourceSnapshot {
            tree: GitOid::parse(tree)
                .map_err(|e| ApiError::new(ApiErrorCode::StorageFailure, e))?,
        })
    })
    .await
    .map(Json)
}
async fn compose(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
) -> ApiResult<Json<SourceSnapshot>> {
    auth(&state, &headers)?;
    blocking(move || {
        let (_, projection) = state.service.load(&goal)?;
        state.service.compose_active(&projection)
    })
    .await
    .map(Json)
}
#[derive(Serialize, Deserialize)]
pub struct WorkspaceInput {
    pub destination: String,
}
async fn workspace(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((goal, checkpoint)): Path<(GoalId, CheckpointId)>,
    Json(request): Json<WorkspaceInput>,
) -> ApiResult<Json<String>> {
    let actor = auth(&state, &headers)?;
    blocking(move || {
        state.service.materialize_checkpoint(
            &actor,
            &goal,
            &checkpoint,
            std::path::Path::new(&request.destination),
        )
    })
    .await
    .map(Json)
}

#[derive(Serialize, Deserialize)]
pub struct ReattachInput {
    pub anchor: CodeAnchor,
    pub blob: GitOid,
}
async fn reattach(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(goal): Path<GoalId>,
    Json(request): Json<ReattachInput>,
) -> ApiResult<Json<crate::anchors::Attachment>> {
    let actor = auth(&state, &headers)?;
    blocking(move || {
        state.service.blob(&actor, &goal, &request.anchor.blob)?;
        let bytes = state.service.blob(&actor, &goal, &request.blob)?;
        Ok(crate::anchors::reattach(
            &request.anchor,
            &request.blob,
            &bytes,
        ))
    })
    .await
    .map(Json)
}
