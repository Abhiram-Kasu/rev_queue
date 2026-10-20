use rev_queue::{
    api::{self, ApiState},
    model::*,
    service::Service,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap, fs::OpenOptions, io::Write, net::SocketAddr, path::PathBuf, sync::Arc,
};

#[derive(Serialize, Deserialize)]
struct Runtime {
    reviewer_token: String,
    reviewer: Actor,
    agent_token: String,
    agent: Actor,
}
fn runtime(path: PathBuf) -> anyhow::Result<Runtime> {
    if path.exists() {
        return Ok(serde_json::from_slice(&std::fs::read(path)?)?);
    }
    let config = Runtime {
        reviewer_token: format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4()),
        reviewer: Actor {
            id: ActorId::new(),
            kind: ActorKind::Reviewer,
            display_name: "Local reviewer".into(),
        },
        agent_token: format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4()),
        agent: Actor {
            id: ActorId::new(),
            kind: ActorKind::Agent,
            display_name: "Agent supervisor".into(),
        },
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&serde_json::to_vec_pretty(&config)?)?;
    file.sync_all()?;
    Ok(config)
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "serve".into());
    let repo = args.next().unwrap_or_else(|| ".".into());
    let service = Service::open(&repo)?;
    match command.as_str() {
        "serve" => {
            let address: SocketAddr = args
                .next()
                .unwrap_or_else(|| "127.0.0.1:3000".into())
                .parse()?;
            anyhow::ensure!(address.ip().is_loopback(), "server must bind loopback");
            let config_path = service
                .repository()
                .common_dir()
                .join("revqueue-runtime.json");
            let config = runtime(config_path.clone())?;
            let capabilities = BTreeMap::from([
                (config.reviewer_token, config.reviewer),
                (config.agent_token, config.agent),
            ]);
            let app = api::router(ApiState {
                service,
                capabilities: Arc::new(capabilities),
            });
            let listener = tokio::net::TcpListener::bind(address).await?;
            eprintln!(
                "Rev Queue listening on http://{}; credentials: {}",
                listener.local_addr()?,
                config_path.display()
            );
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        "export" => {
            let destination = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("export requires a new bundle path"))?;
            service
                .repository()
                .export_bundle(std::path::Path::new(&destination))?;
            println!("Exported Rev Queue history to {destination}");
        }
        "import" => {
            let bundle = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("import requires a bundle path"))?;
            service
                .repository()
                .import_bundle(std::path::Path::new(&bundle))?;
            for (goal, _) in service.repository().list_goals()? {
                service.repair(&GoalId(uuid::Uuid::parse_str(&goal)?))?;
            }
            println!("Imported and verified Rev Queue history");
        }
        "repair" => {
            let id =
                GoalId(uuid::Uuid::parse_str(&args.next().ok_or_else(|| {
                    anyhow::anyhow!("repair requires a goal UUID")
                })?)?);
            let state = service.repair(&id)?;
            println!("Verified goal {} through sequence {}", id, state.sequence);
        }
        "inspect" => {
            for (goal, head) in service.repository().list_goals()? {
                println!("{goal} {head}");
            }
        }
        _ => anyhow::bail!(
            "usage: rev_queue [serve [repository [127.0.0.1:3000]] | inspect repository | repair repository goal-uuid | export repository new.bundle | import repository backup.bundle]"
        ),
    }
    Ok(())
}
