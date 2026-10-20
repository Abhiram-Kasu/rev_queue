use rev_queue::{core, model::*, persistence::GitRepository, service::Service};
use std::collections::BTreeMap;
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    service: Service,
    reviewer: Actor,
    agent: Actor,
    goal: GoalId,
    empty: SourceSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(dir.path()).unwrap();
        let empty = SourceSnapshot {
            tree: GitOid::parse(repo.empty_tree().unwrap()).unwrap(),
        };
        let service = Service::open(dir.path()).unwrap();
        let reviewer = actor(ActorKind::Reviewer);
        let agent = actor(ActorKind::Agent);
        let goal = GoalId::new();
        service
            .create_goal(
                &reviewer,
                CommandEnvelope {
                    command_id: CommandId::new(),
                    expected_head: None,
                    expected_epoch: None,
                    payload: CreateGoal {
                        id: goal.clone(),
                        repository: RepositoryId::new(),
                        title: "Review backend".into(),
                        objective: "Persist review history".into(),
                        base: empty.clone(),
                    },
                },
            )
            .unwrap();
        Self {
            _dir: dir,
            service,
            reviewer,
            agent,
            goal,
            empty,
        }
    }

    fn tree(&self, base: &SourceSnapshot, path: &str, contents: &[u8]) -> SourceSnapshot {
        let repo = self.service.repository();
        let updates = BTreeMap::from([(
            path.to_owned(),
            Some(("100644".into(), repo.hash_blob(contents).unwrap())),
        )]);
        SourceSnapshot {
            tree: GitOid::parse(repo.tree_with_updates(&base.tree.0, &updates).unwrap()).unwrap(),
        }
    }

    fn command(&self, payload: GoalCommand) -> CommandEnvelope<GoalCommand> {
        let (version, _) = self.service.load(&self.goal).unwrap();
        envelope(&version, payload)
    }

    fn reviewer(&self, payload: GoalCommand) -> ApiResult<CommandReceipt> {
        self.service
            .execute(&self.reviewer, &self.goal, self.command(payload))
    }

    fn agent(&self, payload: GoalCommand) -> ApiResult<CommandReceipt> {
        self.service
            .execute(&self.agent, &self.goal, self.command(payload))
    }

    fn submit(&self, thunks: Vec<ThunkSubmission>) {
        self.agent(GoalCommand::SubmitThunks { thunks }).unwrap();
    }

    fn thunk(
        &self,
        before: &SourceSnapshot,
        after: &SourceSnapshot,
        deps: &[ThunkId],
    ) -> ThunkSubmission {
        let epoch = self.service.load(&self.goal).unwrap().0.epoch;
        ThunkSubmission {
            id: ThunkId::new(),
            based_on_epoch: epoch,
            title: "Implement change".into(),
            description: "A coherent change".into(),
            supersedes: None,
            depends_on: deps.to_vec(),
            related_to: vec![],
            provenance: vec![],
            before: before.clone(),
            after: after.clone(),
            confidence: Confidence {
                basis_points: 8500,
                rationale: "Validated locally".into(),
            },
            validation: vec![],
            responds_to: vec![],
        }
    }

    fn save(&self, draft: ReviewDraftInput, expected_edition: Option<u64>) -> CommandReceipt {
        self.reviewer(GoalCommand::SaveDraft {
            draft,
            expected_edition,
        })
        .unwrap()
    }

    fn publish(&self, request: PreviewPublish) -> CommandReceipt {
        let preview = self
            .service
            .preview(&self.reviewer, &self.goal, &request)
            .unwrap();
        self.reviewer(GoalCommand::PublishFeedback {
            preview_digest: preview.digest,
            request,
        })
        .unwrap()
    }
}

fn actor(kind: ActorKind) -> Actor {
    Actor {
        id: ActorId::new(),
        kind,
        display_name: "Test participant".into(),
    }
}

