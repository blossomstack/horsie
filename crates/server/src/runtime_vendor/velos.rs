//! A runtime vendor backed by velos containers.
//!
//! Structurally the twin of [`fly`](crate::runtime_vendor::fly): everything
//! substrate-shaped is behind [`ContainerApi`], so the ordering and the failure
//! taxonomy are testable without a network, and a container is named
//! `horsie-{runtime_id}` so nothing has to be written down to find it again.
//!
//! **This vendor cannot hibernate, and says so by doing nothing.** velos has no
//! suspend — its API is create and delete — and deleting a container is not
//! suspending it: it destroys the workspace and everything in flight to free a
//! slot on a worker. So a hibernate here is advice declined, which the vendor
//! contract explicitly allows and prefers. A Fly vendor stops its machine and
//! keeps the volume; same trait, different capability, and the difference stays
//! inside the implementations exactly as intended.
//!
//! The consequence is that a container only ever disappears because it died.
//! So an acquisition that finds none answers `Gone` rather than scheduling a
//! replacement — a fresh container would come up with an empty workspace, and
//! handing that to a session that believes its work is still there is the one
//! thing an acquisition must never do.
//!
//! **No orphan sweep.** The velos API this uses has no listing, so the default
//! no-op [`RuntimeVendor::sweep_orphans`] applies. A leftover container is
//! reclaimed by name on the next create, which is the same guarantee arriving
//! later rather than a gap.

use crate::runtime_vendor::runtime_command::{build_runtime_command, workspace_paths};
use crate::runtime_vendor::velos_api::{ContainerApi, ContainerLaunchSpec, VelosError};
use crate::runtime_vendor::{RuntimeVendor, RuntimeVendorError};
use async_trait::async_trait;
use horsie_models::runtime_vendor::{RuntimeSpec, RuntimeVendorCapabilities};
use horsie_runtime_host::{
    RuntimeEvent, RuntimeProgress, RuntimeProgressSink,
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
}

