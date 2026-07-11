//! `horsie serve`: the standalone session server (HTTP + SSE).
//!
//! Shares config, providers, and capability resolution with the daemon, but owns
//! a separate journal root (`<data_dir>/server`) and state root
//! (`<state_dir>/server`), so it runs alongside the job daemon untouched.

use crate::capabilities;
use crate::config::{HorsieConfig, build_registry};
use crate::daemon::default_runtime_bin;
use crate::error::CliError;
use horsie_actor::{FileJournal, Journal, spawn_root};
use horsie_models::capabilities::CapabilitySpec;
use horsie_server::http::{AppState, app};
use horsie_server::sessions::spec::ServerDeps;
use horsie_server::sessions::supervisor::SessionSupervisor;
use horsie_server::velos::VelosClient;
use horsie_server::vendor::{LocalProcessVendor, RuntimeVendor, VelosVendor, VelosVendorSettings};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub async fn serve(
    cfg: HorsieConfig,
    addr: String,
    web_dir: Option<PathBuf>,
) -> Result<(), CliError> {
    let state_dir = cfg.storage.state_dir.join("server");
    let data_dir = cfg.storage.data_dir.join("server");
    std::fs::create_dir_all(&state_dir).map_err(|e| CliError::Io(e.to_string()))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| CliError::Io(e.to_string()))?;

    let registry = build_registry(&cfg)?;
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(data_dir));
    let runtime_bin = cfg.runtime.bin.clone().unwrap_or_else(default_runtime_bin);

    // Resolve the default capability spec once, and a finalizer that applies path
    // expansion + plugin grants + platform seatbelt rules to request-supplied specs.
    let default_caps = match &cfg.sandbox.capabilities_file {
        Some(path) => CapabilitySpec::load(path).map_err(CliError::Config)?,
        None => capabilities::builtin_default()?,
    };
    let plugins_dir = crate::plugins::plugins_dir_if_populated(&cfg.storage.plugins_dir);
    let hook_path = if plugins_dir.is_some() {
        crate::plugins::resolve_hook_path(cfg.runtime.hook_path.clone())
    } else {
        Vec::new()
    };
    let (pd, hp) = (plugins_dir.clone(), hook_path.clone());
    let caps_finalize: Arc<dyn Fn(CapabilitySpec) -> CapabilitySpec + Send + Sync> =
        Arc::new(move |caps| {
            capabilities::with_default_seatbelt_rules(capabilities::with_plugin_grants(
                capabilities::resolve_user_paths(caps),
                pd.as_deref(),
                &hp,
            ))
        });
    let default_caps = caps_finalize(default_caps);

    let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
    vendors.insert(
        "local".into(),
        Arc::new(LocalProcessVendor::new(runtime_bin)),
    );

    // Optional velos remote runtime vendor. Present → sessions may target
    // `"vendor": "velos"` to run in a remote container; absent → local only.
    if let Some(vc) = &cfg.velos {
        let vendor = build_velos_vendor(vc).await?;
        println!(
            "velos remote runtime vendor enabled (server {}, image {})",
            vc.server_url, vc.image
        );
        vendors.insert("velos".into(), Arc::new(vendor));
    }

    // The vendor a create request defaults to when it omits `vendor`.
    let default_vendor = cfg.default_vendor.clone().unwrap_or_else(|| "local".into());
    if !vendors.contains_key(&default_vendor) {
        return Err(CliError::Config(format!(
            "default_vendor '{default_vendor}' is not a configured vendor \
             (available: {})",
            available_vendors(&vendors)
        )));
    }

    let deps = ServerDeps {
        provider_registry: registry,
        vendors,
        state_dir,
    };
    let (global_tx, _) = tokio::sync::broadcast::channel(256);
    let supervisor = spawn_root(
        SessionSupervisor::new(deps, global_tx.clone()),
        journal.clone(),
    );

    let state = AppState {
        supervisor,
        journal,
        global_events: global_tx,
        caps_finalize,
        default_caps,
        plugins_dir,
        hook_path,
        default_vendor,
        web_dir,
    };
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| CliError::Executor(format!("bind {addr}: {e}")))?;
    println!("horsie server listening on http://{addr}");
    if let Some(dir) = state.web_dir.as_ref() {
        println!("serving web UI from {}", dir.display());
    }
    axum::serve(listener, app(state))
        .await
        .map_err(|e| CliError::Executor(e.to_string()))
}

/// Build the velos vendor from config: resolve the bearer token, construct the
/// REST client, and bind the shared reverse-dial listener.
async fn build_velos_vendor(
    vc: &crate::config::VelosVendorConfig,
) -> Result<VelosVendor, CliError> {
    let token = vc.resolve_token()?;
    let client = VelosClient::new(&vc.server_url, token)
        .map_err(|e| CliError::Config(format!("velos client: {e}")))?;
    let listen: SocketAddr = vc
        .listen
        .parse()
        .map_err(|e| CliError::Config(format!("invalid velos listen '{}': {e}", vc.listen)))?;
    let settings = VelosVendorSettings {
        image: vc.image.clone(),
        runtime_bin: vc.runtime_bin.clone(),
        workspace_root: vc.workspace_root.clone(),
        advertise_host: vc.advertise_host.clone(),
        listen,
        cpu: vc.cpu,
        memory_bytes: vc.memory_bytes(),
        connect_timeout: Duration::from_secs(vc.connect_timeout_secs),
    };
    VelosVendor::bind(Arc::new(client), settings)
        .await
        .map_err(|e| CliError::Executor(format!("velos vendor: {e}")))
}

/// Comma-separated sorted vendor names, for error messages.
fn available_vendors(vendors: &HashMap<String, Arc<dyn RuntimeVendor>>) -> String {
    let mut names: Vec<&str> = vendors.keys().map(String::as_str).collect();
    names.sort_unstable();
    names.join(", ")
}