fn envelope(version: &GoalVersion, payload: GoalCommand) -> CommandEnvelope<GoalCommand> {
    CommandEnvelope {
        command_id: CommandId::new(),
        expected_head: Some(version.head.clone()),
        expected_epoch: Some(version.epoch),
        payload,
    }
}

fn draft(thunk: &ThunkSubmission, annotated: SourceSnapshot) -> ReviewDraftInput {
    ReviewDraftInput {
        id: DraftId::new(),
        thunk_id: Some(thunk.id.clone()),
        base: Some(thunk.after.clone()),
        annotated: Some(annotated),
        sidebar_feedback: vec![],
        marker_intents: vec![],
    }
}

fn request(draft: &ReviewDraftInput, roots: Vec<ThunkId>) -> PreviewPublish {
    PreviewPublish {
        drafts: vec![DraftEditionRef {
            draft_id: draft.id.clone(),
            edition: 1,
        }],
        message: "Please reconcile".into(),
        scope: ImpactScope::Selected {
            thunks: vec![draft.thunk_id.clone().unwrap()],
        },
        invalidation_roots: roots,
    }
}

#[test]
fn dependency_batches_are_ordered_approvals_are_exact_and_replacements_stale_dependents() {
    let f = Fixture::new();
    let a_tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let b_tree = f.tree(&a_tree, "b.rs", b"fn b() {}\n");
    let a = f.thunk(&f.empty, &a_tree, &[]);
    let b = f.thunk(&a_tree, &b_tree, std::slice::from_ref(&a.id));
    f.submit(vec![b.clone(), a.clone()]);
    let (_, state) = f.service.load(&f.goal).unwrap();
    assert_eq!(
        core::queue(&state, true)
            .iter()
            .map(|item| item.thunk.id.clone())
            .collect::<Vec<_>>(),
        vec![a.id.clone(), b.id.clone()]
    );
    assert_eq!(
        f.reviewer(GoalCommand::RecordDecision {
            thunk_id: b.id.clone(),
            decision: ReviewDecision::Approved
        })
        .unwrap_err()
        .code,
        ApiErrorCode::DependencyNotApproved
    );
    for id in [&a.id, &b.id] {
        f.reviewer(GoalCommand::RecordDecision {
            thunk_id: id.clone(),
            decision: ReviewDecision::Approved,
        })
        .unwrap();
    }
    let mut replacement = f.thunk(&f.empty, &a_tree, &[]);
    replacement.supersedes = Some(a.id.clone());
    f.submit(vec![replacement.clone()]);
    let (_, state) = f.service.load(&f.goal).unwrap();
    assert_eq!(state.thunks[&replacement.id].iteration, 2);
    assert!(!state.current_thunks.contains(&a.id));
    assert!(state.current_thunks.contains(&replacement.id));
    assert_eq!(state.reviews[&replacement.id].decision, None);
    assert_eq!(
        state.reviews[&b.id].decision,
        Some(ReviewDecision::Approved)
    );
    assert!(!state.reviews[&b.id].stale_causes.is_empty());
    assert_eq!(f.service.repair(&f.goal).unwrap(), state);
}

