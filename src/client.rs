use crate::model::*;
use serde::{Serialize, de::DeserializeOwned};

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    endpoint: String,
    token: String,
}
#[derive(Debug)]
pub enum ClientError {
    Transport(reqwest::Error),
    Api(ApiError),
    InvalidEndpoint,
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => e.fmt(f),
            Self::Api(e) => e.fmt(f),
            Self::InvalidEndpoint => write!(f, "expected a loopback HTTP endpoint"),
        }
    }
}
impl std::error::Error for ClientError {}
impl Client {
    pub fn new(endpoint: &str, token: String) -> Result<Self, ClientError> {
        let url = reqwest::Url::parse(endpoint).map_err(|_| ClientError::InvalidEndpoint)?;
        if url.scheme() != "http"
            || !matches!(
                url.host_str(),
                Some("127.0.0.1" | "[::1]" | "::1" | "localhost")
            )
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ClientError::InvalidEndpoint);
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(ClientError::Transport)?;
        Ok(Self {
            http,
            endpoint: endpoint.trim_end_matches('/').into(),
            token,
        })
    }
    async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, ClientError> {
        if response.status().is_success() {
            response.json().await.map_err(ClientError::Transport)
        } else {
            Err(ClientError::Api(
                response.json().await.map_err(ClientError::Transport)?,
            ))
        }
    }
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        Self::decode(
            self.http
                .get(format!("{}/v1/{path}", self.endpoint))
                .bearer_auth(&self.token)
                .send()
                .await
                .map_err(ClientError::Transport)?,
        )
        .await
    }
    pub async fn post<I: Serialize + ?Sized, O: DeserializeOwned>(
        &self,
        path: &str,
        input: &I,
    ) -> Result<O, ClientError> {
        Self::decode(
            self.http
                .post(format!("{}/v1/{path}", self.endpoint))
                .bearer_auth(&self.token)
                .json(input)
                .send()
                .await
                .map_err(ClientError::Transport)?,
        )
        .await
    }
    pub async fn goals(&self) -> Result<Vec<GoalDetail>, ClientError> {
        self.get("goals").await
    }
    pub async fn goal(&self, id: &GoalId) -> Result<GoalDetail, ClientError> {
        self.get(&format!("goals/{id}")).await
    }
    pub async fn create_goal(
        &self,
        command: &CommandEnvelope<CreateGoal>,
    ) -> Result<CommandReceipt, ClientError> {
        self.post("goals", command).await
    }
    pub async fn execute(
        &self,
        goal: &GoalId,
        command: &CommandEnvelope<GoalCommand>,
    ) -> Result<CommandReceipt, ClientError> {
        self.post(&format!("goals/{goal}/commands"), command).await
    }
    pub async fn preview(
        &self,
        goal: &GoalId,
        request: &PreviewPublish,
    ) -> Result<PublishPreview, ClientError> {
        self.post(&format!("goals/{goal}/publish-preview"), request)
            .await
    }
    pub async fn queue(&self, goal: &GoalId) -> Result<Page<QueueItem>, ClientError> {
        self.get(&format!("goals/{goal}/queue")).await
    }
    pub async fn inbox(&self, goal: &GoalId) -> Result<Versioned<Vec<Delivery>>, ClientError> {
        self.get(&format!("goals/{goal}/feedback-batches")).await
    }
}
