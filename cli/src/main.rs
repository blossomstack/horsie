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
use horsie::config::HorsieConfig;
use horsie::connect;
use horsie::error::CliError;
use horsie::session::{self, EventsMode};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "horsie",
    version,
    about = "Session-server client: run this machine as a runtime vendor, tail sessions, and manage the plugin library"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage marketplaces — repos that index plugins you can install by name.
    Marketplace {
        #[command(subcommand)]
        action: MarketplaceAction,
    },
    /// Manage the shared plugin library (skills + SessionStart hooks for runtimes).
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Commands against a session server (`horsie-server`).
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Dial a session server as this machine's runtime — wraps the standalone
    /// `horsie-runtime --endpoint ...` flow so installing `horsie` is enough.
    Connect {
        /// `http(s)://host:port` of the session server to dial.
        #[arg(long)]
        server: String,
        /// Repeatable `[name=]path` workspace root. A bare path defaults to
        /// name "main". At least one is required.
        #[arg(long = "workspace", required = true)]
        workspace: Vec<String>,
        /// Vendor name the server publishes this machine under. Defaults to
        /// "local", matching the server's default vendor pickup.
        #[arg(long, alias = "runtime-id", default_value = "local")]
        name: String,
        /// Apply the server's sandbox policy to every runtime this agent
        /// spawns. Off by default: the machine is already your own.
        #[arg(long)]
        sandbox: bool,
        /// Removed: run the agent under a process manager instead.
        #[arg(long, hide = true)]
        background: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
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
        /// Session server base URL.
        #[arg(long, default_value = "http://127.0.0.1:3789")]
        server: String,
        /// Which events to write.
        #[arg(long, value_enum, default_value = "messages")]
        events: EventsMode,
    },
}