impl<A: ContainerApi + 'static> VelosRuntimeVendor<A> {
    pub fn new(
        name: String,
        api: Arc<A>,
        settings: VelosSettings,
        ) -> Self {
        Self {
            name,
            api,
            settings,
        }
    }

    fn launch_spec(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<ContainerLaunchSpec, RuntimeVendorError> {
        let workspaces = workspace_paths(&self.settings.workspace_root, &spec.workspaces)
            .map_err(RuntimeVendorError::Provision)?;
        let mut env: BTreeMap<String, String> = spec
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
        // address a container of ours can route to, and it does not know this
        // image's filesystem. Neither was supplied before, which is why bundles
        // never worked on this vendor at all.
        env.insert(
            horsie_models::ENV_SERVER_URL.to_string(),
            crate::runtime_vendor::server_url::http_base_of(&self.settings.callback_url),
        );
        env.insert(
            horsie_models::ENV_PLUGINS_DIR.to_string(),
            format!(
                "{}/.horsie-plugins",
                self.settings.workspace_root.trim_end_matches('/')
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
        Ok(ContainerLaunchSpec {
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
        })
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
            .create_container(&name, &self.launch_spec(runtime_id, spec)?)
            .await?;
        Ok(())
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

    /// Asks velos who this client is — the one call that needs the server URL
    /// to be right and the token to be accepted, and schedules nothing.
    ///
    /// Classified more narrowly than [`From<VelosError>`] does, and
    /// deliberately: only velos refusing the *credential* is a verdict on the
    /// configuration. A velos running without auth need not serve this endpoint
    /// at all, and taking its 404 as "your token is wrong" would make a working
    /// deployment unconfigurable.
    ///
    /// [`From<VelosError>`]: RuntimeVendorError
    async fn preflight(&self) -> Result<(), RuntimeVendorError> {
        match self.api.preflight().await {
            Ok(()) => Ok(()),
            Err(VelosError::Status {
                status: status @ (401 | 403),
                body,
            }) => Err(RuntimeVendorError::Provision(format!(
                "velos refused this token ({status}: {body})"
            ))),
            Err(VelosError::Status { status, body }) => Err(RuntimeVendorError::Unavailable(
                format!("velos answered {status}: {body}"),
            )),
            Err(VelosError::Request(m)) => Err(RuntimeVendorError::Unavailable(m)),
        }
    }

    async fn create(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        self.schedule(runtime_id, spec).await?;
        // Nothing is waited on here: whether the runtime came up is its own
        // report on its out topic, and the acquiring node is subscribed to it.
        let _ = progress;
        Ok(RuntimeProgress::Starting {
            detail: "the container is being scheduled".to_string(),
        })
    }

    async fn get(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        provisioning: bool,
        progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        let _ = spec;
        // Not connected is not the same as not there, and conflating them was
        // expensive: `schedule` deletes before it creates, so a `get` that
        // rescheduled whenever the runtime was merely not connected yet
        // destroyed the container its own `create` was still booting — and did
        // it again on every retry, so a container slower than the retry
        // interval could never converge.
        //
        // Two things say the runtime is on its way. A live phase means the
        // container exists and is coming up. And the caller telling us a create
        // is still outstanding covers the window before the substrate reports
        // one at all — a fact the session's own journalled status knows and no
        // node-local table can be trusted for once a session may be acquired
        // from a node that never ran its create.
        let alive = self
            .api
            .container_phase(&container_name(runtime_id))
            .await?
            .is_some_and(|phase| !phase.is_dead());
        if alive || provisioning {
            let _ = progress;
            return Ok(RuntimeProgress::Starting {
                detail: "the container is up; waiting for it to dial back".to_string(),
            });
        }

        // No container, and this vendor never takes one away except on delete —
        // so the runtime died with its worker, and its workspace went with it.
        // Terminal, and deliberately not a create: rebuilding here would hand
        // back an empty workspace to a session that believes it still holds
        // work, which is the one thing an acquisition must never do.
        Err(RuntimeVendorError::Gone(format!(
            "no container for runtime '{runtime_id}'"
        )))
    }

    async fn hibernate(
        &self,
        runtime_id: &str,
        _progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        // Declined, and that is a correct implementation of an advisory
        // suspend. velos has no suspend: its API is create and delete, and
        // deleting a container is not hibernating it — it destroys the
        // workspace and everything in flight to save a slot on a worker.
        // Keeping the runtime running costs compute; the alternative costs the
        // user's work.
        // Never `Stopped`: nothing was stopped. The container was left exactly
        // as it was, and a later `get` is what discovers whether it is still
        // coming up or gone.
        let _ = runtime_id;
        Ok(RuntimeProgress::Starting {
            detail: "velos cannot suspend; the container was left as it is".to_string(),
        })
    }

    async fn delete(
        &self,
        runtime_id: &str,
        _progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError> {
        self.api
            .delete_container(&container_name(runtime_id))
            .await?;
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
        /// What `/auth/v1/me` answers, as a status code. `None` → 200.
        whoami_status: Option<u16>,
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
        async fn preflight(&self) -> Result<(), VelosError> {
            self.calls.lock().unwrap().push("preflight".to_string());
            match self.whoami_status {
                None => Ok(()),
                Some(status) => Err(VelosError::Status {
                    status,
                    body: "no".to_string(),
                }),
            }
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
            )),
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
            Arc::new(horsie_runtime_host::MockTransport::ok("")),
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
    async fn a_get_with_no_container_is_terminal_rather_than_a_rebuild() {
        // Nothing takes a container away but a delete, so an absent one died.
        // Scheduling a replacement would hand the session an empty workspace
        // and call it the one it had.
        let (v, _reg) = vendor(FakeVelos::default());
        let (tx, _rx) = sink();
        let Err(err) = v.get("s1", &spec(), tx).await else {
            panic!("a get must never provision")
        };
        assert!(matches!(err, RuntimeVendorError::Gone(_)), "{err:?}");
        assert!(
            v.api.calls().is_empty(),
            "a failed acquisition must not have built anything"
        );
    }

    /// The failure this vendor could not recover from. `schedule` deletes
    /// before it creates, so a `get` that rescheduled whenever the runtime was
    /// merely *not connected yet* destroyed the container its own `create` was
    /// still booting — and repeated the trick on every retry, so a container
    /// slower than the retry interval never got to dial back at all.
    #[tokio::test]
    async fn a_get_never_destroys_a_container_that_is_still_booting() {
        let api = FakeVelos::default();
        *api.phase.lock().unwrap() = Some(ContainerPhase::Running);
        let (v, _reg) = vendor(api);

        let (tx, _rx) = sink();
        v.create("s1", &spec(), tx).await.unwrap();
        let after_create = v.api.calls();

        let (tx, _rx) = sink();
        let progress = v.get("s1", &spec(), tx).await.unwrap();

        assert!(matches!(progress, RuntimeProgress::Starting { .. }));
        assert_eq!(
            v.api.calls(),
            after_create,
            "a booting container must be waited for, never rebuilt"
        );
    }

    /// The same protection when the substrate has not caught up: velos may not
    /// report a phase for a container it accepted moments ago, and the create
    /// already waiting for the dial-back is what says one exists.
    #[tokio::test]
    async fn a_get_joins_a_create_that_is_already_waiting() {
        let (v, reg) = vendor(FakeVelos::default());
        let (tx, _rx) = sink();
        v.create("s1", &spec(), tx).await.unwrap();
        let after_create = v.api.calls();
        assert!(reg.is_awaited("s1").await);

        let (tx, _rx) = sink();
        v.get("s1", &spec(), tx).await.unwrap();

        assert_eq!(
            v.api.calls(),
            after_create,
            "an acquisition must join the create's wait, not start a rival container"
        );
    }

    /// And both waiters are answered — the create's background task must not
    /// see the acquisition's waiter as *its own* being cancelled, which is how
    /// it used to conclude the runtime was unwanted and delete the container.
    #[tokio::test]
    async fn a_dial_back_answers_the_create_and_the_acquisition_alike() {
        let (v, reg) = vendor(FakeVelos::default());
        let (create_tx, mut create_rx) = sink();
        v.create("s1", &spec(), create_tx).await.unwrap();
        let (get_tx, mut get_rx) = sink();
        v.get("s1", &spec(), get_tx).await.unwrap();
        // Everything up to here, so the assertion below is about what the
        // dial-back caused and not about the create's own name reclaim.
        let scheduled = v.api.calls();

        reg.register_transport(
            "s1".to_string(),
            Arc::new(horsie_runtime_host::MockTransport::ok("")),
        )
        .await;

        for rx in [&mut create_rx, &mut get_rx] {
            let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("both waiters must be answered")
                .expect("the sink must stay open");
            assert!(
                matches!(event.progress, RuntimeProgress::Ready(_)),
                "expected Ready, got {:?}",
                event.progress
            );
        }
        assert_eq!(
            v.api.calls(),
            scheduled,
            "a runtime that dialled back must not be touched again"
        );
    }

    #[tokio::test]
    async fn a_hibernate_leaves_the_container_alone() {
        // Deleting a container is not suspending it. velos has no suspend, so
        // the honest implementation of an advisory one is to decline — the
        // contract says as much, and the alternative trades the user's work for
        // a slot on a worker.
        let (v, reg) = vendor(FakeVelos::default());
        reg.register_transport(
            "s1".to_string(),
            Arc::new(horsie_runtime_host::MockTransport::ok("")),
        )
        .await;

        let (tx, _rx) = sink();
        assert!(matches!(
            v.hibernate("s1", tx).await,
            Ok(RuntimeProgress::Ready(_))
        ));

        assert!(v.api.calls().is_empty(), "nothing may be destroyed here");
        assert!(
            reg.runtime_transport("s1").await.is_some(),
            "a declined hibernate must leave the runtime reachable"
        );
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

    /// Bundles never worked on this vendor: nothing ever told a container what
    /// address to fetch them from, so the runtime gave up before its first
    /// request — silently, because fetching is best-effort. The credential
    /// helper needs the same address, so it is no longer optional.
    #[test]
    fn a_container_learns_where_to_reach_the_server() {
        let (v, _reg) = vendor(FakeVelos::default());
        let launch = v.launch_spec("s1", &spec()).unwrap();
        assert_eq!(
            launch
                .env
                .get(horsie_models::ENV_SERVER_URL)
                .map(String::as_str),
            Some("http://horsie:8080"),
            "derived from the configured callback_url"
        );
        assert!(
            launch.env.contains_key(horsie_models::ENV_PLUGINS_DIR),
            "a runtime also needs somewhere to unpack what it fetches"
        );
    }

    /// The vendor no longer mints — the server does, and the token arrives in
    /// `spec.env` like every other secret only the server can produce. What
    /// this vendor still owes is that it carries it, and carries it in the
    /// environment rather than argv.
    #[test]
    fn the_dial_token_rides_the_environment_and_never_argv() {
        let (v, _reg) = vendor(FakeVelos::default());
        let launch = v
            .launch_spec(
                "s1",
                &RuntimeSpec {
                    env: vec![horsie_models::executor::EnvVar {
                        name: horsie_models::ENV_CONNECT_TOKEN.to_string(),
                        value: "acct.s1.deadbeef".to_string(),
                    }],
                    ..spec()
                },
            )
            .unwrap();
        assert_eq!(
            launch
                .env
                .get(horsie_models::ENV_CONNECT_TOKEN)
                .map(String::as_str),
            Some("acct.s1.deadbeef")
        );
        let argv = launch.command.join(" ");
        assert!(
            !argv.contains("acct.s1.deadbeef"),
            "argv is readable by any process through ps"
        );
    }

    #[test]
    fn provision_steps_ride_the_environment() {
        let (v, _reg) = vendor(FakeVelos::default());
        let launch = v
            .launch_spec(
                "s1",
                &RuntimeSpec {
                    provision: vec![horsie_models::executor::ProvisionStep {
                        name: "checkout".to_string(),
                        uses: "git_checkout".to_string(),
                        with: vec![],
                    }],
                    ..spec()
                },
            )
            .unwrap();
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

    #[tokio::test]
    async fn a_preflight_asks_velos_who_we_are_and_schedules_nothing() {
        let (v, _reg) = vendor(FakeVelos::default());
        assert!(v.preflight().await.is_ok());
        assert_eq!(v.api.calls(), vec!["preflight".to_string()]);
    }

    #[tokio::test]
    async fn only_a_refused_token_is_the_configurations_own_fault() {
        // A velos deployment without auth need not serve `/auth/v1/me` at all.
        // Reading its 404 as "your token is wrong" would refuse a save for a
        // deployment that works perfectly.
        for (status, terminal) in [(401, true), (403, true), (404, false), (500, false)] {
            let (v, _reg) = vendor(FakeVelos {
                whoami_status: Some(status),
                ..FakeVelos::default()
            });
            let err = v.preflight().await.unwrap_err();
            assert_eq!(
                matches!(err, RuntimeVendorError::Provision(_)),
                terminal,
                "{status} was classified wrong"
            );
        }
    }
}
