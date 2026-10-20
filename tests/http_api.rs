use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use rev_queue::{
    api::{self, ApiState},
    model::*,
    persistence::GitRepository,
    service::Service,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{collections::BTreeMap, sync::Arc};
use tempfile::TempDir;
use tower::ServiceExt;

struct Fixture {
    _dir: TempDir,
    service: Service,
    app: Router,
    goal: GoalId,
    empty: SourceSnapshot,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(dir.path()).unwrap();
        let empty = SourceSnapshot {
            tree: GitOid::parse(repo.empty_tree().unwrap()).unwrap(),
        };
        let service = Service::open(dir.path()).unwrap();
        let reviewer = Actor {
            id: ActorId::new(),
            kind: ActorKind::Reviewer,
            display_name: "Reviewer".into(),
        };
        let agent = Actor {
            id: ActorId::new(),
            kind: ActorKind::Agent,
            display_name: "Agent".into(),
        };
        let app = api::router(ApiState {
            service: service.clone(),
            capabilities: Arc::new(BTreeMap::from([
                ("reviewer".into(), reviewer.clone()),
                ("agent".into(), agent.clone()),
            ])),
        });
        let f = Self {
            _dir: dir,
            service,
            app,
            goal: GoalId::new(),
            empty,
        };
        let command = CommandEnvelope {
            command_id: CommandId::new(),
            expected_head: None,
            expected_epoch: None,
            payload: CreateGoal {
                id: f.goal.clone(),
                repository: RepositoryId::new(),
                title: "HTTP goal".into(),
                objective: "Review through HTTP".into(),
                base: f.empty.clone(),
            },
        };
        let (status, _) = f.json("POST", "/v1/goals", "reviewer", &command).await;
        assert_eq!(status, StatusCode::OK);
        f
    }
    fn path(&self, suffix: &str) -> String {
        format!("/v1/goals/{}{suffix}", self.goal)
    }
    async fn raw(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, bytes)
    }
    async fn json(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: &impl Serialize,
    ) -> (StatusCode, Vec<u8>) {
        self.raw(method, path, Some(token), serde_json::to_vec(body).unwrap())
            .await
    }
    async fn get<T: DeserializeOwned>(&self, path: &str, token: &str) -> T {
        let (status, bytes) = self.raw("GET", path, Some(token), vec![]).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        serde_json::from_slice(&bytes).unwrap()
    }
    fn command<T>(&self, payload: T) -> CommandEnvelope<T> {
        let (version, _) = self.service.load(&self.goal).unwrap();
        CommandEnvelope {
            command_id: CommandId::new(),
            expected_head: Some(version.head),
            expected_epoch: Some(version.epoch),
            payload,
        }
    }
    fn thunk(&self, name: &str) -> ThunkSubmission {
        let repo = self.service.repository();
        let blob = repo
            .hash_blob(format!("fn {name}() {{}}\n").as_bytes())
            .unwrap();
        let updates = BTreeMap::from([(format!("{name}.rs"), Some(("100644".into(), blob)))]);
        let after = SourceSnapshot {
            tree: GitOid::parse(
                repo.tree_with_updates(&self.empty.tree.0, &updates)
                    .unwrap(),
            )
            .unwrap(),
        };
        ThunkSubmission {
            id: ThunkId::new(),
            based_on_epoch: 0,
            title: name.into(),
            description: "Test HTTP".into(),
            supersedes: None,
            depends_on: vec![],
            related_to: vec![],
            provenance: vec![],
            before: self.empty.clone(),
            after,
            confidence: Confidence {
                basis_points: 9000,
                rationale: "Checked".into(),
            },
            validation: vec![],
            responds_to: vec![],
        }
    }
    async fn submit(&self, thunks: Vec<ThunkSubmission>) -> CommandReceipt {
        let (status, bytes) = self
            .json(
                "POST",
                &self.path("/thunks"),
                "agent",
                &self.command(api::SubmitThunks { thunks }),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        serde_json::from_slice(&bytes).unwrap()
    }
}

fn error(bytes: &[u8]) -> ApiError {
    serde_json::from_slice(bytes).unwrap()
}
fn cursor_url(path: &str, cursor: &str) -> String {
    let mut url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
    url.query_pairs_mut()
        .append_pair("limit", "1")
        .append_pair("cursor", cursor);
    format!("{}?{}", url.path(), url.query().unwrap())
}