#[derive(Subcommand)]
enum MarketplaceAction {
    /// Add a marketplace by cloning its repo and reading its plugin index.
    Add {
        /// Git URL of the marketplace repo.
        url: String,
        /// Registered name (default: the index's own name, else the repo basename).
        #[arg(long)]
        name: Option<String>,
        /// Git ref/branch to check out.
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Re-add over an existing marketplace of the same name.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List added marketplaces.
    List {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List the plugins a marketplace offers.
    Show {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Update a marketplace's index (git pull).
    Update {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Remove a marketplace. Plugins installed from it stay installed.
    Remove {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum PluginAction {
    /// Install a plugin by cloning its git repo into the shared library.
    Install {
        /// Git URL of the plugin repo, or `<plugin>@<marketplace>` to install
        /// from a marketplace added with `horsie marketplace add`.
        target: String,
        /// Install name (default: derived from the URL).
        #[arg(long)]
        name: Option<String>,
        /// Git ref/branch to check out.
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Reinstall over an existing plugin of the same name.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List installed plugins.
    List {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Update an installed plugin (git pull).
    Update {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Remove an installed plugin.
    Remove {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// Resolve the plugin library paths from config: the symlink farm
/// (`storage.plugins_dir`) and the shared clones (`<data_dir>/sources`).
fn resolve_plugin_paths(config: Option<&Path>) -> Result<horsie::plugins::PluginPaths, CliError> {
    let cfg = HorsieConfig::resolve(config)?;
    Ok(horsie::plugins::PluginPaths {
        sources: cfg.storage.data_dir.join("sources"),
        marketplaces: cfg.storage.data_dir.join("marketplaces"),
        plugins: cfg.storage.plugins_dir,
    })
}

async fn dispatch(command: Command) -> Result<i32, CliError> {
    match command {
        Command::Marketplace { action } => match action {
            MarketplaceAction::Add {
                url,
                name,
                git_ref,
                force,
                config,
            } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                let added = horsie::marketplace::add(&paths, &url, name, git_ref, force)?;
                println!("added marketplace '{added}'");
                Ok(0)
            }
            MarketplaceAction::List { config } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                let markets = horsie::marketplace::list(&paths);
                if markets.is_empty() {
                    println!("no marketplaces added");
                } else {
                    println!("{:<24} {:>7}  SOURCE", "NAME", "PLUGINS");
                    for m in markets {
                        println!("{:<24} {:>7}  {}", m.name, m.plugin_count, m.source);
                    }
                }
                Ok(0)
            }
            MarketplaceAction::Show { name, config } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                let plugins = horsie::marketplace::show(&paths, &name)?;
                if plugins.is_empty() {
                    println!("marketplace '{name}' offers no plugins");
                } else {
                    println!("{:<28} {:<10} DESCRIPTION", "NAME", "VERSION");
                    for p in plugins {
                        println!(
                            "{:<28} {:<10} {}",
                            p.name,
                            p.version.as_deref().unwrap_or("-"),
                            truncate(p.description.as_deref().unwrap_or(""), 60)
                        );
                    }
                }
                Ok(0)
            }
            MarketplaceAction::Update { name, config } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                horsie::marketplace::update(&paths, &name)?;
                println!("updated marketplace '{name}'");
                Ok(0)
            }
            MarketplaceAction::Remove { name, config } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                horsie::marketplace::remove(&paths, &name)?;
                println!("removed marketplace '{name}'");
                Ok(0)
            }
        },
        Command::Plugin { action } => match action {
            PluginAction::Install {
                target,
                name,
                git_ref,
                force,
                config,
            } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                let target = horsie::plugins::InstallTarget::parse(&target);
                let installed = horsie::plugins::install(&paths, &target, name, git_ref, force)?;
                println!(
                    "installed plugin '{installed}' into {}",
                    paths.plugins.display()
                );
                Ok(0)
            }
            PluginAction::List { config } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                let plugins = horsie::plugins::list(&paths);
                if plugins.is_empty() {
                    println!("no plugins installed");
                } else {
                    println!("{:<24} {:<10} SOURCE", "NAME", "VERSION");
                    for p in plugins {
                        println!(
                            "{:<24} {:<10} {}",
                            p.name,
                            p.version.as_deref().unwrap_or("-"),
                            p.source
                        );
                    }
                }
                Ok(0)
            }
            PluginAction::Update { name, config } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                horsie::plugins::update(&paths, &name)?;
                println!("updated plugin '{name}'");
                Ok(0)
            }
            PluginAction::Remove { name, config } => {
                let paths = resolve_plugin_paths(config.as_deref())?;
                horsie::plugins::remove(&paths, &name)?;
                println!("removed plugin '{name}'");
                Ok(0)
            }
        },
        Command::Session { action } => match action {
            SessionAction::Tail {
                session_id,
                output,
                server,
                events,
            } => {
                session::tail(&server, &session_id, &output, events).await?;
                Ok(0)
            }
        },
        Command::Connect {
            server,
            workspace,
            name,
            sandbox,
            background,
            config,
        } => {
            let cfg = HorsieConfig::resolve(config.as_deref())?;
            let runtime_bin = cfg
                .runtime
                .bin
                .clone()
                .unwrap_or_else(connect::default_runtime_bin);
            let (plugins_dir, hook_path) = horsie::plugins::library_for_runtime(
                &cfg.storage.plugins_dir,
                cfg.runtime.hook_path.clone(),
            );
            let sources = cfg.storage.data_dir.join("sources");
            let plugins = plugins_dir.map(|dir| connect::PluginLibrary {
                dir,
                sources: Some(sources),
                hook_path,
            });
            connect::run(
                &runtime_bin,
                &server,
                &workspace,
                &name,
                background,
                &cfg.storage.state_dir,
                plugins,
                sandbox,
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

/// Clip `s` to `max` display columns, marking elision with an ellipsis. Used for
/// marketplace descriptions, which routinely run to several hundred characters.
fn truncate(s: &str, max: usize) -> String {
    let flat = s.replace(['\n', '\r'], " ");
    if flat.chars().count() <= max {
        return flat;
    }
    let kept: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}
