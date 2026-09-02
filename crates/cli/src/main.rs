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
    /// The project to act in, by id or by name.
    ///
    /// Global rather than per-subcommand because every server-facing command
    /// needs one, and because it is the same answer for all of them. Omitted →
    /// the account's default project, resolved from the server: there is
    /// nothing to guess, and a stale id stored locally would be a worse failure
    /// than a round trip.
    #[arg(long, global = true, value_name = "ID_OR_NAME")]
    project: Option<String>,

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
        /// Predefined environment to run in. Alternative to --vendor; one of
        /// the two is required.
        #[arg(long)]
        environment: Option<String>,
        /// Runtime vendor to run on. Alternative to --environment.
        #[arg(long)]
        vendor: Option<String>,
        /// Repeatable clone URL, cloned into the workspace. Goes with
        /// --vendor: a named environment carries its own repos.
        #[arg(long = "repo")]
        repo: Vec<String>,
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
        /// Print the definition as JSON — the document `apply` takes back.
        #[arg(long)]
        json: bool,
        #[arg(long)]
        server: Option<String>,
    },
    /// Create or fully replace a workflow from a JSON definition. The name comes
    /// from the file. Round-trips with `get --json`.
    Apply {
        /// Path to the definition.
        #[arg(short = 'f', long = "file")]
        file: String,
        #[arg(long)]
        server: Option<String>,
    },
    /// Delete a workflow. Its runs are sessions and are left alone — each holds
    /// its own snapshot of the graph.
    Delete {
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
        /// Predefined environment to run in. Alternative to --vendor; one of
        /// the two is required.
        #[arg(long)]
        environment: Option<String>,
        /// Runtime vendor to host the run's shared runtime. Alternative to
        /// --environment.
        #[arg(long)]
        vendor: Option<String>,
        /// Repeatable clone URL, cloned into the run's shared workspace. Goes
        /// with --vendor: a named environment carries its own repos.
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
    /// Re-run one step execution. Appends an attempt; the workspace is not
    /// rolled back.
    Retry {
        /// Session UUID of the run.
        session_id: String,
        /// Index of the execution to re-run, from `workflow status`.
        step_index: u32,
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

async fn dispatch(command: Command, project: Option<&str>) -> Result<i32, CliError> {
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
                session::tail(
                    &server,
                    project,
                    &session_id,
                    &output,
                    events,
                    agent.as_deref(),
                )
                .await?;
                Ok(0)
            }
            SessionAction::List { server } => {
                let server = horsie::config::resolve_server(server, None)?;
                session::list(&server, project).await?;
                Ok(0)
            }
            SessionAction::Status { session_id, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                session::status(&server, project, &session_id).await?;
                Ok(0)
            }
        },
        Command::Workflow { action } => match action {
            WorkflowAction::List { server } => {
                let server = horsie::config::resolve_server(server, None)?;
                workflow::list(&server, project).await?;
                Ok(0)
            }
            WorkflowAction::Get { name, json, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                workflow::get(&server, project, &name, json).await?;
                Ok(0)
            }
            WorkflowAction::Apply { file, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                workflow::apply(&server, project, &file).await?;
                Ok(0)
            }
            WorkflowAction::Delete { name, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                workflow::delete(&server, project, &name).await?;
                Ok(0)
            }
            WorkflowAction::Run {
                name,
                input,
                environment,
                vendor,
                repo,
                session_name,
                server,
            } => {
                let server = horsie::config::resolve_server(server, None)?;
                let environment =
                    horsie::environment::environment_from_flags(environment, vendor, repo)?;
                workflow::run(&server, project, &name, input, environment, session_name).await?;
                Ok(0)
            }
            WorkflowAction::Status { session_id, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                workflow::status(&server, project, &session_id).await?;
                Ok(0)
            }
            WorkflowAction::Retry {
                session_id,
                step_index,
                server,
            } => {
                let server = horsie::config::resolve_server(server, None)?;
                workflow::retry(&server, project, &session_id, step_index).await?;
                Ok(0)
            }
        },
        Command::Agent { action } => match action {
            AgentAction::List { server } => {
                let server = horsie::config::resolve_server(server, None)?;
                agent::list(&server, project).await?;
                Ok(0)
            }
            AgentAction::Get { name, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                agent::get(&server, project, &name).await?;
                Ok(0)
            }
            AgentAction::Invoke {
                name,
                message,
                environment,
                vendor,
                repo,
                session_name,
                server,
            } => {
                let server = horsie::config::resolve_server(server, None)?;
                let environment =
                    horsie::environment::environment_from_flags(environment, vendor, repo)?;
                agent::invoke(&server, project, &name, message, environment, session_name).await?;
                Ok(0)
            }
        },
        Command::Routine { action } => match action {
            RoutineAction::List { server } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::routines::list(&server, project).await?;
                Ok(0)
            }
            RoutineAction::Get { name, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::routines::get(&server, project, &name).await?;
                Ok(0)
            }
            RoutineAction::Invoke { name, server } => {
                let server = horsie::config::resolve_server(server, None)?;
                horsie::routines::invoke(&server, project, &name).await?;
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
                project,
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
    // Before anything opens an https:// or wss:// connection.
    horsie_support::tls::install_crypto_provider();
    let cli = Cli::parse();
    let code = match dispatch(cli.command, cli.project.as_deref()).await {
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
