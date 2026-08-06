#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::wildcard_enum_match_arm
    )
)]

use clap::{Parser, Subcommand};
use horsie::agent;
use horsie::config::HorsieConfig;
use horsie::connect;
use horsie::error::CliError;
use horsie::session::{self, EventsMode};
use horsie::workflow;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "horsie",
    version,
    about = "Session-server client: run this machine as a runtime vendor, and inspect sessions, agents and workflows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Log in to a session server so other commands can reach it.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Commands against a session server (`horsie-server`).
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// List and invoke agent presets on a session server (`horsie-server`).
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Inspect and run workflows on a session server (`horsie-server`).
    Workflow {
        #[command(subcommand)]
        action: WorkflowAction,
    },
    /// List routines and trigger runs on a session server.
    #[command(name = "routines")]
    Routine {
        #[command(subcommand)]
        action: RoutineAction,
    },
    /// Dial a session server as this machine's runtime — wraps the standalone
    /// `horsie-runtime --endpoint ...` flow so installing `horsie` is enough.
    Connect {
        /// `http(s)://host:port` of the session server to dial. Omitted →
        /// the configured default server, else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
        /// Repeatable `[name=]path` workspace root. A bare path defaults to
        /// name "main". At least one is required.
        #[arg(long = "workspace", required = true)]
        workspace: Vec<String>,
        /// Vendor name the server publishes this machine under. Defaults to
        /// "local", matching the server's default vendor pickup.
        #[arg(long, alias = "runtime-id", default_value = "local")]
        name: String,
        /// Do not sandbox the runtimes this agent spawns: they run unconfined
        /// and inherit the ambient environment. Sandboxing (this vendor's
        /// baseline capability spec) is on by default.
        #[arg(long)]
        no_sandbox: bool,
        /// Removed: run the agent under a process manager instead.
        #[arg(long, hide = true)]
        background: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Read and write CLI settings stored in the user config file.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Authorize this machine against a session server, approving in a browser.
    Login {
        /// `http(s)://host:port` of the session server. Omitted → the
        /// configured default server, else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
        /// Store this token instead of running the browser flow. For scripts.
        #[arg(long)]
        token: Option<String>,
        /// Make this server the default one that commands use when `--server`
        /// is omitted. The first server you log in to becomes the default
        /// automatically; this flag forces it for a later login.
        #[arg(long)]
        default: bool,
    },
    /// Forget stored credentials, revoking them server-side when reachable.
    Logout {
        /// Omit to log out of every server.
        #[arg(long)]
        server: Option<String>,
    },
    /// Show which servers this machine has credentials for.
    Status,
}

