//! A runtime vendor backed by Fly Machines.
//!
//! Everything Fly-shaped is behind [`FlyApi`], so this vendor's logic — the
//! ordering, the naming, the failure taxonomy — is testable without a network.
//! The REST client that implements it is the only part that talks HTTP.
//!
//! **Naming is the whole recovery story.** A machine is named
//! `horsie-{runtime_id}`, so nothing about a runtime has to be written down:
//! after a server restart the vendor finds a runtime's machine by asking Fly,
//! and `create` is idempotent against a machine that already exists. That is
//! what lets `RuntimeManager` hold no durable state and still recover.
//!
//! **A machine is not a runtime.** Fly reports `started` when the VM is up; the
//! runtime inside it still has to boot, run its provision steps, and dial back.
//! Only the dial-back means `Ready`, which is why `create` returns `Starting`
//! and finishes on the progress sink.

use crate::runtime_vendor::runtime_command::{build_runtime_command, workspace_paths};
use crate::runtime_vendor::{RuntimeVendor, RuntimeVendorError};
use async_trait::async_trait;
use horsie_models::runtime_vendor::{RuntimeSpec, RuntimeVendorCapabilities};
use horsie_runtime_host::{
    RuntimeEvent, RuntimeProgress, RuntimeProgressSink,
};
use std::sync::Arc;
use std::time::Duration;

/// How long a runtime has to boot, provision and dial back before its
/// acquisition is written off.
///
/// Matches the vendor protocol's request ceiling rather than a typical HTTP
/// timeout: a create with `git_checkout` steps legitimately runs for minutes.
const READY_WINDOW: Duration = Duration::from_secs(900);

/// How long the settings check waits for Fly before calling it unreachable.
///
/// Short on purpose: it runs inside a settings save, and a save that hangs on a
/// slow cloud API is a form the operator cannot tell from a broken button.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

/// The machine name for a runtime. Deterministic on purpose — see the module
/// docs.
#[must_use]
pub fn machine_name(runtime_id: &str) -> String {
    format!("horsie-{runtime_id}")
}

/// The volume name for a runtime.
///
/// Fly's two name rules differ: a machine name may contain dashes, a volume
/// name may not — volumes accept only lowercase letters, digits and
/// underscores, up to 30 characters. So this cannot be [`machine_name`].
///
/// Truncation can make two runtimes share a name, and that is harmless: a Fly
/// volume name is a *group* label, not an identifier. Every mount here is by
/// the id the create call returns.
#[must_use]
pub fn volume_name(runtime_id: &str) -> String {
    let mut name = String::from("horsie_");
    for c in runtime_id.chars() {
        name.push(if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '_'
        });
    }
    name.truncate(30);
    name
}

/// The runtime id a machine name was built from, or `None` when the machine is
/// not one of ours. The inverse of [`machine_name`], and the reason the sweep
/// can be sure it is only ever looking at horsie's own machines: an app shared
/// with anything else keeps its other machines.
#[must_use]
pub fn runtime_id_of(machine_name: &str) -> Option<&str> {
    machine_name
        .strip_prefix("horsie-")
        .filter(|s| !s.is_empty())
}

#[derive(Debug, thiserror::Error)]
pub enum FlyError {
    /// The API answered, and the answer was no.
    #[error("fly rejected the request: {0}")]
    Rejected(String),
    /// The API could not be reached, or answered 5xx.
    #[error("fly is unreachable: {0}")]
    Unreachable(String),
}

impl From<FlyError> for RuntimeVendorError {
    fn from(e: FlyError) -> Self {
        match e {
            // A rejected request is a provisioning failure the session may
            // retry; an unreachable API is not the session's fault at all.
            FlyError::Rejected(m) => Self::Provision(m),
            FlyError::Unreachable(m) => Self::Unavailable(m),
        }
    }
}

/// A Fly machine, as much of one as this vendor cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub id: String,
    pub state: MachineState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineState {
    Started,
    Stopped,
    Suspended,
    /// Anything transitional or unknown — `created`, `starting`, `replacing`
    /// and whatever Fly adds next. Treated as "not usable yet", never as gone
    /// (guessing a machine away would destroy a workspace) and never as
    /// startable: only [`Stopped`](Self::Stopped) and
    /// [`Suspended`](Self::Suspended) are states Fly will start from.
    Other,
}

/// What a machine needs to exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSpec {
    pub name: String,
    pub image: String,
    pub region: String,
    /// `/bin/sh -c "…"`, as `build_container_command` already emits for velos.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Volume id to mount, and where. `None` means an ephemeral machine whose
    /// workspace does not survive a stop.
    pub mount: Option<(String, String)>,
}