#[test]
fn saved_drafts_survive_restart_remain_private_and_enforce_edition_ownership() {
    let f = Fixture::new();
    let tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let a = f.thunk(&f.empty, &tree, &[]);
    f.submit(vec![a.clone()]);
    let annotated = f.tree(&tree, "a.rs", b"// #! Make public\nfn a() {}\n");
    let input = draft(&a, annotated.clone());
    f.save(input.clone(), None);
    let private_blob = GitOid::parse(
        f.service
            .repository()
            .hash_blob(b"// #! Make public\nfn a() {}\n")
            .unwrap(),
    )
    .unwrap();
    assert!(f.service.blob(&f.reviewer, &f.goal, &private_blob).is_ok());
    assert_eq!(
        f.service
            .blob(&f.agent, &f.goal, &private_blob)
            .unwrap_err()
            .code,
        ApiErrorCode::NotFound
    );
    let other = actor(ActorKind::Reviewer);
    assert_eq!(
        f.service
            .execute(
                &other,
                &f.goal,
                f.command(GoalCommand::SaveDraft {
                    draft: input.clone(),
                    expected_edition: Some(1)
                })
            )
            .unwrap_err()
            .code,
        ApiErrorCode::Unauthorized
    );
    assert_eq!(
        f.reviewer(GoalCommand::SaveDraft {
            draft: input.clone(),
            expected_edition: None
        })
        .unwrap_err()
        .code,
        ApiErrorCode::DraftConflict
    );
    assert_eq!(
        f.service.detail(&f.agent, &f.goal).unwrap().counts.drafts,
        0
    );
    let history = f.service.history(&f.agent, &f.goal, None).unwrap();
    assert!(
        !history
            .data
            .iter()
            .flat_map(|tx| &tx.events)
            .any(|event| matches!(event.payload, Event::DraftSaved { .. }))
    );
    let path = f._dir.path().to_path_buf();
    assert!(Service::open(&path).is_err());
    drop(f.service);
    let service = Service::open(path).unwrap();
    let (_, state) = service.load(&f.goal).unwrap();
    assert_eq!(state.drafts[&input.id].edition, 1);
    assert_eq!(state.drafts[&input.id].annotated, Some(annotated));
    assert_eq!(service.repair(&f.goal).unwrap(), state);
}

#[test]
fn inline_preview_is_stable_publish_is_atomic_and_epoch_invalidates_old_submissions() {
    let f = Fixture::new();
    let a_tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let b_tree = f.tree(&a_tree, "b.rs", b"fn b() {}\n");
    let c_tree = f.tree(&b_tree, "c.rs", b"fn c() {}\n");
    let d_tree = f.tree(&f.empty, "d.rs", b"fn d() {}\n");
    let a = f.thunk(&f.empty, &a_tree, &[]);
    let b = f.thunk(&a_tree, &b_tree, std::slice::from_ref(&a.id));
    let c = f.thunk(&b_tree, &c_tree, std::slice::from_ref(&b.id));
    let d = f.thunk(&f.empty, &d_tree, &[]);
    f.submit(vec![a.clone(), b.clone(), c.clone(), d.clone()]);
    let old_epoch_submission = f.thunk(&f.empty, &d_tree, &[]);
    let annotated = f.tree(&a_tree, "a.rs", b"// #! Make public\nfn a() {}\n");
    let input = draft(&a, annotated);
    f.save(input.clone(), None);
    let req = request(&input, vec![a.id.clone()]);
    let preview = f.service.preview(&f.reviewer, &f.goal, &req).unwrap();
    assert_eq!(
        preview,
        f.service.preview(&f.reviewer, &f.goal, &req).unwrap()
    );
    assert_eq!(preview.feedback.len(), 1);
    assert!(preview.edits.is_empty());
    assert_eq!(preview.feedback[0].body, "Make public");
    let affected: std::collections::BTreeSet<_> = preview.affected_thunks.iter().cloned().collect();
    assert_eq!(
        affected,
        [a.id.clone(), b.id.clone(), c.id.clone()]
            .into_iter()
            .collect()
    );
    let command = f.command(GoalCommand::PublishFeedback {
        preview_digest: preview.digest,
        request: req,
    });
    let receipt = f
        .service
        .execute(&f.reviewer, &f.goal, command.clone())
        .unwrap();
    assert_eq!(
        f.service.execute(&f.reviewer, &f.goal, command).unwrap(),
        receipt
    );
    let (_, state) = f.service.load(&f.goal).unwrap();
    assert_eq!(state.epoch, 1);
    assert!(state.drafts.is_empty());
    assert!(state.reviews[&d.id].stale_causes.is_empty());
    assert_eq!(state.published_batches.len(), 1);
    assert_eq!(
        f.agent(GoalCommand::SubmitThunks {
            thunks: vec![old_epoch_submission]
        })
        .unwrap_err()
        .code,
        ApiErrorCode::EpochConflict
    );
    assert_eq!(f.service.repair(&f.goal).unwrap(), state);
}