#[tokio::test]
async fn auth_precedes_body_parsing_and_roles_are_enforced() {
    let f = Fixture::new().await;
    for token in [None, Some("unknown")] {
        let (status, bytes) = f
            .raw("POST", "/v1/goals", token, b"malformed".to_vec())
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(error(&bytes).code, ApiErrorCode::Unauthorized);
    }
    let command = f.command(api::SubmitThunks {
        thunks: vec![f.thunk("a")],
    });
    let (status, bytes) = f
        .json("POST", &f.path("/thunks"), "reviewer", &command)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error(&bytes).code, ApiErrorCode::Unauthorized);
    let (status, bytes) = f
        .raw("GET", &f.path("/drafts"), Some("agent"), vec![])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error(&bytes).code, ApiErrorCode::Unauthorized);
    let create = CommandEnvelope {
        command_id: CommandId::new(),
        expected_head: None,
        expected_epoch: None,
        payload: CreateGoal {
            id: GoalId::new(),
            repository: RepositoryId::new(),
            title: "Agent goal".into(),
            objective: "Must be denied".into(),
            base: f.empty.clone(),
        },
    };
    let (status, bytes) = f.json("POST", "/v1/goals", "agent", &create).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error(&bytes).code, ApiErrorCode::Unauthorized);
}

#[tokio::test]
async fn malformed_json_ids_paths_and_query_parameters_have_typed_errors() {
    let f = Fixture::new().await;
    let cases = [
        ("POST", "/v1/goals".to_owned(), b"{".to_vec()),
        (
            "POST",
            "/v1/goals".to_owned(),
            br#"{"command_id":"not-a-uuid"}"#.to_vec(),
        ),
        ("GET", "/v1/goals/not-a-uuid".to_owned(), vec![]),
        ("GET", f.path("/blobs/not-an-oid"), vec![]),
        ("GET", f.path("/queue?limit=bad"), vec![]),
        ("GET", f.path("/queue?head=bad"), vec![]),
        (
            "POST",
            f.path("/source/capture"),
            serde_json::to_vec(
                &serde_json::json!({"base":f.empty,"worktree":"/tmp","paths":["../escape"]}),
            )
            .unwrap(),
        ),
    ];
    for (method, path, body) in cases {
        let (status, bytes) = f.raw(method, &path, Some("reviewer"), body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}: {}",
            String::from_utf8_lossy(&bytes)
        );
        assert_eq!(error(&bytes).code, ApiErrorCode::InvalidInput);
    }
}

#[tokio::test]
async fn named_and_generic_commands_share_idempotency_and_typed_conflicts() {
    let f = Fixture::new().await;
    let thunk = f.thunk("a");
    let named = f.command(api::SubmitThunks {
        thunks: vec![thunk.clone()],
    });
    let stale = f.command(GoalCommand::CloseGoal {
        reason: "Stale head".into(),
    });
    let (status, bytes) = f.json("POST", &f.path("/thunks"), "agent", &named).await;
    assert_eq!(status, StatusCode::OK);
    let first: CommandReceipt = serde_json::from_slice(&bytes).unwrap();
    let generic = CommandEnvelope {
        command_id: named.command_id,
        expected_head: named.expected_head,
        expected_epoch: named.expected_epoch,
        payload: GoalCommand::SubmitThunks {
            thunks: vec![thunk.clone()],
        },
    };
    let (status, bytes) = f
        .json("POST", &f.path("/commands"), "agent", &generic)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<CommandReceipt>(&bytes).unwrap(),
        first
    );
    let (status, bytes) = f
        .json("POST", &f.path("/commands"), "reviewer", &stale)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let error = error(&bytes);
    assert_eq!(error.code, ApiErrorCode::VersionConflict);
    assert_eq!(error.current_version, Some(first.version));
    let detail: ThunkDetail = f
        .get(&f.path(&format!("/thunks/{}", thunk.id)), "reviewer")
        .await;
    assert_eq!(detail.files.len(), 1);
    assert_eq!(detail.files[0].kind, FileChangeKind::Added);
    let diff: Versioned<Vec<FileDiff>> = f
        .get(&f.path(&format!("/thunks/{}/diff", thunk.id)), "agent")
        .await;
    assert_eq!(diff.data, detail.files);
}

