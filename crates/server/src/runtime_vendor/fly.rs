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

use crate::runtime_vendor::{RuntimeHandle, RuntimeVendor, RuntimeVendorError};
use async_trait::async_trait;
use horsie_models::runtime_vendor::{RuntimeSpec, RuntimeVendorCapabilities};
use horsie_runtime_vendor::{
    ConnectedRuntimeRegistry, RuntimeEvent, RuntimeHandleImpl, RuntimeProgress, RuntimeProgressSink,
};
use std::sync::Arc;
use std::time::Duration;

/// How long a runtime has to boot, provision and dial back before its
/// acquisition is written off.
///
/// Matches the vendor protocol's request ceiling rather than a typical HTTP
/// timeout: a create with `git_checkout` steps legitimately runs for minutes.
const READY_WINDOW: Duration = Duration::from_secs(900);

/// The machine name for a runtime. Deterministic on purpose — see the module
/// docs.
#[must_use]
pub fn machine_name(runtime_id: &str) -> String {
    format!("horsie-{runtime_id}")
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
    /// Anything transitional or unknown. Treated as "not usable yet", never as
    /// gone: guessing a machine away would destroy a workspace.
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
    async fn start(&self, machine_id: &str) -> Result<(), FlyError>;
    async fn stop(&self, machine_id: &str) -> Result<(), FlyError>;
    /// Destroy the machine and any volume it mounted. Idempotent: a machine
    /// that is already gone is success, because delete is what the caller
    /// wanted either way.
    async fn destroy(&self, machine_id: &str) -> Result<(), FlyError>;
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
    /// Where this account's runtimes land when they dial back.
    connected: Arc<ConnectedRuntimeRegistry>,
    /// Signs the token each machine presents on its dial-back.
    dial_secret: Arc<Vec<u8>>,
    /// The account this vendor serves; travels in the dial token so the
    /// connect route can resolve the right registry without a database read.
    account: String,
}

impl<A: FlyApi> FlyRuntimeVendor<A> {
    pub fn new(
        name: String,
        api: A,
        settings: FlySettings,
        connected: Arc<ConnectedRuntimeRegistry>,
        dial_secret: Arc<Vec<u8>>,
        account: String,
    ) -> Self {
        Self {
            name,
            api,
            settings,
            connected,
            dial_secret,
            account,
        }
    }

    fn dial_token(&self, runtime_id: &str) -> String {
        horsie_support::dial_token::mint(
            &self.dial_secret,
            &horsie_support::dial_token::DialClaims {
                user_id: self.account.clone(),
                runtime_id: runtime_id.to_string(),
            },
        )
    }

    fn handle(
        &self,
        runtime_id: &str,
        transport: Arc<dyn horsie_runtime_client::RuntimeTransport>,
    ) -> Arc<dyn RuntimeHandle> {
        // A closed-signal the registry owns would be better, but the registry
        // reports liveness by presence: a dropped runtime is simply absent.
        // Until it carries a watch, this handle is closed only when replaced.
        let (_tx, rx) = tokio::sync::watch::channel(false);
        Arc::new(RuntimeHandleImpl::new(
            runtime_id.to_string(),
            transport,
            rx,
        ))
    }

    fn spec_for(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        volume: Option<String>,
    ) -> MachineSpec {
        let workspaces: Vec<(String, String)> = spec
            .workspaces
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    format!(
                        "{}/{name}",
                        self.settings.workspace_root.trim_end_matches('/')
                    ),
                )
            })
            .collect();
        let mut env: Vec<(String, String)> = spec
            .env
            .iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect();
        // The credential rides the environment, never argv: argv is readable by
        // any process on the host through `ps`.
        env.push((
            horsie_models::ENV_CONNECT_TOKEN.to_string(),
            self.dial_token(runtime_id),
        ));
        MachineSpec {
            name: machine_name(runtime_id),
            image: self.settings.image.clone(),
            region: self.settings.region.clone(),
            command: build_machine_command(
                "horsie-runtime",
                &self.settings.callback_url,
                runtime_id,
                &workspaces,
            ),
            env,
            mount: volume.map(|id| (id, self.settings.workspace_root.clone())),
        }
    }

    /// Wait for the runtime to dial back, then report `Ready` on the sink.
    ///
    /// Spawned only after the calling operation has returned, which is the
    /// ordering rule the vendor contract requires: the return value is the
    /// caller's first observation, and nothing may precede it.
    fn finish_in_background(
        &self,
        runtime_id: &str,
        waiter: tokio::sync::oneshot::Receiver<Result<(), String>>,
        progress: RuntimeProgressSink,
    ) {
        let connected = self.connected.clone();
        let id = runtime_id.to_string();
        tokio::spawn(async move {
            let outcome = tokio::time::timeout(READY_WINDOW, waiter).await;
            let event = match outcome {
                Ok(Ok(Ok(()))) => match connected.runtime_transport(&id).await {
                    Some(transport) => {
                        let (_tx, rx) = tokio::sync::watch::channel(false);
                        RuntimeProgress::Ready(Arc::new(RuntimeHandleImpl::new(
                            id.clone(),
                            transport,
                            rx,
                        )))
                    }
                    None => RuntimeProgress::Gone {
                        reason: "the runtime announced itself and then vanished".to_string(),
                    },
                },
                Ok(Ok(Err(message))) => RuntimeProgress::Gone { reason: message },
                Ok(Err(_)) => RuntimeProgress::Gone {
                    reason: "nothing is waiting for this runtime any more".to_string(),
                },
                Err(_) => RuntimeProgress::Gone {
                    reason: "the runtime never dialed back".to_string(),
                },
            };
            let _ = progress.try_send(RuntimeEvent {
                runtime_id: id,
                progress: event,
            });
        });
    }
}