#[test]
fn changed_draft_requires_fresh_preview_and_real_edits_require_invalidation() {
    let f = Fixture::new();
    let tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let a = f.thunk(&f.empty, &tree, &[]);
    f.submit(vec![a.clone()]);
    let annotated = f.tree(&tree, "a.rs", b"// #! Rename this too\npub fn a() {}\n");
    let input = draft(&a, annotated);
    f.save(input.clone(), None);
    assert_eq!(
        f.service
            .preview(&f.reviewer, &f.goal, &request(&input, vec![]))
            .unwrap_err()
            .code,
        ApiErrorCode::InvalidInput
    );
    let mut req = request(&input, vec![a.id.clone()]);
    let old_preview = f.service.preview(&f.reviewer, &f.goal, &req).unwrap();
    assert_eq!(old_preview.edits.len(), 1);
    assert_eq!(
        f.service
            .repository()
            .read_file(&old_preview.edits[0].edited.tree.0, "a.rs")
            .unwrap(),
        b"pub fn a() {}\n"
    );
    f.save(input.clone(), Some(1));
    assert_eq!(
        f.service
            .preview(&f.reviewer, &f.goal, &req)
            .unwrap_err()
            .code,
        ApiErrorCode::DraftConflict
    );
    req.drafts[0].edition = 2;
    assert_eq!(
        f.reviewer(GoalCommand::PublishFeedback {
            preview_digest: old_preview.digest,
            request: req.clone()
        })
        .unwrap_err()
        .code,
        ApiErrorCode::PreviewExpired
    );
    f.publish(req);
    let (_, state) = f.service.load(&f.goal).unwrap();
    let edit = &state.published_batches.values().next().unwrap().edits[0];
    assert!(
        f.service
            .blob(&f.agent, &f.goal, &edit.source_map_blob)
            .is_ok()
    );
    assert_eq!(f.service.repair(&f.goal).unwrap(), state);
}

#[test]
fn delivery_acknowledgment_does_not_resolve_feedback_and_is_per_agent() {
    let f = Fixture::new();
    let tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let a = f.thunk(&f.empty, &tree, &[]);
    f.submit(vec![a.clone()]);
    let annotated = f.tree(&tree, "a.rs", b"// #! Explain this\nfn a() {}\n");
    let input = draft(&a, annotated);
    f.save(input.clone(), None);
    f.publish(request(&input, vec![]));
    let inbox = f.service.inbox(&f.agent, &f.goal).unwrap();
    assert_eq!(inbox.data.len(), 1);
    let DeliveryPayload::Feedback { batch } = &inbox.data[0].payload else {
        panic!("expected feedback")
    };
    let feedback_id = batch.feedback[0].id.clone();
    f.agent(GoalCommand::AcknowledgeBatch {
        batch_id: batch.id.clone(),
    })
    .unwrap();
    assert!(f.service.inbox(&f.agent, &f.goal).unwrap().data.is_empty());
    assert_eq!(
        f.service
            .inbox(&actor(ActorKind::Agent), &f.goal)
            .unwrap()
            .data
            .len(),
        1
    );
    let (_, state) = f.service.load(&f.goal).unwrap();
    assert!(!state.resolved_feedback.contains(&feedback_id));
    assert_eq!(
        f.agent(GoalCommand::ResolveFeedback {
            feedback_id: feedback_id.clone(),
            resolution: "done".into()
        })
        .unwrap_err()
        .code,
        ApiErrorCode::Unauthorized
    );
    f.reviewer(GoalCommand::ResolveFeedback {
        feedback_id: feedback_id.clone(),
        resolution: "Verified".into(),
    })
    .unwrap();
    assert!(
        f.service
            .load(&f.goal)
            .unwrap()
            .1
            .resolved_feedback
            .contains(&feedback_id)
    );
}

