use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use memhub_app::{
    audit::LogQuery,
    curation::{EvalCase, Proposal, ProposalState, Verdict},
    AgentSession, ConnInfo, Hub, HubConfig,
};
use memhub_curator::{config, connect_hub, port, Config, Curator, LlmClient, RunOptions};
use memhub_mcp::{
    http::{self, HttpConfig, HttpState},
    stdio::{self, StdioLogin},
};
use serde_json::Value;
use tracing_subscriber::EnvFilter;

const CURATOR_AGENT: &str = "memhub-curator";
const DEFAULT_CURATOR_CONFIG: &str = "config/memhub2-curator.yaml";

#[derive(Parser)]
#[command(
    name = "memhub2",
    version,
    about = "Cross-Repo Memory Hub 2.0: layered shared memory (evidence -> curated experience) for agents across repos"
)]
struct Cli {
    /// Data directory holding memories, evidence, registry and audit logs.
    #[arg(
        long,
        global = true,
        env = "MEMHUB2_DATA_DIR",
        default_value = "./memhub2-data"
    )]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over Streamable HTTP (endpoint: /mcp).
    Serve {
        #[arg(long, env = "MEMHUB2_BIND", default_value = "127.0.0.1:7880")]
        bind: SocketAddr,
        /// Close sessions without activity for this many seconds.
        #[arg(long, default_value_t = 3600)]
        idle_timeout_secs: u64,
        /// Trust X-Forwarded-For / X-Real-IP for the client IP (behind a reverse proxy).
        #[arg(long)]
        trust_proxy: bool,
        /// Additional allowed browser Origin (repeatable). Localhost is always allowed.
        #[arg(long = "allowed-origin")]
        allowed_origins: Vec<String>,
        /// Token required by the management web UI (/admin). Without it the UI
        /// is open to anyone who can reach the server.
        #[arg(long, env = "MEMHUB2_ADMIN_TOKEN", hide_env_values = true)]
        admin_token: Option<String>,
        /// Token that turns an MCP session into a curator session (header
        /// X-Memhub-Curator-Token). Without it, curation over HTTP is disabled.
        #[arg(long, env = "MEMHUB2_CURATOR_TOKEN", hide_env_values = true)]
        curator_token: Option<String>,
        /// Keep expired sessions (idle timeout, server restart) resumable by
        /// their id, with the same agent identity, for this many seconds.
        #[arg(long, default_value_t = 7 * 24 * 3600)]
        resume_ttl_secs: u64,
    },
    /// Run the MCP server over stdio (one agent per process).
    Stdio {
        /// Connect as this agent right after initialize.
        #[arg(long, env = "MEMHUB2_AGENT")]
        agent: Option<String>,
        /// Role used to create the agent if it does not exist yet.
        #[arg(long, env = "MEMHUB2_ROLE")]
        role: Option<String>,
        /// Label of this running copy (user, host, ...), recorded with the
        /// connection so instances can be counted.
        #[arg(long, env = "MEMHUB2_INSTANCE")]
        instance: Option<String>,
        /// Open a curator session (writes experience, curation tools).
        #[arg(long)]
        curator: bool,
    },
    /// List registered agents.
    Agents,
    /// Show audit log records (connections, evidence, memory changes, curation).
    Logs {
        #[arg(long)]
        agent: Option<String>,
        /// Event type, e.g. connect, evidence_recorded, memory_changed, memory_searched.
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
    /// Run the LLM curator once: turn new evidence into experience (and
    /// review memories ported from 1.0). Talks to the running server.
    Curate {
        #[arg(long, env = "MEMHUB2_CURATOR_CONFIG", default_value = DEFAULT_CURATOR_CONFIG)]
        config: PathBuf,
        /// Only this target, "repo:<id>" or "agent:<name>" (repeatable).
        #[arg(long = "target")]
        targets: Vec<String>,
        /// Draft and check one batch per target; store nothing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the review of memories ported from 1.0.
        #[arg(long, conflicts_with = "backfill_only")]
        no_backfill: bool,
        /// Only review memories ported from 1.0.
        #[arg(long)]
        backfill_only: bool,
        /// Write the JSON run report here.
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Port Memory Hub 1.0 into 2.0: reads the 1.0 server (port.v1_url) and
    /// imports through the running 2.0 server (hub.url). Idempotent.
    Port {
        #[arg(long, env = "MEMHUB2_CURATOR_CONFIG", default_value = DEFAULT_CURATOR_CONFIG)]
        config: PathBuf,
        /// Report where every memory would go without writing.
        #[arg(long)]
        dry_run: bool,
        /// Only this owner, "repo:<id>" or "agent:<name>" (repeatable).
        #[arg(long)]
        only: Vec<String>,
        /// Write the JSON port report here.
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Inspect and decide curator proposals (works on the data dir directly).
    Proposals {
        #[command(subcommand)]
        action: ProposalAction,
    },
    /// Check retrieval quality: each case's query should find an expected
    /// memory in the top k.
    Eval {
        /// YAML list of {query, repo?, expect: [memory ids]}.
        #[arg(long)]
        cases: PathBuf,
        #[arg(long, default_value_t = 5)]
        k: usize,
    },
    /// Write the commented curator config example.
    InitConfig {
        #[arg(long, default_value = DEFAULT_CURATOR_CONFIG)]
        path: PathBuf,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ProposalAction {
    /// List proposals (default: pending ones).
    List {
        /// pending, applied, rejected, partial or all.
        #[arg(long, default_value = "pending")]
        state: String,
    },
    /// Show a proposal with its operations, checks and decisions.
    Show { id: String },
    /// Approve undecided operations as a human (including those marked
    /// needs_human) and apply them.
    Approve {
        id: String,
        /// Only these operation indexes (repeatable); default: every undecided one that passes its checks.
        #[arg(long = "op")]
        ops: Vec<usize>,
        #[arg(long, default_value = "approved by a human")]
        reason: String,
    },
    /// Reject undecided operations.
    Reject {
        id: String,
        /// Only these operation indexes (repeatable); default: every undecided one.
        #[arg(long = "op")]
        ops: Vec<usize>,
        #[arg(long)]
        reason: String,
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
    let open_hub = || {
        Hub::open(HubConfig::new(&cli.data_dir))
            .with_context(|| format!("cannot open data dir {}", cli.data_dir.display()))
    };

    match cli.command {
        Command::Serve {
            bind,
            idle_timeout_secs,
            trust_proxy,
            allowed_origins,
            admin_token,
            curator_token,
            resume_ttl_secs,
        } => {
            let hub = open_hub()?;
            let config = HttpConfig {
                trust_proxy,
                allowed_origins,
                idle_timeout: Duration::from_secs(idle_timeout_secs.max(1)),
                admin_token: admin_token.filter(|t| !t.trim().is_empty()),
                curator_token: curator_token
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty()),
                resume_ttl: Duration::from_secs(resume_ttl_secs),
                resume_file: Some(cli.data_dir.join("run").join("resumable-sessions.json")),
            };
            tokio::runtime::Runtime::new()?.block_on(serve(hub, bind, config))
        }
        Command::Stdio {
            agent,
            role,
            instance,
            curator,
        } => {
            stdio::run(
                open_hub()?,
                StdioLogin {
                    agent,
                    role,
                    instance,
                    curator,
                },
            )?;
            Ok(())
        }
        Command::Agents => {
            for agent in open_hub()?.registry().agents()? {
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
            let records = open_hub()?.audit().read(&LogQuery {
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
        Command::Curate {
            config,
            targets,
            dry_run,
            no_backfill,
            backfill_only,
            report,
        } => {
            let config = Config::load(Some(&config))?;
            config.validate_for_curation()?;
            let mut hub = connect_hub(&config)?;
            let llm = LlmClient::new(&config.llm);
            let options = RunOptions {
                targets,
                dry_run,
                backfill: no_backfill.then_some(false).or(backfill_only.then_some(true)),
                backfill_only,
            };
            let result = Curator::new(&mut hub, &llm, &config, options).run()?;
            print!("{}", result.summary());
            if let Some(path) = report {
                write_json(&path, &result)?;
            } else if dry_run {
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            let failed = result.targets.iter().any(|t| !t.errors.is_empty());
            if failed {
                bail!("curation finished with errors (see above)");
            }
            Ok(())
        }
        Command::Port {
            config,
            dry_run,
            only,
            report,
        } => {
            let config = Config::load(Some(&config))?;
            let mut v2 = connect_hub(&config)?;
            let mut v1 = port::connect_v1(&config)?;
            let result = port::run(
                &mut v1,
                &mut v2,
                &config,
                &port::PortOptions { dry_run, only },
            )?;
            print!("{}", result.summary());
            for e in &result.errors {
                eprintln!("  error: {e}");
            }
            if let Some(path) = report {
                write_json(&path, &result)?;
            }
            if !result.errors.is_empty() {
                bail!("port finished with {} errors", result.errors.len());
            }
            Ok(())
        }
        Command::Proposals { action } => proposals(open_hub()?, action),
        Command::Eval { cases, k } => {
            let text = std::fs::read_to_string(&cases)
                .with_context(|| format!("cannot read {}", cases.display()))?;
            let cases: Vec<EvalCase> = serde_yaml::from_str(&text)
                .with_context(|| format!("invalid eval cases in {}", cases.display()))?;
            let session = cli_session(&open_hub()?)?;
            let report = session.evaluate(&cases, k)?;
            for c in &report.cases {
                println!(
                    "{:<6} {}  top: {}",
                    c.rank.map_or("miss".to_string(), |r| format!("#{r}")),
                    c.query,
                    c.top.join(", ")
                );
            }
            println!("recall@{}: {:.2}", report.k, report.recall);
            session.close("cli_done");
            Ok(())
        }
        Command::InitConfig { path, force } => {
            if path.exists() && !force {
                bail!("{} exists; pass --force to overwrite", path.display());
            }
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, config::EXAMPLE)?;
            println!(
                "wrote {}; set llm.base_url, llm.model and llm.api_key",
                path.display()
            );
            Ok(())
        }
    }
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("cannot write {}", path.display()))?;
    eprintln!("report written to {}", path.display());
    Ok(())
}

/// A local curator session for CLI commands working on the data dir.
fn cli_session(hub: &Arc<Hub>) -> anyhow::Result<AgentSession> {
    let mut session = hub.open_session(ConnInfo {
        transport: "cli".into(),
        ip: "local".into(),
        client: Some(format!("memhub2-cli {}", env!("CARGO_PKG_VERSION"))),
        instance: None,
    });
    session.connect(CURATOR_AGENT, Some("curator"), None)?;
    session.grant_curator();
    Ok(session)
}

fn proposals(hub: Arc<Hub>, action: ProposalAction) -> anyhow::Result<()> {
    let session = cli_session(&hub)?;
    match action {
        ProposalAction::List { state } => {
            let state = match state.as_str() {
                "all" => None,
                s => Some(
                    serde_json::from_value::<ProposalState>(Value::String(s.into()))
                        .map_err(|_| anyhow::anyhow!("unknown state '{s}'"))?,
                ),
            };
            let list = session.list_proposals(state)?;
            for p in &list {
                let needs_human = p
                    .undecided()
                    .iter()
                    .filter(|i| p.checks[**i].needs_human)
                    .count();
                println!(
                    "{}  {:<28} {:<8} ops={} undecided={} needs_human={}  {}",
                    p.id,
                    p.target.to_string(),
                    serde_json::to_value(p.state)?.as_str().unwrap_or(""),
                    p.ops.len(),
                    p.undecided().len(),
                    needs_human,
                    p.note.as_deref().unwrap_or("")
                );
            }
            if list.is_empty() {
                println!("no proposals");
            }
        }
        ProposalAction::Show { id } => print_proposal(&session.get_proposal(&id)?),
        ProposalAction::Approve { id, ops, reason } => {
            let p = session.get_proposal(&id)?;
            let indexes: Vec<usize> = if ops.is_empty() {
                p.undecided().into_iter().filter(|i| p.checks[*i].ok).collect()
            } else {
                ops
            };
            let verdicts: Vec<Verdict> = indexes
                .into_iter()
                .map(|index| Verdict {
                    index,
                    approve: true,
                    reason: reason.clone(),
                })
                .collect();
            print_proposal(&session.review_changes(&id, &verdicts, true)?);
        }
        ProposalAction::Reject { id, ops, reason } => {
            let p = session.get_proposal(&id)?;
            let indexes = if ops.is_empty() { p.undecided() } else { ops };
            let verdicts: Vec<Verdict> = indexes
                .into_iter()
                .map(|index| Verdict {
                    index,
                    approve: false,
                    reason: reason.clone(),
                })
                .collect();
            print_proposal(&session.review_changes(&id, &verdicts, true)?);
        }
    }
    session.close("cli_done");
    Ok(())
}

fn print_proposal(p: &Proposal) {
    println!(
        "{}  target={}  state={}  by {} at {}",
        p.id,
        p.target,
        serde_json::to_value(p.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        p.created_by,
        p.created_at.format("%Y-%m-%d %H:%M:%S")
    );
    if let Some(note) = &p.note {
        println!("note: {note}");
    }
    for (i, op) in p.ops.iter().enumerate() {
        let check = &p.checks[i];
        let decision = match p.decision(i) {
            Some(d) if d.approved => format!(
                "APPLIED{}",
                d.applied_id
                    .as_ref()
                    .map(|id| format!(" -> {id}"))
                    .unwrap_or_default()
            ),
            Some(d) => format!(
                "REJECTED: {}",
                d.error.clone().unwrap_or_else(|| d.reason.clone())
            ),
            None if !check.ok => "FAILS CHECKS".into(),
            None if check.needs_human => "WAITING FOR HUMAN".into(),
            None => "undecided".into(),
        };
        println!(
            "\n[{i}] {} {}  {decision}",
            op.name(),
            check.target.as_deref().unwrap_or("")
        );
        if let Some(s) = check.status {
            println!("    status={s} support={}", check.support.unwrap_or(0));
        }
        for e in &check.errors {
            println!("    error: {e}");
        }
        for w in &check.warnings {
            println!("    warning: {w}");
        }
        let detail = serde_yaml::to_string(op).unwrap_or_default();
        for line in detail.lines() {
            println!("    | {line}");
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
        "memhub2 listening on http://{bind}/mcp (data dir {})",
        hub.data_root().display()
    );
    if state.config().admin_token.is_some() {
        tracing::info!("management UI at http://{bind}/admin (admin token required)");
    } else {
        tracing::warn!(
            "management UI at http://{bind}/admin is open to anyone who can reach this \
             server; set --admin-token / MEMHUB2_ADMIN_TOKEN to protect it"
        );
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        shutdown_signal().await;
        tracing::info!("shutting down");
    })
    .await?;

    reaper.abort();
    state.close_all("server_shutdown");
    Ok(())
}

/// Ctrl-C / SIGINT, or SIGTERM (sent by service managers) on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn print_record(r: &Value, with_diff: bool) {
    let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("");
    let ts = s("ts").get(..19).unwrap_or(s("ts")).replace('T', " ");
    let agent = r.get("agent").and_then(Value::as_str).unwrap_or("-");
    let event = s("event");
    let detail = match event {
        "connect" => format!(
            "client={}{}{}",
            r.get("client").and_then(Value::as_str).unwrap_or("?"),
            r.get("instance")
                .and_then(Value::as_str)
                .map(|i| format!(" instance={i}"))
                .unwrap_or_default(),
            if r.get("resumed") == Some(&Value::Bool(true)) {
                " resumed"
            } else {
                ""
            }
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
            if let Some(p) = r.get("previous_id").and_then(Value::as_str) {
                d.push_str(&format!(" from={p}"));
            }
            d
        }
        "folder_created" | "folder_described" | "folder_deleted" => {
            let mut d = format!("{}/{}", s("scope"), s("owner"));
            if !s("folder").is_empty() {
                d.push_str(&format!("/{}", s("folder")));
            }
            if let Some(n) = r.get("memory_count").and_then(Value::as_u64) {
                d.push_str(&format!(" ({n} memories)"));
            }
            if !s("description").is_empty() {
                d.push_str(&format!(" \"{}\"", s("description")));
            }
            d
        }
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