/// The Fly Machines calls this vendor makes. One method per REST call, so a
/// test double is a `HashMap` rather than an HTTP server.
#[async_trait]
pub trait FlyApi: Send + Sync {
    /// Create a volume, returning its id. Fly rejects attaching one after a
    /// machine exists, so this always precedes [`Self::create_machine`].
    async fn create_volume(&self, name: &str, region: &str) -> Result<String, FlyError>;
    async fn create_machine(&self, spec: &MachineSpec) -> Result<String, FlyError>;
    /// The machine with this name, or `None` if there is none.
    async fn machine_by_name(&self, name: &str) -> Result<Option<Machine>, FlyError>;
    /// Every machine in the app, with its name. One call, not one per machine:
    /// Fly rate-limits per-machine polling, and the orphan sweep is the only
    /// caller that needs the whole inventory.
    async fn machines(&self) -> Result<Vec<(String, Machine)>, FlyError>;
    async fn start(&self, machine_id: &str) -> Result<(), FlyError>;
    async fn stop(&self, machine_id: &str) -> Result<(), FlyError>;
    /// Destroy the machine. Idempotent: a machine that is already gone is
    /// success, because delete is what the caller wanted either way.
    ///
    /// Deliberately *not* its volume. Fly does not cascade, and the caller has
    /// to delete volumes separately — see [`Self::delete_volume`].
    async fn destroy(&self, machine_id: &str) -> Result<(), FlyError>;
    /// Every volume in the app, as `(id, name)`.
    ///
    /// By name rather than by machine, because a volume outlives the machine
    /// that mounted it and can outlive one that was never created at all: a
    /// create that made the volume and then failed to make the machine leaves
    /// one nothing else names.
    async fn volumes(&self) -> Result<Vec<(String, String)>, FlyError>;
    /// Destroy a volume. Idempotent, like [`Self::destroy`].
    async fn delete_volume(&self, volume_id: &str) -> Result<(), FlyError>;
}

/// How this vendor builds machines.
pub struct FlySettings {
    /// OCI image with `horsie-runtime` baked in.
    pub image: String,
    pub region: String,
    /// Where in the machine workspaces are allocated.
    pub workspace_root: String,
    /// The URL a machine reaches this server on. A deployment only reachable on
    /// localhost cannot use this vendor at all — the string ends up in the
    /// machine's argv.
    pub callback_url: String,
    /// Give each runtime a volume, so a stopped one keeps its workspace.
    /// Without it a stop destroys work and revival is a lie.
    pub volumes: bool,
}

pub struct FlyRuntimeVendor<A: FlyApi> {
    name: String,
    api: A,
    settings: FlySettings,
}

impl<A: FlyApi> FlyRuntimeVendor<A> {
    pub fn new(
        name: String,
        api: A,
        settings: FlySettings,
        ) -> Self {
        Self {
            name,
            api,
            settings,
        }
    }

    /// Delete every volume this runtime's creates have left behind.
    ///
    /// By name, and *all* of them: a Fly volume name is a group label rather
    /// than an identifier, so a runtime rebuilt after a half-failed create has
    /// more than one volume under the same name, and only the last is mounted.
    async fn delete_volumes_of(&self, runtime_id: &str) -> Result<(), RuntimeVendorError> {
        let wanted = volume_name(runtime_id);
        for (id, name) in self.api.volumes().await? {
            if name == wanted {
                self.api.delete_volume(&id).await?;
            }
        }
        Ok(())
    }

    /// Delete every horsie volume whose runtime no longer exists.
    ///
    /// Compares against the names the *live* runtimes would have rather than
    /// inverting a volume name, which cannot be done: [`volume_name`] lowercases,
    /// substitutes and truncates. Two runtimes can therefore share a name, and
    /// the comparison is deliberately the safe way round — a name any live
    /// runtime would use is kept, so a collision costs a leaked volume rather
    /// than a destroyed workspace.
    async fn sweep_volumes(
        &self,
        live: &std::collections::HashSet<String>,
    ) -> Result<(), FlyError> {
        let live_names: std::collections::HashSet<String> =
            live.iter().map(|id| volume_name(id)).collect();
        for (id, name) in self.api.volumes().await? {
            if name.starts_with("horsie_") && !live_names.contains(&name) {
                self.api.delete_volume(&id).await?;
            }
        }
        Ok(())
    }