#[test]
fn restore_appends_history_preserves_dedup_and_delivers_control() {
    let f = Fixture::new();
    let tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let a = f.thunk(&f.empty, &tree, &[]);
    let submitted = f.command(GoalCommand::SubmitThunks {
        thunks: vec![a.clone()],
    });
    let submitted_receipt = f
        .service
        .execute(&f.agent, &f.goal, submitted.clone())
        .unwrap();
    f.reviewer(GoalCommand::RecordDecision {
        thunk_id: a.id.clone(),
        decision: ReviewDecision::Approved,
    })
    .unwrap();
    let checkpoint = CheckpointId::new();
    f.reviewer(GoalCommand::CreateCheckpoint {
        id: checkpoint.clone(),
        name: "Approved A".into(),
        source: tree.clone(),
    })
    .unwrap();
    let mut replacement = f.thunk(&f.empty, &tree, &[]);
    replacement.supersedes = Some(a.id.clone());
    f.submit(vec![replacement.clone()]);
    let before = f.service.load(&f.goal).unwrap().0;
    f.reviewer(GoalCommand::RestoreCheckpoint {
        checkpoint_id: checkpoint,
        reason: "Return to approved implementation".into(),
    })
    .unwrap();
    let (after, state) = f.service.load(&f.goal).unwrap();
    assert_eq!(after.sequence, before.sequence + 1);
    assert_eq!(after.epoch, before.epoch + 1);
    assert_eq!(state.current_thunks, [a.id.clone()].into_iter().collect());
    assert_eq!(
        state.reviews[&a.id].decision,
        Some(ReviewDecision::Approved)
    );
    assert!(state.thunks.contains_key(&replacement.id));
    assert_eq!(
        f.service
            .execute(&f.agent, &f.goal, submitted.clone())
            .unwrap(),
        submitted_receipt
    );
    let mut changed_retry = submitted;
    if let GoalCommand::SubmitThunks { thunks } = &mut changed_retry.payload {
        thunks[0].title = "Different content".into();
    }
    assert_eq!(
        f.service
            .execute(&f.agent, &f.goal, changed_retry)
            .unwrap_err()
            .code,
        ApiErrorCode::IdempotencyConflict
    );
    let inbox = f.service.inbox(&f.agent, &f.goal).unwrap();
    let restored = inbox
        .data
        .iter()
        .find(|item| matches!(item.payload, DeliveryPayload::Restore { .. }))
        .unwrap();
    f.agent(GoalCommand::AcknowledgeControl {
        event_id: restored.event_id.clone(),
    })
    .unwrap();
    assert!(f.service.inbox(&f.agent, &f.goal).unwrap().data.is_empty());
    assert_eq!(
        f.service.at(&f.goal, &before.head).unwrap().current_thunks,
        [replacement.id].into_iter().collect()
    );
    assert_eq!(
        f.service.repair(&f.goal).unwrap(),
        f.service.load(&f.goal).unwrap().1
    );
}

