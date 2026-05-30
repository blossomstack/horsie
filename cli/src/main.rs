use clap::{Parser, Subcommand};
use cli::config::OctoberConfig;
use cli::run::{ResumeParams, RunParams, resume, run};
use cli::validate::validate;
use models::workflow::WorkflowDefinition;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "october",
    about = "Run agent workflows in a nono-sandboxed runtime"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a workflow against a config; report all errors.
    Validate {
        #[arg(long)]
        workflow: PathBuf,
        #[arg(long)]
        config: PathBuf,
    },
    /// Run a workflow against a working directory.
    Run {
        #[arg(long)]
        workflow: PathBuf,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        workdir: PathBuf,
        #[arg(long)]
        input: String,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Capability file fully replacing the runtime's built-in sandbox default.
        /// Overrides `sandbox.capabilities_file` in the config.
        #[arg(long)]
        capabilities: Option<PathBuf>,
    },
    /// Resume a suspended run, injecting a reply.
    Resume {
        #[arg(long = "run")]
        run_id: String,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        message: String,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

/// Locate the sibling `october-runtime` binary next to this executable.
fn runtime_binary_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("october-runtime")))
        .unwrap_or_else(|| PathBuf::from("october-runtime"))
}

fn do_validate(workflow: PathBuf, config: PathBuf) -> i32 {
    let cfg = match OctoberConfig::load(&config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let text = match std::fs::read_to_string(&workflow) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to read workflow: {e}");
            return 2;
        }
    };
    let def: WorkflowDefinition = match serde_json::from_str(&text) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("workflow parse error: {e}");
            return 2;
        }
    };
    let errs = validate(&def, &cfg);
    if errs.is_empty() {
        println!("valid");
        0
    } else {
        for e in &errs {
            eprintln!("✗ {e}");
        }
        1
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Validate { workflow, config } => do_validate(workflow, config),
        Command::Run {
            workflow,
            config,
            workdir,
            input,
            state_dir,
            capabilities,
        } => match run(RunParams {
            workflow_path: workflow,
            config_path: config,
            workdir,
            input,
            state_dir,
            runtime_bin: runtime_binary_path(),
            capabilities_path: capabilities,
        })
        .await
        {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        Command::Resume {
            run_id,
            config,
            message,
            state_dir,
        } => match resume(ResumeParams {
            run_id,
            config_path: config,
            state_dir,
            message,
            runtime_bin: runtime_binary_path(),
        })
        .await
        {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
    };
    std::process::exit(code);
}
