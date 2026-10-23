use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::Context;
use clap::{Parser, Subcommand};
use memhub_app::{audit::LogQuery, Hub, HubConfig};
use memhub_mcp::{
    http::{self, HttpConfig, HttpState},
    stdio::{self, StdioLogin},
};
use serde_json::Value;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "memhub",
    version,
    about = "Cross-Repo Memory Hub: shared memory for multiple agents across multiple repos"
)]
struct Cli {
    /// Data directory holding memories, registry and audit logs.
    #[arg(
        long,
        global = true,
        env = "MEMHUB_DATA_DIR",
        default_value = "./memhub-data"
    )]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over Streamable HTTP (endpoint: /mcp).
    Serve {
        #[arg(long, env = "MEMHUB_BIND", default_value = "127.0.0.1:7878")]
        bind: SocketAddr,
        /// Close sessions without activity for this many seconds.
        #[arg(long, default_value_t = 1800)]
        idle_timeout_secs: u64,
        /// Trust X-Forwarded-For / X-Real-IP for the client IP (behind a reverse proxy).
        #[arg(long)]
        trust_proxy: bool,
        /// Additional allowed browser Origin (repeatable). Localhost is always allowed.
        #[arg(long = "allowed-origin")]
        allowed_origins: Vec<String>,
        /// Token required by the management web UI (/admin). Without it the UI
        /// is open to anyone who can reach the server.
        #[arg(long, env = "MEMHUB_ADMIN_TOKEN", hide_env_values = true)]
        admin_token: Option<String>,
    },
    /// Run the MCP server over stdio (one agent per process).
    Stdio {
        /// Connect as this agent right after initialize.
        #[arg(long, env = "MEMHUB_AGENT")]
        agent: Option<String>,
        /// Role used to create the agent if it does not exist yet.
        #[arg(long, env = "MEMHUB_ROLE")]
        role: Option<String>,
    },
    /// List registered agents.
    Agents,
    /// Show audit log records (connections and memory changes).
    Logs {
        #[arg(long)]
        agent: Option<String>,
        /// Event type, e.g. connect, disconnect, memory_changed.
        #[arg(long)]
        event: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        memory: Option<String>,
        /// Show only the newest N records.
        #[arg(long, default_value_t = 50)]
        tail: usize,
        /// Print raw JSON lines.
        #[arg(long)]
        json: bool,
        /// Include body diffs in human-readable output.
        #[arg(long)]
        diff: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let hub = Hub::open(HubConfig::new(&cli.data_dir))
        .with_context(|| format!("cannot open data dir {}", cli.data_dir.display()))?;

    match cli.command {
        Command::Serve {
            bind,
            idle_timeout_secs,
            trust_proxy,
            allowed_origins,
            admin_token,
        } => {
            let config = HttpConfig {
                trust_proxy,
                allowed_origins,
                idle_timeout: Duration::from_secs(idle_timeout_secs.max(1)),
                admin_token: admin_token.filter(|t| !t.trim().is_empty()),
            };
            tokio::runtime::Runtime::new()?.block_on(serve(hub, bind, config))
        }
        Command::Stdio { agent, role } => {
            stdio::run(hub, StdioLogin { agent, role })?;
            Ok(())
        }
        Command::Agents => {
            for agent in hub.registry().agents()? {
                let seen = agent
                    .last_seen_at
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{:<24} {:<18} last seen {seen}  repos: {}",
                    agent.name,
                    agent.role,
                    agent.repos.join(", ")
                );
            }
            Ok(())
        }
        Command::Logs {
            agent,
            event,
            session,
            memory,
            tail,
            json,
            diff,
        } => {
            let records = hub.audit().read(&LogQuery {
                agent,
                event,
                session_id: session,
                memory_id: memory,
                since: None,
                limit: Some(tail),
            })?;
            for record in records {
                if json {
                    println!("{record}");
                } else {
                    print_record(&record, diff);
                }
            }
            Ok(())
        }
    }
}

async fn serve(
    hub: std::sync::Arc<Hub>,
    bind: SocketAddr,
    config: HttpConfig,
) -> anyhow::Result<()> {
    let state = HttpState::new(hub.clone(), config);
    let reaper = http::spawn_reaper(state.clone());
    let app = http::router(state.clone());

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot bind {bind}"))?;
    tracing::info!(
        "memhub listening on http://{bind}/mcp (data dir {})",
        hub.data_root().display()
    );
    if state.config().admin_token.is_some() {
        tracing::info!("management UI at http://{bind}/admin (admin token required)");
    } else {
        tracing::warn!(
            "management UI at http://{bind}/admin is open to anyone who can reach this \
             server; set --admin-token / MEMHUB_ADMIN_TOKEN to protect it"
        );
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
    .await?;

    reaper.abort();
    state.close_all("server_shutdown");
    Ok(())
}

fn print_record(r: &Value, with_diff: bool) {
    let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("");
    let ts = s("ts").get(..19).unwrap_or(s("ts")).replace('T', " ");
    let agent = r.get("agent").and_then(Value::as_str).unwrap_or("-");
    let event = s("event");
    let detail = match event {
        "connect" => format!(
            "client={}",
            r.get("client").and_then(Value::as_str).unwrap_or("?")
        ),
        "disconnect" => format!(
            "reason={} after {:.1}s",
            s("reason"),
            r.get("duration_secs")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        ),
        "agent_registered" => format!("role={}", s("role")),
        "repo_joined" | "repo_left" => format!(
            "repo={}{}",
            s("repo"),
            if r.get("created") == Some(&Value::Bool(true)) {
                " (new)"
            } else {
                ""
            }
        ),
        "memory_changed" => {
            let v = |k: &str| r.get(k).and_then(Value::as_u64);
            let versions = match (v("version_before"), v("version_after")) {
                (Some(b), Some(a)) => format!("v{b}->v{a}"),
                (None, Some(a)) => format!("v{a}"),
                (Some(b), None) => format!("was v{b}"),
                (None, None) => String::new(),
            };
            let fields = r
                .get("changed_fields")
                .and_then(Value::as_array)
                .map(|f| {
                    f.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let mut d = format!(
                "{} {} \"{}\" {versions}",
                s("action"),
                s("memory_id"),
                s("title"),
            );
            if !fields.is_empty() {
                d.push_str(&format!(" [{fields}]"));
            }
            if let Some(g) = r.get("group").and_then(Value::as_str) {
                d.push_str(&format!(" group={g}"));
            }
            d
        }
        "folder_created" => format!("{}/{}", s("scope"), s("owner")),
        "folder_deleted" => format!(
            "{}/{} ({} memories)",
            s("scope"),
            s("owner"),
            r.get("memory_count").and_then(Value::as_u64).unwrap_or(0)
        ),
        _ => String::new(),
    };
    println!(
        "{ts}  {event:<16} {agent:<20} {:<15} {:<6} session={}  {detail}",
        s("ip"),
        s("transport"),
        s("session_id").get(..8).unwrap_or(s("session_id")),
    );
    if with_diff {
        if let Some(diff) = r.get("diff").and_then(Value::as_str) {
            for line in diff.lines() {
                println!("    {line}");
            }
        }
    }
}