#[test]
fn commands_enforce_roles_head_epoch_and_visible_goal_objects() {
    let f = Fixture::new();
    let tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let a = f.thunk(&f.empty, &tree, &[]);
    assert_eq!(
        f.reviewer(GoalCommand::SubmitThunks {
            thunks: vec![a.clone()]
        })
        .unwrap_err()
        .code,
        ApiErrorCode::Unauthorized
    );
    let old = f.command(GoalCommand::CloseGoal {
        reason: "Done".into(),
    });
    f.submit(vec![a.clone()]);
    assert_eq!(
        f.service
            .execute(&f.reviewer, &f.goal, old)
            .unwrap_err()
            .code,
        ApiErrorCode::VersionConflict
    );
    let mut wrong_epoch = f.command(GoalCommand::CloseGoal {
        reason: "Done".into(),
    });
    wrong_epoch.expected_epoch = Some(999);
    assert_eq!(
        f.service
            .execute(&f.reviewer, &f.goal, wrong_epoch)
            .unwrap_err()
            .code,
        ApiErrorCode::EpochConflict
    );
    assert_eq!(
        f.agent(GoalCommand::RecordDecision {
            thunk_id: a.id,
            decision: ReviewDecision::Approved
        })
        .unwrap_err()
        .code,
        ApiErrorCode::Unauthorized
    );
    let unrelated = GitOid::parse(
        f.service
            .repository()
            .hash_blob(b"unrelated secret")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        f.service
            .blob(&f.reviewer, &f.goal, &unrelated)
            .unwrap_err()
            .code,
        ApiErrorCode::NotFound
    );
    assert_eq!(
        f.service.inbox(&f.reviewer, &f.goal).unwrap_err().code,
        ApiErrorCode::Unauthorized
    );
}

#[test]
fn invalid_dependency_batches_commit_nothing() {
    let f = Fixture::new();
    let tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let a = f.thunk(&f.empty, &tree, &[]);
    let invalid = f.thunk(&f.empty, &tree, std::slice::from_ref(&a.id));
    let before = f.service.load(&f.goal).unwrap();
    assert_eq!(
        f.agent(GoalCommand::SubmitThunks {
            thunks: vec![a.clone(), invalid]
        })
        .unwrap_err()
        .code,
        ApiErrorCode::CompositionConflict
    );
    assert_eq!(f.service.load(&f.goal).unwrap(), before);
    let mut x = a.clone();
    let mut y = f.thunk(&f.empty, &tree, std::slice::from_ref(&x.id));
    x.depends_on = vec![y.id.clone()];
    assert_eq!(
        f.agent(GoalCommand::SubmitThunks {
            thunks: vec![x, y.clone()]
        })
        .unwrap_err()
        .code,
        ApiErrorCode::DependencyCycle
    );
    assert_eq!(f.service.load(&f.goal).unwrap(), before);
    y.depends_on = vec![ThunkId::new()];
    assert_eq!(
        f.agent(GoalCommand::SubmitThunks { thunks: vec![y] })
            .unwrap_err()
            .code,
        ApiErrorCode::DependencyNotCurrent
    );
    assert_eq!(f.service.load(&f.goal).unwrap(), before);
}

#[test]
fn conflicting_draft_edits_block_publish_until_consolidated() {
    let f = Fixture::new();
    let tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let a = f.thunk(&f.empty, &tree, &[]);
    f.submit(vec![a.clone()]);
    let first = draft(&a, f.tree(&tree, "a.rs", b"pub fn a() {}\n"));
    let second = draft(&a, f.tree(&tree, "a.rs", b"fn renamed() {}\n"));
    f.save(first.clone(), None);
    f.save(second.clone(), None);
    let mut req = request(&first, vec![a.id.clone()]);
    req.drafts.push(DraftEditionRef {
        draft_id: second.id.clone(),
        edition: 1,
    });
    let before = f.service.load(&f.goal).unwrap();
    assert_eq!(
        f.service
            .preview(&f.reviewer, &f.goal, &req)
            .unwrap_err()
            .code,
        ApiErrorCode::CompositionConflict
    );
    assert_eq!(f.service.load(&f.goal).unwrap(), before);
    f.reviewer(GoalCommand::DiscardDraft {
        draft_id: second.id,
    })
    .unwrap();
    req.drafts.pop();
    f.publish(req);
    assert_eq!(
        f.service.load(&f.goal).unwrap().1.published_batches.len(),
        1
    );
}