#[tokio::test]
async fn queue_and_history_cursors_pin_the_original_head_across_mutations() {
    let f = Fixture::new().await;
    f.submit(vec![f.thunk("a"), f.thunk("b")]).await;
    let queue: Page<QueueItem> = f.get(&f.path("/queue?limit=1"), "reviewer").await;
    let history: Page<Transaction> = f.get(&f.path("/history?limit=1"), "reviewer").await;
    assert!(queue.next_cursor.is_some());
    assert!(history.next_cursor.is_some());
    let original_head = queue.version.head.clone();
    f.submit(vec![f.thunk("c")]).await;
    let second_queue: Page<QueueItem> = f
        .get(
            &cursor_url(&f.path("/queue"), queue.next_cursor.as_ref().unwrap()),
            "reviewer",
        )
        .await;
    assert_eq!(second_queue.version.head, original_head);
    assert_eq!(second_queue.items.len(), 1);
    assert_ne!(queue.items[0].thunk.id, second_queue.items[0].thunk.id);
    assert!(second_queue.next_cursor.is_none());
    let second_history: Page<Transaction> = f
        .get(
            &cursor_url(&f.path("/history"), history.next_cursor.as_ref().unwrap()),
            "reviewer",
        )
        .await;
    assert_eq!(second_history.version, history.version);
    assert_eq!(second_history.items[0].sequence, 1);
    assert!(second_history.next_cursor.is_none());
    let fresh: Page<QueueItem> = f.get(&f.path("/queue"), "agent").await;
    assert_eq!(fresh.items.len(), 3);
    assert_ne!(fresh.version.head, original_head);
    let pinned: Page<QueueItem> = f
        .get(
            &f.path(&format!("/queue?head={}", original_head.0)),
            "reviewer",
        )
        .await;
    assert_eq!(pinned.items.len(), 2);
    let (status, bytes) = f
        .raw(
            "GET",
            &f.path("/queue?cursor=bad"),
            Some("reviewer"),
            vec![],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error(&bytes).code, ApiErrorCode::InvalidInput);
}

#[tokio::test]
async fn drafts_history_and_blobs_respect_visibility() {
    let f = Fixture::new().await;
    let thunk = f.thunk("a");
    f.submit(vec![thunk.clone()]).await;
    let repo = f.service.repository();
    let private_bytes = b"// #! PRIVATE_DRAFT_INSTRUCTION\nfn a() {}\n";
    let private_blob = GitOid::parse(repo.hash_blob(private_bytes).unwrap()).unwrap();
    let updates = BTreeMap::from([(
        "a.rs".to_owned(),
        Some(("100644".into(), private_blob.0.clone())),
    )]);
    let annotated = SourceSnapshot {
        tree: GitOid::parse(
            repo.tree_with_updates(&thunk.after.tree.0, &updates)
                .unwrap(),
        )
        .unwrap(),
    };
    let draft = ReviewDraftInput {
        id: DraftId::new(),
        thunk_id: Some(thunk.id.clone()),
        base: Some(thunk.after.clone()),
        annotated: Some(annotated),
        sidebar_feedback: vec![],
        marker_intents: vec![],
    };
    let command = f.command(api::SaveDraftInput {
        draft: draft.clone(),
        expected_edition: None,
    });
    let (status, bytes) = f
        .json(
            "PUT",
            &f.path(&format!("/drafts/{}", draft.id)),
            "reviewer",
            &command,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let drafts: Versioned<Vec<ReviewDraft>> = f.get(&f.path("/drafts"), "reviewer").await;
    assert_eq!(drafts.data.len(), 1);
    let history: Page<Transaction> = f.get(&f.path("/history"), "agent").await;
    assert!(
        !history
            .items
            .iter()
            .flat_map(|tx| &tx.events)
            .any(|event| matches!(event.payload, Event::DraftSaved { .. }))
    );
    let private_path = f.path(&format!("/blobs/{}", private_blob.0));
    let (status, bytes) = f.raw("GET", &private_path, Some("agent"), vec![]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error(&bytes).code, ApiErrorCode::NotFound);
    let (status, bytes) = f.raw("GET", &private_path, Some("reviewer"), vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, private_bytes);
    let public_blob = GitOid::parse(repo.hash_blob(b"fn a() {}\n").unwrap()).unwrap();
    let (status, bytes) = f
        .raw(
            "GET",
            &f.path(&format!("/blobs/{}", public_blob.0)),
            Some("agent"),
            vec![],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"fn a() {}\n");
    let unrelated = repo.hash_blob(b"not owned by goal").unwrap();
    let (status, _) = f
        .raw(
            "GET",
            &f.path(&format!("/blobs/{unrelated}")),
            Some("reviewer"),
            vec![],
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