    fn spec_for(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        volume: Option<String>,
    ) -> Result<MachineSpec, RuntimeVendorError> {
        let workspaces = workspace_paths(&self.settings.workspace_root, &spec.workspaces)
            .map_err(RuntimeVendorError::Provision)?;
        let mut env: Vec<(String, String)> = spec
            .env
            .iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect();
        // The dial token is already in `spec.env`, minted by the server. It
        // rides the environment and never argv, which argv-readable `ps` is the
        // reason for; copying `spec.env` wholesale is what carries it.
        //
        // Where this runtime reaches the server, and where it unpacks bundles.
        // Both are the *vendor's* knowledge — the server cannot know the
        // address a machine of ours can route to, and it does not know this
        // image's filesystem. Neither was supplied before, which is why bundles
        // never worked on this vendor at all.
        env.push((
            horsie_models::ENV_SERVER_URL.to_string(),
            crate::runtime_vendor::server_url::http_base_of(&self.settings.callback_url),
        ));
        env.push((
            horsie_models::ENV_PLUGINS_DIR.to_string(),
            format!(
                "{}/.horsie-plugins",
                self.settings.workspace_root.trim_end_matches('/')
            ),
        ));
        // Provision steps travel the same channel the process provider uses, so
        // the runtime binary needs no Fly-specific path. Encoding cannot fail
        // for this type, and a machine with no provision steps is a working
        // machine — so a failure here drops the steps rather than the runtime.
        if !spec.provision.is_empty()
            && let Ok(json) = serde_json::to_string(&spec.provision)
        {
            env.push((horsie_models::ENV_PROVISION.to_string(), json));
        }
        Ok(MachineSpec {
            name: machine_name(runtime_id),
            image: self.settings.image.clone(),
            region: self.settings.region.clone(),
            command: build_runtime_command(
                "horsie-runtime",
                &self.settings.callback_url,
                runtime_id,
                &workspaces,
            ),
            env,
            mount: volume.map(|id| (id, self.settings.workspace_root.clone())),
        })
    }
}

#[async_trait]
impl<A: FlyApi> RuntimeVendor for FlyRuntimeVendor<A> {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> RuntimeVendorCapabilities {
        RuntimeVendorCapabilities {
            supports_provisioning: true,
        }
    }

    /// Lists the app's machines. The cheapest call Fly has that needs both
    /// halves of a configuration to be right: the token authenticates it and
    /// the app is in the path, so a bad token is a 401 and an app that does not
    /// exist is a 404 — the two mistakes that otherwise wait for the first
    /// session to make them.
    ///
    /// Bounded by its own deadline. Every other call this vendor makes is
    /// inside a session that already has one; this one is inside a settings
    /// save, where a hung request is a form that never comes back.
    async fn preflight(&self) -> Result<(), RuntimeVendorError> {
        match tokio::time::timeout(PREFLIGHT_TIMEOUT, self.api.machines()).await {
            Ok(Ok(_)) => Ok(()),
            // Named for what an operator can act on. Fly's own message says
            // which of the two it is, and says it in fly's words.
            Ok(Err(FlyError::Rejected(m))) => Err(RuntimeVendorError::Provision(format!(
                "fly refused to list the app's machines ({m}) — check the API token, and that the app already exists"
            ))),
            Ok(Err(e @ FlyError::Unreachable(_))) => Err(e.into()),
            Err(_elapsed) => Err(RuntimeVendorError::Unavailable(format!(
                "fly did not answer within {}s",
                PREFLIGHT_TIMEOUT.as_secs()
            ))),
        }
    }

    async fn create(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        let volume = if self.settings.volumes {
            Some(
                self.api
                    .create_volume(&volume_name(runtime_id), &self.settings.region)
                    .await?,
            )
        } else {
            None
        };
        self.api
            .create_machine(&self.spec_for(runtime_id, spec, volume)?)
            .await?;

        // Nothing is waited on here. Whether the runtime came up is the
        // runtime's own report, announced on its out topic, and the acquiring
        // node is subscribed to it — this vendor's job ends once the substrate
        // has accepted the machine.
        let _ = progress;
        Ok(RuntimeProgress::Starting {
            detail: "the machine is booting".to_string(),
        })
    }

    async fn get(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        provisioning: bool,
        progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        let Some(machine) = self.api.machine_by_name(&machine_name(runtime_id)).await? else {
            // Terminal, and deliberately not a create: rebuilding here would
            // silently replace a workspace the user believes still holds work.
            return Err(RuntimeVendorError::Gone(format!(
                "no machine for runtime '{runtime_id}'"
            )));
        };

        // A started machine is a *live* runtime, always. The runtime is PID 1
        // under `restart: no`, so a machine outlives its runtime process by
        // nothing — and the runtime now re-dials rather than exiting when its
        // link drops. So "started but not connected" means it is mid-retry
        // (this server restarted, or the network blinked), and the only correct
        // thing to do is wait. Bouncing it here would kill a runtime that was
        // seconds from reconnecting, and take its in-flight work with it.
        let detail = match machine.state {
            MachineState::Started => "the machine is up; waiting for it to dial back",
            // `stopped` and `suspended` are the only two states Fly will start
            // from, and they are exactly the two a hibernate leaves behind.
            MachineState::Stopped | MachineState::Suspended => {
                self.api.start(&machine.id).await?;
                "the machine is resuming"
            }
            // Everything else is already moving under its own power. A fresh
            // machine sits in `created` for about six seconds before `started`,
            // and starting one is a `412 failed_precondition` — which is what
            // made the first turn of every new session fail. So honour what
            // `Other` has always claimed to mean and wait. The wait is bounded
            // by `READY_WINDOW`, so a state that never becomes a live runtime
            // still resolves, as "never dialed back" rather than as a start Fly
            // was always going to reject.
            MachineState::Other => "the machine is still coming up",
        };
        // Unused: a Fly machine keeps its volume across a hibernate, so
        // resuming one needs nothing rebuilt from the spec.
        let _ = spec;

        let _ = progress;
        Ok(RuntimeProgress::Starting {
            detail: detail.to_string(),
        })
    }