#[test]
fn restore_recovers_feedback_actionability_without_erasing_audit_or_acknowledgments() {
    let f = Fixture::new();
    let tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let a = f.thunk(&f.empty, &tree, &[]);
    f.submit(vec![a.clone()]);
    let early = draft(
        &a,
        f.tree(&tree, "a.rs", b"// #! Early request\nfn a() {}\n"),
    );
    f.save(early.clone(), None);
    f.publish(request(&early, vec![]));
    let early_batch = f
        .service
        .load(&f.goal)
        .unwrap()
        .1
        .published_batches
        .values()
        .next()
        .unwrap()
        .clone();
    f.agent(GoalCommand::AcknowledgeBatch {
        batch_id: early_batch.id.clone(),
    })
    .unwrap();
    let checkpoint = CheckpointId::new();
    f.reviewer(GoalCommand::CreateCheckpoint {
        id: checkpoint.clone(),
        name: "Before later feedback".into(),
        source: tree.clone(),
    })
    .unwrap();
    f.reviewer(GoalCommand::ResolveFeedback {
        feedback_id: early_batch.feedback[0].id.clone(),
        resolution: "Resolved after checkpoint".into(),
    })
    .unwrap();
    let later = draft(
        &a,
        f.tree(&tree, "a.rs", b"// #! Later request\nfn a() {}\n"),
    );
    f.save(later.clone(), None);
    f.publish(request(&later, vec![]));
    let (_, before) = f.service.load(&f.goal).unwrap();
    let later_batch = before
        .published_batches
        .values()
        .find(|batch| batch.id != early_batch.id)
        .unwrap()
        .clone();
    f.agent(GoalCommand::AcknowledgeBatch {
        batch_id: later_batch.id.clone(),
    })
    .unwrap();
    f.reviewer(GoalCommand::RestoreCheckpoint {
        checkpoint_id: checkpoint,
        reason: "Abandon the later review round".into(),
    })
    .unwrap();
    let (_, restored) = f.service.load(&f.goal).unwrap();
    let expected = [early_batch.feedback[0].id.clone()].into_iter().collect();
    assert_eq!(restored.active_feedback, expected);
    assert_eq!(
        f.service
            .detail(&f.reviewer, &f.goal)
            .unwrap()
            .actionable_feedback,
        expected
    );
    assert!(
        !restored
            .resolved_feedback
            .contains(&early_batch.feedback[0].id)
    );
    assert_eq!(core::queue(&restored, true)[0].unresolved_feedback, 1);
    assert_eq!(restored.published_batches.len(), 2);
    for batch in [&early_batch, &later_batch] {
        assert!(
            restored
                .acknowledgments
                .contains(&(batch.id.clone(), f.agent.id.clone()))
        );
    }
    assert!(
        f.service
            .inbox(&f.agent, &f.goal)
            .unwrap()
            .data
            .iter()
            .all(|delivery| !matches!(delivery.payload, DeliveryPayload::Feedback { .. }))
    );
    assert!(
        f.service
            .history(&f.reviewer, &f.goal, None)
            .unwrap()
            .data
            .iter()
            .flat_map(|tx| &tx.events)
            .filter(|event| matches!(event.payload, Event::FeedbackPublished { .. }))
            .count()
            == 2
    );
    assert_eq!(f.service.repair(&f.goal).unwrap(), restored);
}

