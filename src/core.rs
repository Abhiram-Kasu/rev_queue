use crate::model::*;
use std::collections::{BTreeMap, BTreeSet};

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorCode::InvalidInput, message)
}

pub fn initial(goal: Goal) -> GoalProjection {
    GoalProjection {
        active_feedback: BTreeSet::new(),
        draft_editions: BTreeMap::new(),
        goal,
        sequence: 0,
        epoch: 0,
        closed: false,
        thunks: BTreeMap::new(),
        current_thunks: BTreeSet::new(),
        reviews: BTreeMap::new(),
        drafts: BTreeMap::new(),
        published_batches: BTreeMap::new(),
        resolved_feedback: BTreeSet::new(),
        acknowledgments: BTreeSet::new(),
        control_acknowledgments: BTreeSet::new(),
        checkpoints: BTreeMap::new(),
        reverse_dependencies: BTreeMap::new(),
        submission_sequences: BTreeMap::new(),
        deliveries: Vec::new(),
    }
}

pub fn dependent_closure(
    state: &GoalProjection,
    roots: impl IntoIterator<Item = ThunkId>,
) -> BTreeSet<ThunkId> {
    let mut result = BTreeSet::new();
    let mut pending: Vec<_> = roots.into_iter().collect();
    while let Some(id) = pending.pop() {
        if !result.insert(id.clone()) {
            continue;
        }
        if let Some(dependents) = state.reverse_dependencies.get(&id) {
            pending.extend(
                dependents
                    .iter()
                    .filter(|d| state.current_thunks.contains(*d))
                    .cloned(),
            );
        }
    }
    result
}
fn stale(state: &mut GoalProjection, ids: BTreeSet<ThunkId>, cause: StaleCause) {
    for id in ids {
        if state.current_thunks.contains(&id) {
            let review = state.reviews.entry(id).or_default();
            if !review.stale_causes.contains(&cause) {
                review.stale_causes.push(cause.clone());
            }
        }
    }
}
fn rebuild_dependencies(state: &mut GoalProjection) {
    state.reverse_dependencies.clear();
    for id in &state.current_thunks {
        for dep in &state.thunks[id].depends_on {
            state
                .reverse_dependencies
                .entry(dep.clone())
                .or_default()
                .insert(id.clone());
        }
    }
}
pub fn reduce(previous: Option<&GoalProjection>, tx: &Transaction) -> ApiResult<GoalProjection> {
    if tx.schema_version != 1 {
        return Err(ApiError::new(
            ApiErrorCode::UpgradeRequired,
            "unsupported transaction schema",
        ));
    }
    let mut state = match previous {
        Some(state) => {
            if tx.sequence != state.sequence + 1 || tx.goal_id != state.goal.id {
                return Err(invalid("non-contiguous transaction history"));
            }
            state.clone()
        }
        None => {
            if tx.sequence != 1 {
                return Err(invalid("initial sequence must be one"));
            }
            match tx.events.first().map(|e| &e.payload) {
                Some(Event::GoalCreated { goal }) if goal.id == tx.goal_id => initial(goal.clone()),
                _ => return Err(invalid("history must start with goal creation")),
            }
        }
    };
    for record in &tx.events {
        match &record.payload {
            Event::GoalCreated { .. } => {
                if previous.is_some() || state.sequence != 0 {
                    return Err(invalid("duplicate goal creation"));
                }
            }
            Event::ThunkSubmitted { thunk } => {
                if state.thunks.contains_key(&thunk.id) {
                    return Err(invalid("duplicate thunk ID"));
                }
                if let Some(old) = &thunk.supersedes {
                    let mut closure = dependent_closure(&state, [old.clone()]);
                    closure.remove(old);
                    stale(
                        &mut state,
                        closure,
                        StaleCause::ReplacedDependency {
                            old: old.clone(),
                            replacement: thunk.id.clone(),
                        },
                    );
                    state.current_thunks.remove(old);
                }
                state.current_thunks.insert(thunk.id.clone());
                state
                    .submission_sequences
                    .insert(thunk.id.clone(), tx.sequence);
                state
                    .reviews
                    .insert(thunk.id.clone(), ThunkReviewState::default());
                state.thunks.insert(thunk.id.clone(), thunk.clone());
                rebuild_dependencies(&mut state);
            }
            Event::ThunkWithdrawn { thunk_id, .. } => {
                let mut closure = dependent_closure(&state, [thunk_id.clone()]);
                closure.remove(thunk_id);
                stale(
                    &mut state,
                    closure,
                    StaleCause::WithdrawnDependency {
                        thunk_id: thunk_id.clone(),
                    },
                );
                state.current_thunks.remove(thunk_id);
                state.reviews.entry(thunk_id.clone()).or_default().withdrawn = true;
                rebuild_dependencies(&mut state);
            }
            Event::DraftSaved { draft } => {
                state.draft_editions.insert(draft.id.clone(), draft.edition);
                state.drafts.insert(draft.id.clone(), draft.clone());
            }
            Event::DraftDiscarded { draft_id } => {
                state.drafts.remove(draft_id);
            }
            Event::FeedbackPublished { batch } => {
                if batch.epoch_before != state.epoch || batch.epoch_after != state.epoch + 1 {
                    return Err(invalid("invalid batch epoch"));
                }
                state.epoch = batch.epoch_after;
                stale(
                    &mut state,
                    batch.stale_thunks.iter().cloned().collect(),
                    StaleCause::Feedback {
                        batch_id: batch.id.clone(),
                    },
                );
                for consumed in &batch.consumed_drafts {
                    state.drafts.remove(&consumed.draft_id);
                }
                state
                    .active_feedback
                    .extend(batch.feedback.iter().map(|f| f.id.clone()));
                state
                    .published_batches
                    .insert(batch.id.clone(), batch.clone());
                state.deliveries.push(Delivery {
                    event_id: record.id.clone(),
                    sequence: tx.sequence,
                    epoch: state.epoch,
                    payload: DeliveryPayload::Feedback {
                        batch: batch.clone(),
                    },
                });
            }
            Event::DecisionRecorded { thunk_id, decision } => {
                state.reviews.entry(thunk_id.clone()).or_default().decision = Some(decision.clone())
            }
            Event::FeedbackResolved { feedback_id, .. } => {
                state.resolved_feedback.insert(feedback_id.clone());
            }
            Event::BatchAcknowledged { batch_id, agent } => {
                state
                    .acknowledgments
                    .insert((batch_id.clone(), agent.clone()));
            }
            Event::ControlAcknowledged { event_id, agent } => {
                state
                    .control_acknowledgments
                    .insert((event_id.clone(), agent.clone()));
            }
            Event::CheckpointCreated { checkpoint } => {
                state
                    .checkpoints
                    .insert(checkpoint.id.clone(), checkpoint.clone());
            }
            Event::GoalRestored { restore } => {
                if restore.new_epoch != state.epoch + 1 {
                    return Err(invalid("invalid restore epoch"));
                }
                state.epoch = restore.new_epoch;
                state.current_thunks = restore.state.current_thunks.clone();
                for (id, review) in &restore.state.reviews {
                    state.reviews.insert(id.clone(), review.clone());
                }
                state.resolved_feedback = restore.state.resolved_feedback.clone();
                state.active_feedback = restore.state.active_feedback.clone();
                state.drafts.clear();
                state.closed = false;
                rebuild_dependencies(&mut state);
                state.deliveries.push(Delivery {
                    event_id: record.id.clone(),
                    sequence: tx.sequence,
                    epoch: state.epoch,
                    payload: DeliveryPayload::Restore {
                        restore: restore.clone(),
                    },
                });
            }
            Event::GoalClosed { reason } => {
                state.closed = true;
                state.deliveries.push(Delivery {
                    event_id: record.id.clone(),
                    sequence: tx.sequence,
                    epoch: state.epoch,
                    payload: DeliveryPayload::Closed {
                        reason: reason.clone(),
                    },
                });
            }
        }
    }
    state.sequence = tx.sequence;
    Ok(state)
}