#[derive(Subcommand)]
enum SessionAction {
    /// Stream a session's messages to a local JSONL file until Ctrl-C.
    /// Resumes after the last recorded event when the output file exists.
    Tail {
        /// Session UUID on the server.
        session_id: String,
        /// Output file, or an existing directory to write
        /// `<session-id>.jsonl` into.
        #[arg(long)]
        output: PathBuf,
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
        /// Which events to write.
        #[arg(long, value_enum, default_value = "messages")]
        events: EventsMode,
        /// Which agent's transcript to follow. Omitted → the session's main
        /// agent. A workflow run has no main agent: pass a step's agent id
        /// from `horsie workflow status`.
        #[arg(long)]
        agent: Option<String>,
    },
    /// List sessions on the server.
    List {
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
    /// Show a session's current status (point-in-time snapshot).
    Status {
        /// Session UUID on the server.
        session_id: String,
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// List agent presets.
    List {
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
    /// Show one agent preset.
    Get {
        /// Agent preset name.
        name: String,
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
    /// Invoke an agent with a message: creates a session and prints its id
    /// and web link immediately.
    Invoke {
        /// Agent preset name.
        name: String,
        /// First user message (required).
        #[arg(short = 'm', long)]
        message: String,
        /// Optional session title.
        #[arg(long)]
        session_name: Option<String>,
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorkflowAction {
    /// List workflows.
    List {
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
    /// Show one workflow: its steps, their outputs, and where each one goes.
    Get {
        /// Workflow name.
        name: String,
        #[arg(long)]
        server: Option<String>,
    },
    /// Start a run. Prints the session id — a run is a session, so
    /// `horsie session status` and `horsie session tail` work on it.
    Run {
        /// Workflow name.
        name: String,
        /// What the first step is handed (required).
        #[arg(short = 'i', long)]
        input: String,
        /// Runtime vendor to host the run's shared runtime. Omitted → the
        /// server's default.
        #[arg(long)]
        vendor: Option<String>,
        /// Repeatable clone URL, cloned into the run's shared workspace.
        #[arg(long = "repo")]
        repo: Vec<String>,
        /// Optional run title.
        #[arg(long)]
        session_name: Option<String>,
        #[arg(long)]
        server: Option<String>,
    },
    /// Show where a run got to: every step execution, and what it did.
    Status {
        /// Session UUID of the run.
        session_id: String,
        #[arg(long)]
        server: Option<String>,
    },
}

#[derive(Subcommand)]
enum RoutineAction {
    /// List routines.
    List {
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
    /// Show one routine.
    Get {
        /// Routine name.
        name: String,
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
    /// Trigger a routine run now, creating an unattended session.
    Invoke {
        /// Routine name.
        name: String,
        /// Session server base URL. Omitted → the configured default server,
        /// else `https://auth.horsie.dev`.
        #[arg(long)]
        server: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Set a config key. Supported keys: `default-server`.
    Set {
        key: String,
        value: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print a config key's current value.
    Get {
        key: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Remove a config key.
    Unset {
        key: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// The config keys `horsie config` knows how to read and write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigKey {
    DefaultServer,
}

impl ConfigKey {
    fn parse(key: &str) -> Result<Self, CliError> {
        match key {
            "default-server" => Ok(Self::DefaultServer),
            other => Err(CliError::Validation(format!(
                "unknown config key '{other}' (supported: default-server)"
            ))),
        }
    }
}

async fn dispatch(command: Command) -> Result<i32, CliError> {
    match command {
        Command::Auth { action } => match action {
            AuthAction::Login {
                server,
                token,
                default,
            } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::auth::login(&server, token.as_deref(), default).await?;
                Ok(0)
            }
            AuthAction::Logout { server } => {
                horsie::auth::logout(server.as_deref()).await?;
                Ok(0)
            }
            AuthAction::Status => {
                horsie::auth::status()?;
                Ok(0)
            }
        },
        Command::Session { action } => match action {
            SessionAction::Tail {
                session_id,
                output,
                server,
                events,
                agent,
            } => {
                let server = horsie::config::resolve_server(server, None)?;
                session::tail(&server, &session_id, &output, events, agent.as_deref()).await?;
                Ok(0)
            }
            SessionAction::List { server } => {
                let server = horsie::config::resolve_server(server, None)?;
                session::list(&server).await?;
                Ok(0)
            }
            SessionAction::Status { session_id, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                session::status(&server, &session_id).await?;
                Ok(0)
            }
        },
        Command::Workflow { action } => match action {
            WorkflowAction::List { server } => {
                let server = horsie::config::resolve_server(server, None)?;
                workflow::list(&server).await?;
                Ok(0)
            }
            WorkflowAction::Get { name, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                workflow::get(&server, &name).await?;
                Ok(0)
            }
            WorkflowAction::Run {
                name,
                input,
                vendor,
                repo,
                session_name,
                server,
            } => {
                let server = horsie::config::resolve_server(server, None)?;
                workflow::run(&server, &name, input, vendor, repo, session_name).await?;
                Ok(0)
            }
            WorkflowAction::Status { session_id, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                workflow::status(&server, &session_id).await?;
                Ok(0)
            }
        },
        Command::Agent { action } => match action {
            AgentAction::List { server } => {
                let server = horsie::config::resolve_server(server, None)?;
                agent::list(&server).await?;
                Ok(0)
            }
            AgentAction::Get { name, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                agent::get(&server, &name).await?;
                Ok(0)
            }
            AgentAction::Invoke {
                name,
                message,
                session_name,
                server,
            } => {
                let server = horsie::config::resolve_server(server, None)?;
                agent::invoke(&server, &name, message, session_name).await?;
                Ok(0)
            }
        },
        Command::Routine { action } => match action {
            RoutineAction::List { server } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::routines::list(&server).await?;
                Ok(0)
            }
            RoutineAction::Get { name, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::routines::get(&server, &name).await?;
                Ok(0)
            }
            RoutineAction::Invoke { name, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::routines::invoke(&server, &name).await?;
                Ok(0)
            }
        },
        Command::Config { action } => match action {
            ConfigAction::Set { key, value, config } => match ConfigKey::parse(&key)? {
                ConfigKey::DefaultServer => {
                    let normalized = horsie::config::set_default_server(&value, config.as_deref())?;
                    println!("default server set to {normalized}");
                    Ok(0)
                }
            },
            ConfigAction::Get { key, config } => match ConfigKey::parse(&key)? {
                ConfigKey::DefaultServer => {
                    match horsie::config::get_default_server(config.as_deref())? {
                        Some(server) => {
                            println!("{server}");
                            Ok(0)
                        }
                        None => {
                            println!("no default server set");
                            Ok(0)
                        }
                    }
                }
            },
            ConfigAction::Unset { key, config } => match ConfigKey::parse(&key)? {
                ConfigKey::DefaultServer => {
                    match horsie::config::unset_default_server(config.as_deref())? {
                        Some(server) => println!("removed default server {server}"),
                        None => println!("no default server set"),
                    }
                    Ok(0)
                }
            },
        },
        Command::Connect {
            server,
            workspace,
            name,
            no_sandbox,
            background,
            config,
        } => {
            let server = horsie::config::resolve_server(server, config.as_deref())?;
            let cfg = HorsieConfig::resolve(config.as_deref())?;
            let runtime_bin = cfg
                .runtime
                .bin
                .clone()
                .unwrap_or_else(connect::default_runtime_bin);
            let hook_path = connect::resolve_hook_path(cfg.runtime.hook_path.clone());
            println!(
                "note: skills now come from the server per session; a local \
                 plugin library is no longer read and can be deleted"
            );
            connect::run(
                &runtime_bin,
                &server,
                &workspace,
                &name,
                background,
                &cfg.storage.state_dir,
                hook_path,
                !no_sandbox,
            )
            .await
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match dispatch(cli.command).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_key_parse_accepts_default_server() {
        assert_eq!(
            ConfigKey::parse("default-server").unwrap(),
            ConfigKey::DefaultServer
        );
    }

    #[test]
    fn config_key_parse_rejects_unknown_keys() {
        let err = ConfigKey::parse("default-vendor").unwrap_err();
        assert!(format!("{err}").contains("unknown config key"), "{err}");
    }
}