#[test]
fn marker_only_publication_retains_original_source_and_maps_after_gc_restart() {
    let f = Fixture::new();
    let tree = f.tree(&f.empty, "a.rs", b"fn a() {}\r\n");
    let a = f.thunk(&f.empty, &tree, &[]);
    f.submit(vec![a.clone()]);
    let annotated_bytes = b"// #! Keep this instruction in history\r\nfn a() {}\r\n";
    let annotated = f.tree(&tree, "a.rs", annotated_bytes);
    let input = draft(&a, annotated.clone());
    f.save(input.clone(), None);
    f.publish(request(&input, vec![]));
    let (_, state) = f.service.load(&f.goal).unwrap();
    let batch = state.published_batches.values().next().unwrap();
    assert!(batch.edits.is_empty());
    assert_eq!(batch.extractions.len(), 1);
    let extraction = batch.extractions[0].clone();
    assert_eq!(extraction.annotated, annotated);
    assert_eq!(extraction.cleaned, tree);
    assert_eq!(extraction.draft.draft_id, input.id);
    let map_bytes = f
        .service
        .blob(&f.agent, &f.goal, &extraction.source_map_blob)
        .unwrap();
    let maps: BTreeMap<String, rev_queue::review::SourceMap> =
        serde_json::from_slice(&map_bytes).unwrap();
    assert!(maps["a.rs"].segments.iter().any(|segment| segment.removed));
    let path = f._dir.path().to_path_buf();
    drop(f.service);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["gc", "--prune=now"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let service = Service::open(path).unwrap();
    assert_eq!(
        service
            .repository()
            .read_file(&extraction.annotated.tree.0, "a.rs")
            .unwrap(),
        annotated_bytes
    );
    assert_eq!(
        service
            .blob(&f.agent, &f.goal, &extraction.source_map_blob)
            .unwrap(),
        map_bytes
    );
    assert_eq!(service.repair(&f.goal).unwrap(), state);
}

#[test]
fn consumed_and_discarded_draft_ids_cannot_be_reused() {
    let f = Fixture::new();
    let tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let a = f.thunk(&f.empty, &tree, &[]);
    f.submit(vec![a.clone()]);
    let consumed = draft(&a, f.tree(&tree, "a.rs", b"// #! Review this\nfn a() {}\n"));
    f.save(consumed.clone(), None);
    f.publish(request(&consumed, vec![]));
    let discarded = draft(&a, tree);
    f.save(discarded.clone(), None);
    f.reviewer(GoalCommand::DiscardDraft {
        draft_id: discarded.id.clone(),
    })
    .unwrap();
    let before = f.service.load(&f.goal).unwrap();
    for input in [consumed, discarded] {
        for edition in [None, Some(1)] {
            assert_eq!(
                f.reviewer(GoalCommand::SaveDraft {
                    draft: input.clone(),
                    expected_edition: edition
                })
                .unwrap_err()
                .code,
                ApiErrorCode::InvalidInput
            );
            assert_eq!(f.service.load(&f.goal).unwrap(), before);
        }
        assert_eq!(before.1.draft_editions[&input.id], 1);
    }
    assert!(before.1.drafts.is_empty());
}

#[test]
fn replacement_batch_cannot_add_work_using_a_transitively_invalidated_prerequisite() {
    let f = Fixture::new();
    let a_tree = f.tree(&f.empty, "a.rs", b"fn a() {}\n");
    let b_tree = f.tree(&a_tree, "b.rs", b"fn b() {}\n");
    let c_tree = f.tree(&b_tree, "c.rs", b"fn c() {}\n");
    let a = f.thunk(&f.empty, &a_tree, &[]);
    let b = f.thunk(&a_tree, &b_tree, std::slice::from_ref(&a.id));
    f.submit(vec![a.clone(), b.clone()]);
    let mut replacement = f.thunk(&f.empty, &a_tree, &[]);
    replacement.supersedes = Some(a.id.clone());
    let c = f.thunk(&b_tree, &c_tree, std::slice::from_ref(&b.id));
    let before = f.service.load(&f.goal).unwrap();
    for batch in [vec![replacement.clone(), c.clone()], vec![c, replacement]] {
        assert_eq!(
            f.agent(GoalCommand::SubmitThunks { thunks: batch })
                .unwrap_err()
                .code,
            ApiErrorCode::DependencyNotCurrent
        );
        assert_eq!(f.service.load(&f.goal).unwrap(), before);
    }
    assert!(before.1.current_thunks.contains(&a.id));
    assert!(before.1.reviews[&b.id].stale_causes.is_empty());
}