pub fn submission_order(
    state: &GoalProjection,
    inputs: &[ThunkSubmission],
) -> ApiResult<Vec<ThunkSubmission>> {
    if inputs.is_empty() {
        return Err(invalid("submission batch is empty"));
    }
    let mut remaining = BTreeMap::new();
    let mut replaced = BTreeSet::new();
    for thunk in inputs {
        if state.thunks.contains_key(&thunk.id)
            || remaining.insert(thunk.id.clone(), thunk.clone()).is_some()
        {
            return Err(invalid("thunk ID already exists"));
        }
        if thunk.based_on_epoch != state.epoch {
            return Err(ApiError::new(
                ApiErrorCode::EpochConflict,
                "thunk was prepared against an old epoch",
            ));
        }
        if thunk.title.trim().is_empty() || thunk.confidence.basis_points > 10_000 {
            return Err(invalid(
                "title is required and confidence must be 0..=10000",
            ));
        }
        let unique: BTreeSet<_> = thunk.depends_on.iter().collect();
        if unique.len() != thunk.depends_on.len() {
            return Err(invalid("duplicate prerequisite"));
        }
        if let Some(old) = &thunk.supersedes
            && (!state.current_thunks.contains(old) || !replaced.insert(old.clone()))
        {
            return Err(ApiError::new(
                ApiErrorCode::DependencyNotCurrent,
                "replacement must supersede a unique current thunk",
            ));
        }
        for id in thunk.related_to.iter().chain(&thunk.provenance) {
            if !state.thunks.contains_key(id) && !inputs.iter().any(|t| &t.id == id) {
                return Err(invalid("unknown related/provenance thunk"));
            }
        }
        for feedback in &thunk.responds_to {
            if !state
                .published_batches
                .values()
                .any(|b| b.feedback.iter().any(|f| &f.id == feedback))
            {
                return Err(invalid("unknown published feedback"));
            }
        }
    }
    let invalidated = dependent_closure(state, replaced.iter().cloned());
    let mut available = state
        .current_thunks
        .difference(&replaced)
        .cloned()
        .collect::<BTreeSet<_>>();
    for thunk in inputs {
        for dep in &thunk.depends_on {
            if !available.contains(dep) && !remaining.contains_key(dep) {
                return Err(ApiError::new(
                    ApiErrorCode::DependencyNotCurrent,
                    "prerequisite must be current after this transaction",
                ));
            }
            if let Some(review) = state.reviews.get(dep)
                && (!review.stale_causes.is_empty()
                    || review.withdrawn
                    || invalidated.contains(dep))
            {
                return Err(ApiError::new(
                    ApiErrorCode::DependencyNotCurrent,
                    "prerequisite needs reconciliation",
                ));
            }
        }
    }
    let mut result = Vec::new();
    while !remaining.is_empty() {
        let next = remaining
            .iter()
            .find(|(_, t)| t.depends_on.iter().all(|d| available.contains(d)))
            .map(|(id, _)| id.clone());
        let Some(id) = next else {
            return Err(ApiError::new(
                ApiErrorCode::DependencyCycle,
                "submission dependencies contain a cycle",
            ));
        };
        let thunk = remaining.remove(&id).expect("selected existing entry");
        available.insert(id);
        result.push(thunk);
    }
    Ok(result)
}

