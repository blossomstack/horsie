//! A runtime vendor backed by velos containers.
//!
//! Structurally the twin of [`fly`](crate::runtime_vendor::fly): everything
//! substrate-shaped is behind [`ContainerApi`], so the ordering and the failure
//! taxonomy are testable without a network, and a container is named
//! `horsie-{runtime_id}` so nothing has to be written down to find it again.
//!
//! **Containers are ephemeral.** velos has no volumes and no pause, so a
//! hibernate deletes the container outright and a later `get` schedules a fresh
//! one from the spec the request carries. That is only safe because the
//! workspace was never the durable thing: session state lives server-side, and
//! provision steps rebuild the working tree. A Fly vendor with volumes keeps
//! the workspace instead — same trait, different bargain, and the difference
//! stays inside the implementations exactly as intended.
//!
//! **No orphan sweep.** The velos API this uses has no listing, so the default
//! no-op [`RuntimeVendor::sweep_orphans`] applies. A leftover container is
//! reclaimed by name on the next create, which is the same guarantee arriving
//! later rather than a gap.

use crate::runtime_vendor::runtime_command::{build_runtime_command, workspace_paths};
use crate::runtime_vendor::velos_api::{ContainerApi, ContainerLaunchSpec, VelosError};
use crate::runtime_vendor::{RuntimeHandle, RuntimeVendor, RuntimeVendorError};
use async_trait::async_trait;
use horsie_models::runtime_vendor::{RuntimeSpec, RuntimeVendorCapabilities};
use horsie_runtime_vendor::{
    ConnectedRuntimeRegistry, RuntimeEvent, RuntimeHandleImpl, RuntimeProgress, RuntimeProgressSink,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// How long a runtime has to be scheduled, provision and dial back.
///
/// Matches the vendor protocol's request ceiling rather than a typical HTTP
/// timeout: a create with `git_checkout` steps legitimately runs for minutes.
const READY_WINDOW: Duration = Duration::from_secs(900);

/// How often the container's phase is checked while waiting for a dial-back.
///
/// The point is failing *fast*, not observing state: a container that crashed
/// on a bad image will never dial back, and without this the acquisition would
/// burn the whole `READY_WINDOW` before saying so.
const PHASE_POLL: Duration = Duration::from_millis(750);

/// The velos object name for a runtime. Deterministic — see the module docs.
#[must_use]
pub fn container_name(runtime_id: &str) -> String {
    format!("horsie-{runtime_id}")
}

impl From<VelosError> for RuntimeVendorError {
    fn from(e: VelosError) -> Self {
        match e {
            // velos answered and refused: a provisioning failure the session
            // may retry. An unreachable velos is not the session's fault.
            VelosError::Status { status, body } => {
                let message = format!("{status}: {body}");
                if status == 429 || status >= 500 {
                    Self::Unavailable(message)
                } else {
                    Self::Provision(message)
                }
            }
            VelosError::Request(m) => Self::Unavailable(m),
        }
    }
}

/// How this vendor schedules containers.
pub struct VelosSettings {
    /// OCI image bundling `horsie-runtime`, built without the sandbox feature —
    /// the container is already the isolation boundary.
    pub image: String,
    /// Path to `horsie-runtime` inside the image.
    pub runtime_bin: String,
    /// Where in the container workspaces are allocated.
    pub workspace_root: String,
    /// The `ws://` URL a container reaches this server on, *from velos's
    /// container network*. Not necessarily the address a browser uses.
    pub callback_url: String,
    pub cpu: u32,
    pub memory_bytes: u64,
}

pub struct VelosRuntimeVendor<A: ContainerApi> {
    name: String,
    /// Shared rather than owned because the background wait outlives the call
    /// that started it, and needs the API to poll the container's phase and to
    /// delete one that will never connect.
    api: Arc<A>,
    settings: VelosSettings,
    /// Where this account's runtimes land when they dial back.
    connected: Arc<ConnectedRuntimeRegistry>,
    /// Signs the token each container presents on its dial-back.
    dial_secret: Arc<Vec<u8>>,
    /// The account this vendor serves; travels in the dial token so the connect
    /// route can resolve the right registry without a database read.
    account: String,
}

impl<A: ContainerApi + 'static> VelosRuntimeVendor<A> {
    pub fn new(
        name: String,
        api: Arc<A>,
        settings: VelosSettings,
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

    fn handle(
        &self,
        runtime_id: &str,
        transport: Arc<dyn horsie_runtime_client::RuntimeTransport>,
    ) -> Arc<dyn RuntimeHandle> {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        Arc::new(RuntimeHandleImpl::new(
            runtime_id.to_string(),
            transport,
            rx,
        ))
    }

    fn launch_spec(&self, runtime_id: &str, spec: &RuntimeSpec) -> ContainerLaunchSpec {
        let workspaces = workspace_paths(&self.settings.workspace_root, &spec.workspaces);
        let mut env: BTreeMap<String, String> = spec
            .env
            .iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect();
        // The credential rides the environment, never argv: argv is readable by
        // any process in the container through `ps`.
        env.insert(
            horsie_models::ENV_CONNECT_TOKEN.to_string(),
            horsie_support::dial_token::mint(
                &self.dial_secret,
                &horsie_support::dial_token::DialClaims {
                    user_id: self.account.clone(),
                    runtime_id: runtime_id.to_string(),
                },
            ),
        );
        // Encoding cannot fail for this type, and a container with no provision
        // steps is a working container — so a failure drops the steps rather
        // than the runtime.
        if !spec.provision.is_empty()
            && let Ok(json) = serde_json::to_string(&spec.provision)
        {
            env.insert(horsie_models::ENV_PROVISION.to_string(), json);
        }
        ContainerLaunchSpec {
            image: self.settings.image.clone(),
            command: build_runtime_command(
                &self.settings.runtime_bin,
                &self.settings.callback_url,
                runtime_id,
                &workspaces,
            ),
            env,
            cpu: self.settings.cpu,
            memory_bytes: self.settings.memory_bytes,
        }
    }

    /// Schedule a container, reclaiming any left under the same name first.
    ///
    /// The delete is what makes a create idempotent after a crash: velos
    /// rejects a duplicate name, and a container from a previous incarnation
    /// can no longer dial anywhere useful.
    async fn schedule(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<(), RuntimeVendorError> {
        let name = container_name(runtime_id);
        let _ = self.api.delete_container(&name).await;
        self.api
            .create_container(&name, &self.launch_spec(runtime_id, spec))
            .await?;
        Ok(())
    }

    /// Wait for the dial-back, then report `Ready` on the sink.
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
        let api = self.api.clone();
        let connected = self.connected.clone();
        let id = runtime_id.to_string();
        tokio::spawn(async move {
            let outcome = await_dial_back(api.as_ref(), &id, waiter).await;
            let event = match outcome {
                Ok(()) => match connected.runtime_transport(&id).await {
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
                Err(reason) => {
                    // The container is not coming back, and leaving it costs a
                    // slot on a worker for nothing.
                    let _ = api.delete_container(&container_name(&id)).await;
                    connected.remove(&id).await;
                    RuntimeProgress::Gone { reason }
                }
            };
            let _ = progress.try_send(RuntimeEvent {
                runtime_id: id,
                progress: event,
            });
        });
    }
}

/// Race the dial-back against the deadline and against the container dying.
///
/// Free rather than a method because the background wait outlives the call that
/// started it, and only needs the API.
async fn await_dial_back<A: ContainerApi + ?Sized>(
    api: &A,
    runtime_id: &str,
    waiter: tokio::sync::oneshot::Receiver<Result<(), String>>,
) -> Result<(), String> {
    {
        let name = container_name(runtime_id);
        tokio::pin!(waiter);
        let deadline = tokio::time::sleep(READY_WINDOW);
        tokio::pin!(deadline);
        let mut poll =
            tokio::time::interval_at(tokio::time::Instant::now() + PHASE_POLL, PHASE_POLL);
        loop {
            tokio::select! {
                res = &mut waiter => {
                    return match res {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(message)) => Err(message),
                        Err(_) => Err(
                            "nothing is waiting for this runtime any more".to_string()
                        ),
                    };
                }
                () = &mut deadline => {
                    return Err("the runtime never dialed back".to_string());
                }
                _ = poll.tick() => {
                    // Only a *dead* phase ends the wait. `Unknown` is a worker
                    // whose lease went briefly stale, and treating it as death
                    // would destroy a container that was about to connect.
                    if let Ok(Some(phase)) = api.container_phase(&name).await
                        && phase.is_dead()
                    {
                        return Err(format!(
                            "the container reached {phase:?} before connecting"
                        ));
                    }
                }
            }
        }
    }
}

#[async_trait]
impl<A: ContainerApi + 'static> RuntimeVendor for VelosRuntimeVendor<A> {
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
        // Registered BEFORE the container is scheduled. A container that boots
        // and dials back faster than this call returns would otherwise find
        // nobody waiting, and the acquisition would hang until its window
        // expired.
        let waiter = self.connected.notify_when_ready(runtime_id).await;
        self.schedule(runtime_id, spec).await?;
        self.finish_in_background(runtime_id, waiter, progress);
        Ok(RuntimeProgress::Starting {
            detail: "the container is being scheduled".to_string(),
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
        // Unlike Fly, there is nothing to resume: a hibernated velos runtime is
        // a deleted container. Rescheduling is the resume, and it is safe here
        // for the reason in the module docs — the workspace was never the
        // durable thing.
        let waiter = self.connected.notify_when_ready(runtime_id).await;
        self.schedule(runtime_id, spec).await?;
        self.finish_in_background(runtime_id, waiter, progress);
        Ok(RuntimeProgress::Starting {
            detail: "the container is being rescheduled".to_string(),
        })
    }

    async fn hibernate(
        &self,
        runtime_id: &str,
        _progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        // velos has no pause, so freeing the runtime means deleting it.
        self.api
            .delete_container(&container_name(runtime_id))
            .await?;
        self.connected.remove(runtime_id).await;
        Ok(RuntimeProgress::Stopped)
    }

    async fn delete(
        &self,
        runtime_id: &str,
        _progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        self.api
            .delete_container(&container_name(runtime_id))
            .await?;
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
    use crate::runtime_vendor::velos_api::ContainerPhase;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeVelos {
        calls: Mutex<Vec<String>>,
        phase: Mutex<Option<ContainerPhase>>,
        reject_create: bool,
    }

    impl FakeVelos {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ContainerApi for FakeVelos {
        async fn create_container(
            &self,
            name: &str,
            spec: &ContainerLaunchSpec,
        ) -> Result<(), VelosError> {
            self.calls.lock().unwrap().push(format!("create:{name}"));
            if self.reject_create {
                return Err(VelosError::Status {
                    status: 422,
                    body: "no such image".to_string(),
                });
            }
            assert!(!spec.command.is_empty());
            Ok(())
        }
        async fn delete_container(&self, name: &str) -> Result<(), VelosError> {
            self.calls.lock().unwrap().push(format!("delete:{name}"));
            Ok(())
        }
        async fn container_phase(&self, _name: &str) -> Result<Option<ContainerPhase>, VelosError> {
            Ok(*self.phase.lock().unwrap())
        }
    }

    fn vendor(
        api: FakeVelos,
    ) -> (
        Arc<VelosRuntimeVendor<FakeVelos>>,
        Arc<ConnectedRuntimeRegistry>,
    ) {
        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        (
            Arc::new(VelosRuntimeVendor::new(
                "velos".to_string(),
                Arc::new(api),
                VelosSettings {
                    image: "ghcr.io/o/runtime:1".to_string(),
                    runtime_bin: "horsie-runtime".to_string(),
                    workspace_root: "/workspaces".to_string(),
                    callback_url: "ws://horsie:8080/api/runtime/connect".to_string(),
                    cpu: 1,
                    memory_bytes: 1 << 30,
                },
                connected.clone(),
                Arc::new(vec![0_u8; 32]),
                "u1".to_string(),
            )),
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
    async fn a_create_reclaims_the_name_before_scheduling() {
        // velos rejects a duplicate name, so without the delete a re-create
        // after a crash could never succeed.
        let (v, _reg) = vendor(FakeVelos::default());
        let (tx, _rx) = sink();
        let progress = v.create("s1", &spec(), tx).await.unwrap();
        assert!(matches!(progress, RuntimeProgress::Starting { .. }));
        assert_eq!(
            v.api.calls(),
            vec![
                "delete:horsie-s1".to_string(),
                "create:horsie-s1".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn a_rejected_schedule_is_a_provisioning_failure() {
        let (v, _reg) = vendor(FakeVelos {
            reject_create: true,
            ..FakeVelos::default()
        });
        let (tx, _rx) = sink();
        assert!(matches!(
            v.create("s1", &spec(), tx).await,
            Err(RuntimeVendorError::Provision(_))
        ));
    }

    #[tokio::test]
    async fn a_get_for_a_connected_runtime_hands_it_straight_back() {
        let (v, reg) = vendor(FakeVelos::default());
        let (tx, _rx) = sink();
        reg.register_transport(
            "s1".to_string(),
            Arc::new(horsie_runtime_client::MockTransport::ok("")),
        )
        .await;
        assert!(matches!(
            v.get("s1", &spec(), tx).await,
            Ok(RuntimeProgress::Ready(_))
        ));
        assert!(
            v.api.calls().is_empty(),
            "a live runtime must not be rebuilt"
        );
    }

    #[tokio::test]
    async fn a_get_for_a_hibernated_runtime_reschedules_it() {
        // The velos bargain: no volumes, so resuming *is* rescheduling, from
        // the spec the request carries.
        let (v, _reg) = vendor(FakeVelos::default());
        let (tx, _rx) = sink();
        let progress = v.get("s1", &spec(), tx).await.unwrap();
        assert!(matches!(progress, RuntimeProgress::Starting { .. }));
        assert_eq!(
            v.api.calls(),
            vec![
                "delete:horsie-s1".to_string(),
                "create:horsie-s1".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn a_hibernate_deletes_the_container() {
        let (v, _reg) = vendor(FakeVelos::default());
        let (tx, _rx) = sink();
        assert!(matches!(
            v.hibernate("s1", tx).await,
            Ok(RuntimeProgress::Stopped)
        ));
        assert_eq!(v.api.calls(), vec!["delete:horsie-s1".to_string()]);
    }

    #[tokio::test]
    async fn a_dead_container_fails_the_acquisition_without_waiting_out_the_window() {
        // Without the phase poll this would burn the full 15-minute window on a
        // container that crashed on a bad image seconds after being scheduled.
        let api = FakeVelos::default();
        *api.phase.lock().unwrap() = Some(ContainerPhase::Failed);
        let (v, _reg) = vendor(api);
        let (tx, mut rx) = sink();
        v.create("s1", &spec(), tx).await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("an outcome must arrive well inside the ready window")
            .expect("the sink must stay open");
        assert!(matches!(event.progress, RuntimeProgress::Gone { .. }));
        assert!(
            v.api.calls().contains(&"delete:horsie-s1".to_string()),
            "a container that will never connect must not be left running"
        );
    }

    #[tokio::test]
    async fn a_container_in_an_unknown_phase_is_given_more_time() {
        // `Unknown` is a worker whose lease went briefly stale. Treating it as
        // death would destroy a container that was about to connect.
        let api = FakeVelos::default();
        *api.phase.lock().unwrap() = Some(ContainerPhase::Unknown);
        let (v, _reg) = vendor(api);
        let (tx, mut rx) = sink();
        v.create("s1", &spec(), tx).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .is_err(),
            "an unknown phase must not end the wait"
        );
    }

    #[test]
    fn the_dial_token_rides_the_environment_and_never_argv() {
        let (v, _reg) = vendor(FakeVelos::default());
        let launch = v.launch_spec("s1", &spec());
        assert!(launch.env.contains_key(horsie_models::ENV_CONNECT_TOKEN));
        let argv = launch.command.join(" ");
        assert!(
            !argv.contains(&launch.env[horsie_models::ENV_CONNECT_TOKEN]),
            "argv is readable by any process through ps"
        );
    }

    #[test]
    fn provision_steps_ride_the_environment() {
        let (v, _reg) = vendor(FakeVelos::default());
        let launch = v.launch_spec(
            "s1",
            &RuntimeSpec {
                provision: vec![horsie_models::executor::ProvisionStep {
                    name: "checkout".to_string(),
                    uses: "git_checkout".to_string(),
                    with: vec![],
                }],
                ..spec()
            },
        );
        assert!(
            launch.env[horsie_models::ENV_PROVISION].contains("git_checkout"),
            "{:?}",
            launch.env
        );
    }

    #[test]
    fn a_container_is_named_for_its_runtime_so_nothing_has_to_be_written_down() {
        assert_eq!(container_name("abc-123"), "horsie-abc-123");
    }
}
