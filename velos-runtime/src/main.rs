//! `horsie-velos-runtime` — a runtime vendor agent backed by velos containers.
//!
//! It dials a session server's `/api/vendor/connect` and serves runtimes by
//! scheduling one velos container each. The server holds no velos credentials:
//! the URL and token live here, in this process's own configuration.
//!
//! Containers publish no inbound ports, so each container's `horsie-runtime`
//! dials *back* to this agent's `--advertise` address. That address must be
//! routable from velos's container network to wherever this agent runs.

mod provider;
mod velos;

use clap::Parser;
use horsie_runtime_vendor::{
    ConnectedRuntimeRegistry, RuntimeEndpoint, RuntimeListenerServer, RuntimeVendor,
    serve_runtime_connections,
};
use provider::{ManagedWorkspaces, VelosContainerProvider, VelosProviderSettings};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "horsie-velos-runtime",
    about = "Serve horsie sessions with velos-scheduled container runtimes"
)]
struct Cli {
    /// `http(s)://host:port` of the session server to dial.
    #[arg(long)]
    server: String,
    /// Vendor name the server publishes this agent under.
    #[arg(long, default_value = "velos")]
    name: String,
    /// velos control-plane URL, e.g. `http://velos.example:8080`.
    #[arg(long)]
    velos_url: String,
    /// velos API token. Prefer `HORSIE_VELOS_TOKEN` over passing it on argv.
    #[arg(long, env = "HORSIE_VELOS_TOKEN")]
    velos_token: String,
    /// `host:port` this agent's runtime listener is reachable at *from velos's
    /// container network*.
    #[arg(long)]
    advertise: String,
    /// Address to bind the runtime listener on.
    #[arg(long, default_value = "0.0.0.0:3790")]
    listen: SocketAddr,
    /// OCI image bundling `horsie-runtime`.
    #[arg(long)]
    image: String,
    /// Path to `horsie-runtime` inside the image.
    #[arg(long, default_value = "horsie-runtime")]
    runtime_bin: String,
    /// In-container root each workspace is allocated under.
    #[arg(long, default_value = "/workspace")]
    workspace_root: String,
    #[arg(long, default_value_t = 2)]
    cpu: u32,
    /// Memory per runtime, in MiB.
    #[arg(long, default_value_t = 2048)]
    memory_mib: u64,
    /// How long to wait for a scheduled container's runtime to dial back.
    #[arg(long, default_value_t = 60)]
    connect_timeout_secs: u64,
    /// Scratch directory for per-runtime state.
    #[arg(long, default_value = "/var/lib/horsie-velos-runtime")]
    state_dir: PathBuf,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("horsie-velos-runtime: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Translate the server URL into the agent connect endpoint.
fn server_to_endpoint(server: &str) -> Result<String, String> {
    let (scheme, rest) = server
        .split_once("://")
        .ok_or_else(|| format!("--server must be a URL, got '{server}'"))?;
    let ws_scheme = match scheme {
        "http" => "ws",
        "https" => "wss",
        other => {
            return Err(format!(
                "--server must be http:// or https://, got '{other}://'"
            ));
        }
    };
    Ok(format!(
        "{ws_scheme}://{}/api/vendor/connect",
        rest.trim_end_matches('/')
    ))
}

async fn run(cli: Cli) -> Result<(), String> {
    let endpoint = server_to_endpoint(&cli.server)?;

    // Bound once, for the whole life of the process: the agent reconnects to
    // the server without disturbing this listener. Rebinding it per connection
    // would risk "address in use" and would drop every container currently
    // dialed into it, none of which the server hanging up has any bearing on.
    let connected = Arc::new(ConnectedRuntimeRegistry::new());
    let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Tcp(cli.listen))
        .await
        .map_err(|e| format!("bind runtime listener on {}: {e}", cli.listen))?;
    let cancel = CancellationToken::new();
    serve_runtime_connections(listener, connected.clone(), cancel.clone());

    let api = Arc::new(
        velos::VelosClient::new(
            cli.velos_url.clone(),
            Some(horsie_agentcore::Secret::from(cli.velos_token)),
        )
        .map_err(|e| format!("velos client: {e}"))?,
    );
    // Fail fast on an unreachable server or a bad token, rather than letting
    // the first session discover it as a provisioning error.
    match api.whoami().await {
        Ok(identity) => println!("velos: authenticated as {identity}"),
        Err(e) => return Err(format!("velos {}: {e}", cli.velos_url)),
    }

    let advertise_ws = format!("ws://{}", cli.advertise.trim_end_matches('/'));
    let settings = VelosProviderSettings {
        image: cli.image.clone(),
        runtime_bin: cli.runtime_bin.clone(),
        advertise_ws: advertise_ws.clone(),
        cpu: cli.cpu,
        memory_bytes: cli.memory_mib.saturating_mul(1024 * 1024),
        connect_timeout: Duration::from_secs(cli.connect_timeout_secs),
    };

    // One provider serves every runtime: unlike the local agent, there is no
    // per-runtime sandbox file to bind — the container is the boundary.
    let shared: Arc<dyn horsie_runtime_vendor::RuntimeProvider> = Arc::new(
        VelosContainerProvider::new(api, connected.clone(), settings),
    );
    let provider: horsie_runtime_vendor::ProviderFactory = {
        let shared = shared.clone();
        Arc::new(move |_runtime_id: &str, _caps: Option<PathBuf>| shared.clone())
    };

    let agent = RuntimeVendor::new(
        cli.name.clone(),
        // velos allocates a fresh workspace per runtime: repos and bundles can
        // be provisioned into it.
        true,
        provider,
        connected,
        Arc::new(ManagedWorkspaces::new(cli.workspace_root.clone())),
        cli.state_dir.clone(),
    )
    .with_bundles(horsie_runtime_vendor::BundleDelivery {
        // Containers reach the server the same way they reach us: over the
        // advertise address, which is routable from velos's container network.
        base_url: format!("http://{}", cli.advertise.trim_end_matches('/')),
        // A fixed in-container path: the container is ephemeral and isolated,
        // so one dir per runtime buys nothing, and there is nothing to cache
        // across containers.
        dir: "/horsie/plugins".to_string(),
        cache_dir: None,
    });

    println!(
        "connected to {} as vendor \"{}\" · velos {} · containers dial back to {}",
        cli.server, cli.name, cli.velos_url, advertise_ws
    );

    // The signal cancels rather than races the agent: `run` reconnects on its
    // own until the token fires, and only then stops the containers it
    // scheduled. Selecting against it would drop that shutdown on the floor.
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        signal_cancel.cancel();
    });

    agent.run(&endpoint, cancel.clone()).await
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut sigint) = signal(SignalKind::interrupt()) else {
        return;
    };
    let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
        return;
    };
    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn server_url_becomes_the_vendor_connect_endpoint() {
        assert_eq!(
            server_to_endpoint("http://localhost:3789").unwrap(),
            "ws://localhost:3789/api/vendor/connect"
        );
        assert_eq!(
            server_to_endpoint("https://horsie.example.com/").unwrap(),
            "wss://horsie.example.com/api/vendor/connect"
        );
        assert!(server_to_endpoint("localhost:3789").is_err());
    }
}