/// The machine's command line: make the workspace directories, then `exec` the
/// runtime so it becomes PID 1 and its exit is the machine's exit.
#[must_use]
pub fn build_machine_command(
    runtime_bin: &str,
    endpoint: &str,
    runtime_id: &str,
    workspaces: &[(String, String)],
) -> Vec<String> {
    let mut exec_line = format!(
        "exec {} --endpoint {} --runtime-id {}",
        shell_quote(runtime_bin),
        shell_quote(endpoint),
        shell_quote(runtime_id),
    );
    for (name, path) in workspaces {
        exec_line.push_str(&format!(
            " --workspace {}",
            shell_quote(&format!("{name}={path}"))
        ));
    }
    let script = if workspaces.is_empty() {
        exec_line
    } else {
        let dirs = workspaces
            .iter()
            .map(|(_, path)| shell_quote(path))
            .collect::<Vec<_>>()
            .join(" ");
        format!("mkdir -p {dirs} && {exec_line}")
    };
    vec!["/bin/sh".to_string(), "-c".to_string(), script]
}

/// POSIX single-quote a value so it survives `sh -c` verbatim. Workspace paths
/// derive from user input, so quote defensively.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
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

    async fn create(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        // Registered BEFORE the machine is asked for. A machine that boots and
        // dials back faster than this call returns would otherwise find nobody
        // waiting, and the acquisition would hang until its window expired.
        let waiter = self.connected.notify_when_ready(runtime_id).await;

        let volume = if self.settings.volumes {
            Some(
                self.api
                    .create_volume(&machine_name(runtime_id), &self.settings.region)
                    .await?,
            )
        } else {
            None
        };
        self.api
            .create_machine(&self.spec_for(runtime_id, spec, volume))
            .await?;

        self.finish_in_background(runtime_id, waiter, progress);
        Ok(RuntimeProgress::Starting {
            detail: "the machine is booting".to_string(),
        })
    }

    async fn get(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        if let Some(transport) = self.connected.runtime_transport(runtime_id).await {
            return Ok(RuntimeProgress::Ready(self.handle(runtime_id, transport)));
        }

        let Some(machine) = self.api.machine_by_name(&machine_name(runtime_id)).await? else {
            // Terminal, and deliberately not a create: rebuilding here would
            // silently replace a workspace the user believes still holds work.
            return Err(RuntimeVendorError::Gone(format!(
                "no machine for runtime '{runtime_id}'"
            )));
        };

        let waiter = self.connected.notify_when_ready(runtime_id).await;
        // Started but not connected means the server restarted: the runtime's
        // socket died with it, and since the runtime has no reconnect loop its
        // process is gone. Bounce the machine so a fresh one dials in.
        match machine.state {
            MachineState::Started => {
                self.api.stop(&machine.id).await?;
                self.api.start(&machine.id).await?;
            }
            MachineState::Stopped | MachineState::Suspended | MachineState::Other => {
                self.api.start(&machine.id).await?;
            }
        }
        let _ = spec;

        self.finish_in_background(runtime_id, waiter, progress);
        Ok(RuntimeProgress::Starting {
            detail: "the machine is resuming".to_string(),
        })
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
        self.connected.remove(runtime_id).await;
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
        self.connected.remove(runtime_id).await;
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
        unreachable: bool,
    }

    impl FakeFly {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
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
    ) -> (FlyRuntimeVendor<FakeFly>, Arc<ConnectedRuntimeRegistry>) {
        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        (
            FlyRuntimeVendor::new(
                "fly-iad".to_string(),
                api,
                settings(volumes),
                connected.clone(),
                Arc::new(b"secret".to_vec()),
                "acct".to_string(),
            ),
            connected,
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
                "volume:horsie-s1".to_string(),
                "machine:horsie-s1".to_string()
            ]
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
    async fn a_get_for_a_started_but_unconnected_machine_bounces_it() {
        // Started with no transport means the server restarted: the runtime's
        // socket died with it and, having no reconnect loop, so did its
        // process. Only a fresh boot produces a runtime that dials in.
        let (v, _reg) = vendor(
            FakeFly::default().with_machine("s1", MachineState::Started),
            false,
        );
        let (tx, _rx) = sink();
        let progress = v.get("s1", &spec(), tx).await.unwrap();
        assert!(matches!(progress, RuntimeProgress::Starting { .. }));
        assert_eq!(
            v.api.calls(),
            vec!["stop:m-s1".to_string(), "start:m-s1".to_string()]
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
    fn the_machine_command_quotes_every_path_it_interpolates() {
        let command = build_machine_command(
            "horsie-runtime",
            "wss://h/api/runtime/connect",
            "s1",
            &[("main".to_string(), "/work/it's here".to_string())],
        );
        assert_eq!(command[0], "/bin/sh");
        assert_eq!(command[1], "-c");
        assert!(
            command[2].contains(r"'/work/it'\''s here'"),
            "an apostrophe must not end the quoted string: {}",
            command[2]
        );
    }

    #[test]
    fn a_machine_is_named_for_its_runtime_so_nothing_has_to_be_written_down() {
        assert_eq!(machine_name("abc-123"), "horsie-abc-123");
    }
}