    async fn sweep_orphans(
        &self,
        live: &std::collections::HashSet<String>,
    ) -> Result<Vec<String>, RuntimeVendorError> {
        let mut swept = Vec::new();
        // One stuck object must not shield every other from the sweep. The
        // whole point of this pass is that things have already been billing for
        // longer than they should, and aborting on the first failure — while
        // discarding the ids already swept — meant one undeletable machine kept
        // the rest alive indefinitely.
        let mut failure = None;
        for (name, machine) in self.api.machines().await? {
            // Two filters, and both matter. The prefix means a shared app keeps
            // its other machines; the `live` check means a machine whose
            // session still exists is never touched, however long it has been
            // stopped — a hibernated runtime looks exactly like an orphan from
            // the substrate alone.
            let Some(runtime_id) = runtime_id_of(&name) else {
                continue;
            };
            if live.contains(runtime_id) {
                continue;
            }
            match self.api.destroy(&machine.id).await {
                Ok(()) => {
                    swept.push(runtime_id.to_string());
                }
                Err(e) => failure = Some(e),
            }
        }

        // Volumes are swept by name, not through the machines above: Fly does
        // not cascade a delete, and a volume can outlive every machine that
        // ever referenced it — including one whose machine create failed after
        // the volume was made. Nothing else would ever name that volume again.
        match self.sweep_volumes(live).await {
            Ok(()) => {}
            Err(e) => failure = Some(e),
        }

        match failure {
            // The ids that *were* swept are the return value either way; the
            // error says the pass was incomplete, not that it did nothing.
            Some(e) if swept.is_empty() => Err(e.into()),
            Some(e) => {
                tracing::warn!(error = %e, swept = swept.len(), "the orphan sweep was incomplete");
                Ok(swept)
            }
            None => Ok(swept),
        }
    }

    async fn hibernate(
        &self,
        runtime_id: &str,
        _progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        let Some(machine) = self.api.machine_by_name(&machine_name(runtime_id)).await? else {
            return Ok(RuntimeProgress::Stopped);
        };
        self.api.stop(&machine.id).await?;
        Ok(RuntimeProgress::Stopped)
    }