pub fn queue(state: &GoalProjection, include_drafts: bool) -> Vec<QueueItem> {
    let mut remaining = state.current_thunks.clone();
    let mut emitted = BTreeSet::new();
    let mut result = Vec::new();
    while !remaining.is_empty() {
        let next = remaining
            .iter()
            .filter(|id| {
                state.thunks[*id]
                    .depends_on
                    .iter()
                    .all(|d| !remaining.contains(d) || emitted.contains(d))
            })
            .min_by_key(|id| {
                (
                    state.submission_sequences.get(*id).copied().unwrap_or(0),
                    (*id).clone(),
                )
            })
            .cloned();
        let Some(id) = next else {
            break;
        };
        remaining.remove(&id);
        emitted.insert(id.clone());
        let thunk = state.thunks[&id].clone();
        let review = &state.reviews[&id];
        let status = if !review.stale_causes.is_empty() {
            QueueStatus::Stale
        } else {
            match review.decision {
                Some(ReviewDecision::Approved) => QueueStatus::Approved,
                Some(ReviewDecision::ChangesRequested { .. }) => QueueStatus::ChangesRequested,
                None => QueueStatus::Pending,
            }
        };
        let blocked_by = thunk
            .depends_on
            .iter()
            .filter(|d| {
                !state.current_thunks.contains(*d)
                    || state.reviews.get(*d).is_none_or(|r| {
                        r.withdrawn
                            || !r.stale_causes.is_empty()
                            || r.decision != Some(ReviewDecision::Approved)
                    })
            })
            .cloned()
            .collect();
        let unresolved_feedback = state
            .published_batches
            .values()
            .flat_map(|b| &b.feedback)
            .filter(|f| {
                state.active_feedback.contains(&f.id)
                    && f.target.thunk_id() == Some(&id)
                    && f.kind == FeedbackKind::ChangeRequest
                    && !state.resolved_feedback.contains(&f.id)
            })
            .count() as u32;
        let unpublished_feedback = if include_drafts {
            state
                .drafts
                .values()
                .filter(|d| d.thunk_id.as_ref() == Some(&id))
                .map(|d| d.sidebar_feedback.len() as u32)
                .sum()
        } else {
            0
        };
        result.push(QueueItem {
            thunk,
            status,
            blocked_by,
            unresolved_feedback,
            unpublished_feedback,
            submission_sequence: state.submission_sequences[&id],
        });
    }
    result
}

pub fn require_current<'a>(state: &'a GoalProjection, id: &ThunkId) -> ApiResult<&'a Thunk> {
    if !state.current_thunks.contains(id) {
        return Err(ApiError::new(
            ApiErrorCode::DependencyNotCurrent,
            "thunk is not current",
        ));
    }
    state
        .thunks
        .get(id)
        .ok_or_else(|| ApiError::new(ApiErrorCode::NotFound, "thunk not found"))
}
pub fn check_approval(state: &GoalProjection, id: &ThunkId) -> ApiResult<()> {
    require_current(state, id)?;
    let item = queue(state, false)
        .into_iter()
        .find(|q| q.thunk.id == *id)
        .ok_or_else(|| invalid("thunk missing from queue"))?;
    if item.status == QueueStatus::Stale
        || !item.blocked_by.is_empty()
        || item.unresolved_feedback > 0
    {
        return Err(ApiError::new(
            ApiErrorCode::DependencyNotApproved,
            "thunk is stale, has unapproved prerequisites, or unresolved change requests",
        ));
    }
    Ok(())
}