    async fn delete(
        &self,
        runtime_id: &str,
        _progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        if let Some(machine) = self.api.machine_by_name(&machine_name(runtime_id)).await? {
            self.api.destroy(&machine.id).await?;
        }
        // After the machine, never before: Fly refuses to delete a volume that
        // is still attached. And never skipped because the machine was already
        // gone — a volume survives its machine, and one nobody deletes bills
        // for its full size forever.
        self.delete_volumes_of(runtime_id).await?;
        Ok(RuntimeProgress::Gone {
            reason: "the owning session was deleted".to_string(),
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeFly {
        machines: Mutex<Vec<(String, Machine)>>,
        calls: Mutex<Vec<String>>,
        volumes: Mutex<Vec<String>>,
        reject_create: bool,
        /// The app or the token is wrong, so listing machines is a 401/404 —
        /// what a settings check is there to catch.
        reject_list: bool,
        unreachable: bool,
        /// Machine ids whose destroy fails, so a test can hold one object stuck
        /// and watch what the sweep does with the rest.
        undeletable: Vec<String>,
    }

    impl FakeFly {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn volume_names(&self) -> Vec<String> {
            self.volumes.lock().unwrap().clone()
        }
        fn with_volume(self, runtime_id: &str) -> Self {
            self.volumes.lock().unwrap().push(volume_name(runtime_id));
            self
        }
        fn with_machine(self, runtime_id: &str, state: MachineState) -> Self {
            self.machines.lock().unwrap().push((
                machine_name(runtime_id),
                Machine {
                    id: format!("m-{runtime_id}"),
                    state,
                },
            ));
            self
        }
    }

    #[async_trait]
    impl FlyApi for FakeFly {
        async fn create_volume(&self, name: &str, _: &str) -> Result<String, FlyError> {
            self.calls.lock().unwrap().push(format!("volume:{name}"));
            self.volumes.lock().unwrap().push(name.to_string());
            Ok(format!("vol-{name}"))
        }
        async fn create_machine(&self, spec: &MachineSpec) -> Result<String, FlyError> {
            if self.unreachable {
                return Err(FlyError::Unreachable("no route".to_string()));
            }
            if self.reject_create {
                return Err(FlyError::Rejected("no capacity".to_string()));
            }
            self.calls
                .lock()
                .unwrap()
                .push(format!("machine:{}", spec.name));
            self.machines.lock().unwrap().push((
                spec.name.clone(),
                Machine {
                    id: format!("m-{}", spec.name),
                    state: MachineState::Started,
                },
            ));
            Ok(format!("m-{}", spec.name))
        }
        async fn machine_by_name(&self, name: &str) -> Result<Option<Machine>, FlyError> {
            Ok(self
                .machines
                .lock()
                .unwrap()
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, m)| m.clone()))
        }
        async fn machines(&self) -> Result<Vec<(String, Machine)>, FlyError> {
            if self.unreachable {
                return Err(FlyError::Unreachable("no route".to_string()));
            }
            if self.reject_list {
                return Err(FlyError::Rejected("401: unauthorized".to_string()));
            }
            Ok(self.machines.lock().unwrap().clone())
        }
        async fn start(&self, id: &str) -> Result<(), FlyError> {
            self.calls.lock().unwrap().push(format!("start:{id}"));
            Ok(())
        }
        async fn stop(&self, id: &str) -> Result<(), FlyError> {
            self.calls.lock().unwrap().push(format!("stop:{id}"));
            Ok(())
        }
        async fn destroy(&self, id: &str) -> Result<(), FlyError> {
            self.calls.lock().unwrap().push(format!("destroy:{id}"));
            if self.undeletable.iter().any(|u| u == id) {
                return Err(FlyError::Rejected("machine is busy".to_string()));
            }
            self.machines.lock().unwrap().retain(|(_, m)| m.id != id);
            Ok(())
        }
        async fn volumes(&self) -> Result<Vec<(String, String)>, FlyError> {
            Ok(self
                .volumes
                .lock()
                .unwrap()
                .iter()
                .map(|name| (format!("vol-{name}"), name.clone()))
                .collect())
        }
        async fn delete_volume(&self, id: &str) -> Result<(), FlyError> {
            self.calls.lock().unwrap().push(format!("rm-volume:{id}"));
            self.volumes
                .lock()
                .unwrap()
                .retain(|name| format!("vol-{name}") != id);
            Ok(())
        }
    }

    fn settings(volumes: bool) -> FlySettings {
        FlySettings {
            image: "registry.fly.io/horsie:latest".to_string(),
            region: "iad".to_string(),
            workspace_root: "/workspaces".to_string(),
            callback_url: "wss://horsie.example.com/api/runtime/connect".to_string(),
            volumes,
        }
    }

    fn vendor(
        api: FakeFly,
        volumes: bool,
    ) -> FlyRuntimeVendor<FakeFly> {
        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        (
            FlyRuntimeVendor::new(
                "fly-iad".to_string(),
                api,
                settings(volumes),
                connected.clone(),
            ),
        )
    }

    fn spec() -> RuntimeSpec {
        RuntimeSpec {
            workspaces: vec!["main".to_string()],
            env: vec![],
            provision: vec![],
        }
    }

    fn sink() -> (
        RuntimeProgressSink,
        tokio::sync::mpsc::Receiver<RuntimeEvent>,
    ) {
        tokio::sync::mpsc::channel(8)
    }

    #[tokio::test]
    async fn create_returns_starting_because_a_machine_is_not_a_runtime() {
        // Fly reports `started` when the VM is up; the runtime inside still has
        // to boot, provision and dial back. Only the dial-back is Ready.
        let (v, _reg) = vendor(FakeFly::default(), false);
        let (tx, _rx) = sink();
        let progress = v.create("s1", &spec(), tx).await.unwrap();
        assert!(matches!(progress, RuntimeProgress::Starting { .. }));
    }

    #[tokio::test]
    async fn a_volume_is_created_before_its_machine() {
        // Fly rejects attaching a volume to an existing machine, so the order
        // is not a preference.
        let (v, _reg) = vendor(FakeFly::default(), true);
        let (tx, _rx) = sink();
        v.create("s1", &spec(), tx).await.unwrap();
        assert_eq!(
            v.api.calls(),
            vec![
                "volume:horsie_s1".to_string(),
                "machine:horsie-s1".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn a_sweep_destroys_only_machines_whose_session_is_gone() {
        let (v, _reg) = vendor(
            FakeFly::default()
                .with_machine("s1", MachineState::Started)
                .with_machine("s2", MachineState::Stopped),
            false,
        );
        let live = std::collections::HashSet::from(["s1".to_string()]);
        assert_eq!(
            v.sweep_orphans(&live).await.unwrap(),
            vec!["s2".to_string()]
        );
        assert_eq!(v.api.calls(), vec!["destroy:m-s2".to_string()]);
    }

    #[tokio::test]
    async fn a_sweep_never_touches_a_machine_that_is_not_ours() {
        // The app may be shared. A prefix is the only thing separating horsie's
        // machines from someone else's, and destroying is not recoverable.
        let fake = FakeFly::default();
        fake.machines.lock().unwrap().push((
            "postgres".to_string(),
            Machine {
                id: "m-pg".to_string(),
                state: MachineState::Started,
            },
        ));
        let (v, _reg) = vendor(fake, false);
        let live = std::collections::HashSet::new();
        assert!(v.sweep_orphans(&live).await.unwrap().is_empty());
        assert!(v.api.calls().is_empty(), "got {:?}", v.api.calls());
    }

    #[tokio::test]
    async fn a_sweep_keeps_a_hibernated_runtime() {
        // The case that makes a naive sweep destructive: a stopped machine and
        // an orphan are indistinguishable on the substrate. Only the server's
        // own list of sessions tells them apart.
        let (v, _reg) = vendor(
            FakeFly::default().with_machine("s1", MachineState::Stopped),
            false,
        );
        let live = std::collections::HashSet::from(["s1".to_string()]);
        assert!(v.sweep_orphans(&live).await.unwrap().is_empty());
        assert!(v.api.calls().is_empty(), "got {:?}", v.api.calls());
    }

    /// Fly does not cascade. Deleting only the machine left a volume behind
    /// billing at its full size, for every runtime, forever.
    #[tokio::test]
    async fn deleting_a_runtime_deletes_its_volume_too() {
        let (v, _reg) = vendor(
            FakeFly::default()
                .with_machine("s1", MachineState::Stopped)
                .with_volume("s1"),
            true,
        );
        let (tx, _rx) = sink();
        v.delete("s1", tx).await.unwrap();
        assert!(
            v.api.volume_names().is_empty(),
            "the volume outlived its machine: {:?}",
            v.api.volume_names()
        );
    }

    /// And a volume outlives the machine that mounted it, so "no machine" is
    /// not "nothing to clean up" — a create that made the volume and then
    /// failed to make the machine leaves one nothing else ever names.
    #[tokio::test]
    async fn a_volume_with_no_machine_is_still_deleted() {
        let (v, _reg) = vendor(FakeFly::default().with_volume("s1"), true);
        let (tx, _rx) = sink();
        v.delete("s1", tx).await.unwrap();
        assert!(v.api.volume_names().is_empty());
    }

    #[tokio::test]
    async fn a_sweep_reclaims_orphaned_volumes_and_keeps_live_ones() {
        let (v, _reg) = vendor(
            FakeFly::default()
                .with_machine("s2", MachineState::Stopped)
                .with_volume("s1")
                .with_volume("s2"),
            true,
        );
        let live = std::collections::HashSet::from(["s1".to_string()]);
        v.sweep_orphans(&live).await.unwrap();
        assert_eq!(
            v.api.volume_names(),
            vec![volume_name("s1")],
            "a live runtime's volume must survive the sweep"
        );
    }

    #[tokio::test]
    async fn a_sweep_never_touches_a_volume_that_is_not_ours() {
        let fake = FakeFly::default();
        fake.volumes.lock().unwrap().push("pgdata".to_string());
        let (v, _reg) = vendor(fake, true);
        let live = std::collections::HashSet::new();
        v.sweep_orphans(&live).await.unwrap();
        assert_eq!(v.api.volume_names(), vec!["pgdata".to_string()]);
    }

    /// One stuck object must not shield the rest. Everything here has already
    /// been billing longer than it should, and aborting on the first failure —
    /// discarding what was swept along the way — meant one undeletable machine
    /// kept every other orphan alive indefinitely.
    #[tokio::test]
    async fn a_sweep_that_cannot_destroy_one_machine_still_destroys_the_others() {
        let (v, _reg) = vendor(
            FakeFly {
                undeletable: vec!["m-s2".to_string()],
                ..FakeFly::default()
            }
            .with_machine("s2", MachineState::Started)
            .with_machine("s3", MachineState::Started),
            false,
        );
        let live = std::collections::HashSet::new();
        assert_eq!(
            v.sweep_orphans(&live).await.unwrap(),
            vec!["s3".to_string()],
            "the machine that could be destroyed must be reported as swept"
        );
    }

    #[test]
    fn a_machine_name_round_trips_to_its_runtime_id() {
        assert_eq!(runtime_id_of(&machine_name("s1")), Some("s1"));
        assert_eq!(runtime_id_of("postgres"), None);
        assert_eq!(runtime_id_of("horsie-"), None);
    }

    #[test]
    fn a_volume_name_obeys_flys_narrower_rule() {
        // Machine names may carry dashes, volume names may not — so the two
        // names cannot be one function.
        assert_eq!(volume_name("s1"), "horsie_s1");
        assert_eq!(volume_name("RT-9f.2"), "horsie_rt_9f_2");
        assert!(volume_name(&"x".repeat(60)).len() <= 30);
    }

    #[test]
    fn provision_steps_ride_the_environment() {
        // The same channel the process provider uses, so the runtime binary
        // needs no Fly-specific path — and without it a provisioned session
        // would come up with an empty workspace and no error.
        let (v, _reg) = vendor(FakeFly::default(), false);
        let spec = RuntimeSpec {
            provision: vec![horsie_models::executor::ProvisionStep {
                name: "checkout".to_string(),
                uses: "git_checkout".to_string(),
                with: vec![],
            }],
            ..spec()
        };
        let machine = v.spec_for("s1", &spec, None).unwrap();
        let provision = machine
            .env
            .iter()
            .find(|(k, _)| k == horsie_models::ENV_PROVISION)
            .map(|(_, v)| v.clone())
            .expect("provision steps must reach the machine");
        assert!(provision.contains("git_checkout"), "{provision}");
    }

    /// Bundles never worked on this vendor: nothing ever told a machine what
    /// address to fetch them from, so the runtime gave up before its first
    /// request — silently, because fetching is best-effort. The credential
    /// helper needs the same address, so it is no longer optional.
    #[test]
    fn a_machine_learns_where_to_reach_the_server() {
        let (v, _reg) = vendor(FakeFly::default(), false);
        let machine = v.spec_for("s1", &spec(), None).unwrap();
        let env: std::collections::HashMap<_, _> = machine.env.into_iter().collect();
        assert_eq!(
            env.get(horsie_models::ENV_SERVER_URL).map(String::as_str),
            Some("https://horsie.example.com"),
            "derived from the configured callback_url"
        );
        assert!(
            env.contains_key(horsie_models::ENV_PLUGINS_DIR),
            "a runtime also needs somewhere to unpack what it fetches"
        );
    }

    /// The vendor no longer mints — the server does, and the token arrives in
    /// `spec.env` like every other secret only the server can produce. What
    /// this vendor still owes is that it carries it, and carries it in the
    /// environment rather than argv, which `ps` makes readable to anything on
    /// the host.
    #[test]
    fn the_dial_token_rides_the_environment_and_never_argv() {
        let (v, _reg) = vendor(FakeFly::default(), false);
        let spec = RuntimeSpec {
            env: vec![horsie_models::executor::EnvVar {
                name: horsie_models::ENV_CONNECT_TOKEN.to_string(),
                value: "acct.s1.deadbeef".to_string(),
            }],
            ..spec()
        };
        let machine = v.spec_for("s1", &spec, None).unwrap();
        assert!(
            machine
                .env
                .iter()
                .any(|(k, val)| k == horsie_models::ENV_CONNECT_TOKEN && val == "acct.s1.deadbeef"),
            "the machine must carry the server's dial token"
        );
        assert!(
            !machine.command.join(" ").contains("acct.s1.deadbeef"),
            "argv is readable by any process through ps"
        );
    }

    #[tokio::test]
    async fn a_rejected_create_is_retryable_but_an_unreachable_api_is_not_the_sessions_fault() {
        let (v, _reg) = vendor(
            FakeFly {
                reject_create: true,
                ..FakeFly::default()
            },
            false,
        );
        let (tx, _rx) = sink();
        assert!(matches!(
            v.create("s1", &spec(), tx).await,
            Err(RuntimeVendorError::Provision(_))
        ));

        let (v, _reg) = vendor(
            FakeFly {
                unreachable: true,
                ..FakeFly::default()
            },
            false,
        );
        let (tx, _rx) = sink();
        assert!(matches!(
            v.create("s1", &spec(), tx).await,
            Err(RuntimeVendorError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_get_for_a_machine_that_does_not_exist_is_gone_not_a_create() {
        // Rebuilding here would silently replace a workspace the user believes
        // still holds their work.
        let (v, _reg) = vendor(FakeFly::default(), false);
        let (tx, _rx) = sink();
        assert!(matches!(
            v.get("s1", &spec(), tx).await,
            Err(RuntimeVendorError::Gone(_))
        ));
        assert!(v.api.calls().is_empty(), "a gone runtime must not be built");
    }

    #[tokio::test]
    async fn a_get_for_a_started_but_unconnected_machine_waits_rather_than_bouncing() {
        // The runtime is PID 1 under `restart: no` and re-dials when its link
        // drops, so a started machine is a live runtime mid-retry — this server
        // restarted, or the network blinked. Bouncing it would kill a runtime
        // seconds from reconnecting and take its in-flight work with it.
        let (v, _reg) = vendor(
            FakeFly::default().with_machine("s1", MachineState::Started),
            false,
        );
        let (tx, _rx) = sink();
        let progress = v.get("s1", &spec(), tx).await.unwrap();
        assert!(matches!(progress, RuntimeProgress::Starting { .. }));
        assert!(
            v.api.calls().is_empty(),
            "a live machine must be left alone, got {:?}",
            v.api.calls()
        );
    }

    #[tokio::test]
    async fn a_get_for_a_stopped_machine_starts_it() {
        let (v, _reg) = vendor(
            FakeFly::default().with_machine("s1", MachineState::Stopped),
            false,
        );
        let (tx, _rx) = sink();
        v.get("s1", &spec(), tx).await.unwrap();
        assert_eq!(v.api.calls(), vec!["start:m-s1".to_string()]);
    }

    #[tokio::test]
    async fn a_get_for_a_booting_machine_waits_instead_of_starting_it() {
        // The first turn of every new session used to fail with `412
        // failed_precondition: unable to start machine from current state:
        // 'created'`. A fresh machine sits in `created` for about six seconds,
        // `parse_state` maps that to `Other`, and `get` used to group `Other`
        // with `Stopped` and start it — which Fly rejects, every time.
        let (v, _reg) = vendor(
            FakeFly::default().with_machine("s1", MachineState::Other),
            false,
        );
        let (tx, _rx) = sink();
        let progress = v.get("s1", &spec(), tx).await.unwrap();
        assert!(matches!(progress, RuntimeProgress::Starting { .. }));
        assert!(
            v.api.calls().is_empty(),
            "a booting machine must be left to finish booting, got {:?}",
            v.api.calls()
        );
    }

    #[tokio::test]
    async fn hibernating_a_runtime_with_no_machine_is_not_an_error() {
        // Advisory: a vendor that has nothing to stop has already achieved what
        // was asked.
        let (v, _reg) = vendor(FakeFly::default(), false);
        let (tx, _rx) = sink();
        assert!(matches!(
            v.hibernate("s1", tx).await.unwrap(),
            RuntimeProgress::Stopped
        ));
    }

    #[tokio::test]
    async fn deleting_destroys_the_machine_and_forgets_the_runtime() {
        let (v, _reg) = vendor(
            FakeFly::default().with_machine("s1", MachineState::Started),
            false,
        );
        let (tx, _rx) = sink();
        assert!(matches!(
            v.delete("s1", tx).await.unwrap(),
            RuntimeProgress::Gone { .. }
        ));
        assert_eq!(v.api.calls(), vec!["destroy:m-s1".to_string()]);
    }

    #[tokio::test]
    async fn a_runtime_that_never_dials_back_is_reported_gone_rather_than_hanging() {
        // A session parked forever on a create is indistinguishable from a
        // deadlock, so the window has to end in an answer.
        let (v, reg) = vendor(FakeFly::default(), false);
        let (tx, mut rx) = sink();
        v.create("s1", &spec(), tx).await.unwrap();
        reg.fail_pending("s1", "it died".to_string()).await;

        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("an outcome must arrive")
            .expect("the sink must stay open");
        assert_eq!(event.runtime_id, "s1");
        assert!(matches!(event.progress, RuntimeProgress::Gone { .. }));
    }

    #[test]
    fn a_machine_is_named_for_its_runtime_so_nothing_has_to_be_written_down() {
        assert_eq!(machine_name("abc-123"), "horsie-abc-123");
    }

    #[tokio::test]
    async fn a_preflight_lists_the_apps_machines_and_creates_nothing() {
        let (v, _reg) = vendor(FakeFly::default(), true);
        assert!(v.preflight().await.is_ok());
        assert!(
            v.api.calls().is_empty(),
            "a check must not build anything, got {:?}",
            v.api.calls()
        );
    }

    #[tokio::test]
    async fn a_refused_listing_is_the_configurations_own_fault() {
        // The whole point: a bad token and an app that does not exist both come
        // back from this one call, and both are terminal — no retry fixes them.
        let (v, _reg) = vendor(
            FakeFly {
                reject_list: true,
                ..FakeFly::default()
            },
            true,
        );
        let err = v.preflight().await.unwrap_err();
        assert!(
            matches!(&err, RuntimeVendorError::Provision(m) if m.contains("401")),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("app already exists"),
            "the message has to name what to go and check: {err}"
        );
    }

    #[tokio::test]
    async fn fly_being_down_is_never_a_verdict_on_the_configuration() {
        // The distinction the caller acts on: refusing to store a vendor
        // because Fly is having an outage would be worse than storing one that
        // may be wrong.
        let (v, _reg) = vendor(
            FakeFly {
                unreachable: true,
                ..FakeFly::default()
            },
            true,
        );
        assert!(matches!(
            v.preflight().await.unwrap_err(),
            RuntimeVendorError::Unavailable(_)
        ));
    }
}
