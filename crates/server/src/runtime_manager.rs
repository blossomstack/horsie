//! Runtime lifecycle, owned server-side and kept off every actor mailbox.
//!
//! The session and agent actors do not know how a runtime comes to exist, how
//! it is resumed, or how it goes away. They ask for a client and get one, or
//! they get one of two errors that mean genuinely different things
//! ([`RuntimeError::Unavailable`] is retryable; [`RuntimeError::Gone`] is
//! terminal). Everything else — resolving the vendor, assembling the spec,
//! minting short-lived credentials — happens here.
//!
//! **Created once.** [`RuntimeManager::create`] has exactly one caller — the
//! session actor that owns the runtime — and [`RuntimeManager::get`] can never
//! provision, so no later code path can silently rebuild a workspace the user
//! believes still exists.
//!
//! **A create does not wait; an acquisition does.** Nothing here knows a create
//! is in flight — that belongs to the session, which journals the attempt and
//! refuses to start a turn until it has an answer, a wait that survives the
//! process dying mid-create where one held in a map beside this manager could
//! not. [`RuntimeManager::get`] is the other case: a vendor whose substrate
//! boots a machine answers `Starting` and reports the outcome on a progress
//! sink, so somebody has to read that sink, and the caller asking for a client
//! is the only one who needs the answer.

use crate::runtime_vendor::RuntimeVendor;
use crate::runtime_vendor::{RuntimeSpec, RuntimeVendorError, WorkspaceSpec};
use crate::sessions::spec::{RuntimeVendorMap, SessionSpec};
use horsie_models::runtime::RuntimeOutboundMessage;
use horsie_runtime_host::{InFlight, RuntimeClient};
use std::collections::HashMap;
use std::sync::Arc;

/// What can go wrong acquiring a runtime, split by what the session should do
/// about it.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The vendor itself is not reachable — not registered, or its socket is
    /// dead. Always retryable: a laptop agent offline for ten minutes must
    /// never cost a session permanently.
    #[error("runtime vendor unavailable: {0}")]
    Unavailable(String),
    /// A live vendor says this session's runtime cannot be produced. Terminal
    /// for the session.
    #[error("runtime is gone: {0}")]
    Gone(String),
    /// A create could not provision. The session can try again.
    #[error("runtime provisioning failed: {0}")]
    Provision(String),
}

/// What the manager needs from the server to assemble a runtime spec.
#[derive(Clone)]
pub struct RuntimeDeps {
    pub vendors: RuntimeVendorMap,
    pub github_tokens: Option<Arc<dyn crate::github::GithubTokenMinter>>,
    pub plugins: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
    /// Signs the dial token every runtime presents when it dials back.
    ///
    /// Here rather than in each vendor because the server is now the only
    /// minter. A vendor that signed with a secret of its own — which
    /// `horsie connect` did — produced a token only *it* could verify, so the
    /// server could not accept that runtime's dial-back and could not
    /// authenticate anything else the runtime later asked for.
    pub dial_secret: Arc<Vec<u8>>,
    /// The account whose runtimes these are. Travels in the dial token so the
    /// route that accepts the dial knows which secret to check it against.
    pub account: String,
    /// How a runtime is reached once it has dialled in.
    ///
    /// Here rather than in each vendor because *which* node accepted the dial
    /// is not a vendor's business, and a topic name contains no node. A vendor
    /// brings the substrate up; the manager is what waits for the runtime on it
    /// and hands back something to talk to.
    pub bus: Arc<dyn crate::bus::Bus>,
}

/// What ended one turn of the acquisition wait.
///
/// Named rather than expressed as nested `Option`s because the two sources mean
/// genuinely different things: one is the substrate's opinion of the machine,
/// the other is the runtime speaking for itself.
enum Awaited {
    /// The vendor said something about the substrate of *this* runtime.
    Substrate(horsie_runtime_host::RuntimeProgress),
    /// The runtime announced itself on its out topic.
    Dialled,
    /// The out topic itself ended, so nothing can ever be observed on it.
    TopicClosed,
}

/// How many progress reports may queue before the oldest are dropped.
///
/// Dropping is correct: progress is advisory and the call's return value is the
/// outcome, so a consumer falling behind must never stall a provision.
const PROGRESS_BUFFER: usize = 32;

/// How long an acquisition waits for a runtime to become reachable.
///
/// Above every vendor's own substrate window on purpose, so a vendor that *can*
/// see why the runtime will never come up is always the one to say so first.
/// This is the backstop for the case nobody can explain: a machine the substrate
/// is happy with, holding a runtime that never announces itself. Without it such
/// an acquisition would park a turn forever.
const ACQUIRE_WINDOW: std::time::Duration = std::time::Duration::from_secs(960);

/// Where an acquisition's running commentary goes, in the vendor's own words.
///
/// A plain channel for the same reason [`RuntimeProgressSink`] is one, and
/// `try_send` for the same reason too: narration is advisory, so a consumer
/// falling behind must drop words rather than stall the acquisition it is
/// describing.
///
/// Only the words, not the [`RuntimeProgress`] they came from: the caller is a
/// log, not a state machine — the outcome is the return value, and a second
/// party interpreting progress states is how two readings of one runtime start
/// to disagree.
///
/// [`RuntimeProgressSink`]: horsie_runtime_host::RuntimeProgressSink
/// [`RuntimeProgress`]: horsie_runtime_host::RuntimeProgress
pub type NarrationSink = tokio::sync::mpsc::Sender<String>;

/// How many unread lines of narration may queue before the newest are dropped.
pub const NARRATION_BUFFER: usize = 8;

/// What the manager keeps for one acquired runtime, for as long as it is
/// acquired.
///
/// Per `(runtime, incarnation)` rather than per client, because both members are
/// about the *sandbox*: one in-flight map so a reconciler sees every agent's
/// calls rather than one agent's, and one reconciler task so N clones of a
/// `RuntimeClient` do not become N polling loops.
struct RuntimeSlot {
    in_flight: Arc<InFlight>,
    /// Aborted when this slot is dropped — on hibernate, on delete, or when the
    /// manager itself goes away.
    _reconciler: ReconcilerTask,
}

struct ReconcilerTask(tokio::task::JoinHandle<()>);

impl Drop for ReconcilerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct RuntimeManager {
    deps: RuntimeDeps,
    /// One slot per acquired runtime, keyed by `(runtime, incarnation)`.
    ///
    /// A `std::sync::Mutex` rather than an async one: every critical section here
    /// is a map read or insert, and nothing awaits while holding it.
    slots: std::sync::Mutex<HashMap<(String, String), Arc<RuntimeSlot>>>,
}

impl RuntimeManager {
    #[must_use]
    pub fn new(deps: RuntimeDeps) -> Self {
        Self {
            deps,
            slots: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// This runtime's slot, spawning its reconciler on the first acquisition.
    ///
    /// The transport is only used to build the reconciler, and only on the round
    /// that creates the slot: every later acquisition of the same runtime joins
    /// the loop already running rather than starting a rival.
    fn slot(
        &self,
        session: &str,
        incarnation: &str,
        transport: &Arc<dyn horsie_runtime_host::RuntimeTransport>,
    ) -> Arc<RuntimeSlot> {
        let key = (session.to_string(), incarnation.to_string());
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(slots.entry(key).or_insert_with(|| {
            let in_flight = Arc::new(InFlight::new());
            let reconciler = tokio::spawn(crate::runtime_reconciler::reconcile(
                transport.clone(),
                in_flight.clone(),
            ));
            Arc::new(RuntimeSlot {
                in_flight,
                _reconciler: ReconcilerTask(reconciler),
            })
        }))
    }

    /// Forget every slot for this runtime, whatever its incarnation, which is
    /// what stops its reconciler.
    ///
    /// By runtime and not by `(runtime, incarnation)` because the callers —
    /// hibernate and delete — are about the runtime as a whole, and a slot left
    /// behind for a superseded incarnation would poll a sandbox nobody can reach.
    fn forget(&self, session: &str) {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(runtime, _), _| runtime != session);
    }

    fn vendor(&self, vendor: &str) -> Result<Arc<dyn RuntimeVendor>, RuntimeError> {
        let links =
            self.deps.vendors.read().map_err(|_| {
                RuntimeError::Unavailable("vendor registry lock poisoned".to_string())
            })?;
        let link = links.get(vendor).cloned().ok_or_else(|| {
            RuntimeError::Unavailable(format!("unknown runtime vendor '{vendor}'"))
        })?;
        if !link.is_reachable() {
            return Err(RuntimeError::Unavailable(format!(
                "vendor '{vendor}' is not connected"
            )));
        }
        Ok(link)
    }

    /// The session's provision steps in the shape the runtime protocol speaks.
    ///
    /// A conversion rather than one shared type: `ProvisionStepSpec` is
    /// persisted with the session and evolves at the speed of a migration, while
    /// the wire type evolves at the speed of the protocol. Conflating them is
    /// how one ends up gating the other.
    fn wire_steps(
        steps: &[crate::sessions::spec::ProvisionStepSpec],
    ) -> Vec<horsie_models::executor::ProvisionStep> {
        steps
            .iter()
            .map(|s| horsie_models::executor::ProvisionStep {
                name: s.name.clone(),
                uses: s.uses.clone(),
                with: s
                    .with
                    .iter()
                    .map(|(k, v)| horsie_models::executor::StepParam {
                        key: k.clone(),
                        value: v.clone(),
                    })
                    .collect(),
            })
            .collect()
    }

    /// Assemble the vendor-facing spec.
    ///
    /// Re-assembled on every create rather than cached. Nothing in here is
    /// worth holding on to: the dial token is cheap to derive, and everything
    /// else the runtime needs it now fetches for itself.
    async fn runtime_spec(
        &self,
        session: &str,
        incarnation: &str,
        spec: &SessionSpec,
    ) -> Result<RuntimeSpec, RuntimeError> {
        let mut rt_spec = RuntimeSpec {
            workspaces: spec
                .workspaces
                .iter()
                .map(|w| WorkspaceSpec {
                    name: w.name.clone(),
                })
                .collect(),
            // The environment's variables first; the server pushes its own
            // (the dial token, below) after. A name that would shadow one
            // cannot reach here — the environment service refuses it at save.
            env: spec
                .env_vars
                .iter()
                .map(|v| horsie_models::executor::EnvVar {
                    name: v.name.clone(),
                    value: v.value.clone(),
                })
                .collect(),
        };

        // The runtime's own identity, and after this the only credential it
        // carries. Everything that expires is fetched against it rather than
        // baked in beside it — which matters because a vendor whose substrate
        // cannot rewrite a running machine's environment freezes whatever was
        // here at create time, forever.
        rt_spec.env.push(horsie_models::executor::EnvVar {
            name: horsie_models::ENV_CONNECT_TOKEN.to_string(),
            value: horsie_support::dial_token::mint(
                &self.deps.dial_secret,
                &horsie_support::dial_token::DialClaims {
                    user_id: self.deps.account.clone(),
                    runtime_id: session.to_string(),
                    // Not minted here. A fresh value per call would differ
                    // between the create that started the sandbox and the
                    // acquisition that later reaches for it, so the server
                    // would be addressing a provision that never existed.
                    incarnation: incarnation.to_string(),
                },
            ),
        });

        // No GitHub token travels here. A `git_checkout` of a private
        // repository authenticates through the runtime's credential helper,
        // which mints one per git operation against the dial token above —
        // scoped to the same repositories this would have covered, but at the
        // moment of use rather than an hour before it.

        // Resolve the session's selected bundles to fetch refs plus a scoped
        // token; the runtime reads both from its environment at startup.
        //
        // Every vendor participates, including one that cannot provision a
        // workspace. Bundles are not a workspace: the runtime fetches them over
        // its own outbound connection into its own plugins dir, which it can do
        // over a directory it did not create. `horsie connect` announces
        // `supports_provisioning: false` yet already wires `with_bundles`, so
        // gating here was the one thing keeping skills off the most common
        // self-hosted vendor.
        //
        // The runtime resolves the overlap with a host `--plugins-dir` library:
        // fetched bundles win, the host library is the fallback. So selecting
        // bundles replaces the host library for that session, and selecting
        // none leaves it in place.
        if let Some(prov) = self.deps.plugins.as_ref() {
            let mut names = spec.plugins.clone();
            if names.is_empty() {
                names = prov.default_names().await;
            }
            if !names.is_empty() {
                let refs = prov
                    .resolve(&names)
                    .await
                    .map_err(RuntimeError::Provision)?;
                let manifest = serde_json::to_string(&refs)
                    .map_err(|e| RuntimeError::Provision(e.to_string()))?;
                rt_spec.env.push(horsie_models::executor::EnvVar {
                    name: horsie_models::ENV_PLUGIN_MANIFEST.to_string(),
                    value: manifest,
                });
            }
        }

        Ok(rt_spec)
    }

    /// Provision this session's runtime. One caller, once per session.
    ///
    /// Answers with what the vendor said about the runtime it just accepted —
    /// "the machine is booting", "the container is being scheduled" — for the
    /// session to journal. Returned rather than pushed anywhere, because a
    /// create's first observation is the last one this call has: the substrate
    /// finishes on a sink nothing here waits on.
    pub async fn create(
        &self,
        session: &str,
        incarnation: &str,
        vendor: &str,
        spec: &SessionSpec,
    ) -> Result<Option<String>, RuntimeError> {
        let link = self.vendor(vendor)?;
        let rt_spec = self.runtime_spec(session, incarnation, spec).await?;
        // Not awaited to `Ready`, unlike an acquisition. A create's job is to
        // get the substrate to accept the runtime; the session journals that it
        // happened and the first `get` is what waits for it to come up — a wait
        // that survives this process dying, which one held here would not.
        let (progress, _rx) = tokio::sync::mpsc::channel(PROGRESS_BUFFER);
        let first = link
            .create(session, &rt_spec.to_wire(), progress)
            .await
            .map_err(Self::vendor_error)?;
        Ok(Self::narration(&first))
    }

    /// What a progress report says, when it says anything a person would read.
    ///
    /// The vendor's own words, verbatim: it is the only party that knows
    /// whether a machine is booting, resuming or merely still coming up, and a
    /// vocabulary invented up here would have to guess at all three.
    fn narration(progress: &horsie_runtime_host::RuntimeProgress) -> Option<String> {
        use horsie_runtime_host::RuntimeProgress as P;
        match progress {
            P::Starting { detail } | P::Provisioning { detail } => Some(detail.clone()),
            // Nothing to narrate. A runtime that is already up, or one the
            // substrate has merely acknowledged, has no news in it; and the
            // ways a runtime ends are an outcome, which travels as this call's
            // return value rather than as a line in a log.
            P::Requested | P::Ready(_) | P::Stopping | P::Stopped | P::Gone { .. } => None,
        }
    }

    /// Hand back a client for this session's runtime, resuming it if the
    /// vendor hibernated it. Never provisions.
    ///
    /// The client is bound to the vendor's *name*, not to the link this call
    /// happened to resolve. A caller holds it for a whole run — the toolbox an
    /// agent loop executes against is built once — and a vendor process that
    /// reconnects mid-run comes back on a different link. Binding to the name
    /// means the next tool call finds it; binding to the link meant every tool
    /// call for the rest of that turn failed on a dead socket.
    ///
    /// `narrate` is where the wait describes itself. An acquisition is the long
    /// one — a machine that has to resume takes minutes — and the vendor is
    /// saying why the whole time, so a caller with somewhere to put those words
    /// passes a sink and a caller without one passes `None`.
    pub async fn get(
        &self,
        session: &str,
        incarnation: &str,
        vendor: &str,
        spec: &SessionSpec,
        provisioning: bool,
        narrate: Option<NarrationSink>,
    ) -> Result<RuntimeClient, RuntimeError> {
        let link = self.vendor(vendor)?;
        // Subscribed before the vendor is asked for anything. A runtime that is
        // already up answers the moment it is spoken to, and the bus keeps
        // nothing for a subscriber that has not arrived yet — so a subscription
        // opened after the `get` would miss the `Ready` it exists to wait for.
        let mut dialled = crate::bus::topics::runtime_out(
            self.deps.bus.clone(),
            &self.deps.account,
            session,
            incarnation,
        )
        .subscribe()
        .await
        .map_err(|e| RuntimeError::Unavailable(e.to_string()))?;
        // The receiver is held for the whole acquisition, not dropped on the
        // way out. A vendor whose substrate has to boot a machine answers
        // `Starting` and reports what became of the *substrate* on this sink —
        // so dropping it loses the `Gone` that says the machine will never come
        // up at all.
        let (progress, mut rx) = tokio::sync::mpsc::channel(PROGRESS_BUFFER);
        let rt_spec = self.runtime_spec(session, incarnation, spec).await?;
        let first = link
            .get(session, &rt_spec.to_wire(), provisioning, progress)
            .await
            .map_err(Self::vendor_error)?;
        let transport = self
            .await_ready(
                session,
                incarnation,
                first,
                &mut rx,
                &mut dialled,
                narrate.as_ref(),
            )
            .await?;
        let slot = self.slot(session, incarnation, &transport);
        let client = Self::client(session, transport, slot.in_flight.clone());

        // Every acquisition, not only the first. The steps are idempotent, and
        // this is the one party that cannot know whether a hibernated runtime
        // kept its workspace — a Fly machine with a volume did, a rebuilt velos
        // container did not, and the vendor contract deliberately does not say
        // which. So it asks rather than remembering, and the runtime answers
        // from the only place the truth lives.
        if !spec.provision.is_empty() {
            client
                .provision_workspace(Self::wire_steps(&spec.provision))
                .await
                .map_err(|e| RuntimeError::Provision(e.to_string()))?;
        }
        Ok(client)
    }

    fn vendor_error(e: RuntimeVendorError) -> RuntimeError {
        match e {
            RuntimeVendorError::Gone(m) => RuntimeError::Gone(m),
            RuntimeVendorError::Unavailable(m) => RuntimeError::Unavailable(m),
            RuntimeVendorError::Provision(m) => RuntimeError::Provision(m),
        }
    }

    /// Follow an acquisition from its first observation to a runtime that can
    /// be talked to.
    ///
    /// The vendor contract makes this a plain fold: the return value *is* the
    /// first event, and every later one arrives on the sink in order, so there
    /// is nothing to reconcile — only a state to walk until it settles.
    ///
    /// Events for another runtime are ignored rather than trusted: one account
    /// has one sink, and a vendor is free to report on anything it owns.
    ///
    /// Every non-terminal state the fold walks through is narrated on the way
    /// past. That is the whole of what a person waiting has to go on: this loop
    /// can sit here for minutes, and the vendor is describing the wait — first
    /// in the value it returned, then on the sink — the entire time.
    async fn await_ready(
        &self,
        session: &str,
        incarnation: &str,
        first: horsie_runtime_host::RuntimeProgress,
        rx: &mut tokio::sync::mpsc::Receiver<horsie_runtime_host::RuntimeEvent>,
        dialled: &mut crate::bus::Reader<horsie_models::runtime::RuntimeOutboundMessage>,
        narrate: Option<&NarrationSink>,
    ) -> Result<Arc<dyn horsie_runtime_host::RuntimeTransport>, RuntimeError> {
        use Awaited::{Dialled, Substrate, TopicClosed};
        use horsie_runtime_host::RuntimeProgress as P;
        let deadline = tokio::time::Instant::now() + ACQUIRE_WINDOW;
        let mut progress = first;
        // Whether the vendor's sink is still worth selecting on. A closed `mpsc`
        // receiver is always ready, so leaving one in the `select!` would spin
        // this loop at full speed for the rest of the window.
        let mut vendor_talking = true;
        loop {
            if let Some(sink) = narrate
                && let Some(line) = Self::narration(&progress)
            {
                // Dropped rather than awaited when the consumer is behind:
                // nothing about this acquisition may wait on somebody reading
                // about it.
                let _ = sink.try_send(line);
            }
            match progress {
                // A vendor that hands back a pipe owns one: `horsie connect`
                // relays its runtimes through its own link rather than having
                // them dial this server, so there is no topic to wait on.
                P::Ready(transport) => return Ok(transport),
                // Terminal, and the reason travels: a session whose runtime is
                // gone has to be able to say so rather than retry forever.
                P::Gone { reason } => return Err(RuntimeError::Gone(reason)),
                // Not terminal. A vendor that reports a runtime stopped during
                // an acquisition is one that could not revive it this time.
                P::Stopped | P::Stopping => {
                    return Err(RuntimeError::Unavailable(format!(
                        "runtime '{session}' went down during the acquisition"
                    )));
                }
                P::Requested | P::Starting { .. } | P::Provisioning { .. } => {}
            }
            // Two things can end this wait, and they come from different
            // places. The vendor reports on the *substrate* — a machine that
            // will never boot is a `Gone` nothing else would ever say. The
            // runtime reports on *itself*, by dialling in and announcing
            // `Ready` on its out topic, which is the only evidence that
            // something is actually there to talk to.
            let next = tokio::time::timeout_at(deadline, async {
                loop {
                    tokio::select! {
                        event = rx.recv(), if vendor_talking => {
                            match event {
                                // One sink serves every runtime a vendor owns,
                                // so somebody else's news is skipped here rather
                                // than escaping and being narrated as this
                                // runtime's.
                                Some(event) if event.runtime_id == session => {
                                    return Substrate(event.progress);
                                }
                                Some(_) => {}
                                // The vendor has said all it is going to, which
                                // is the *happy* path for a topic-addressed
                                // runtime: once the substrate has accepted the
                                // machine, fly has nothing left to report and
                                // drops its sink on the way out of `get`. The
                                // runtime still has its own announcement to
                                // make, so this ends the vendor's half of the
                                // race and nothing else. Reading it as "the
                                // vendor died" failed every Fly acquisition
                                // instantly.
                                None => vendor_talking = false,
                            }
                        }
                        message = dialled.recv() => {
                            match message {
                                // Everything the runtime says crosses this
                                // topic, so most frames here are replies to
                                // somebody else's request. Only the handshake
                                // ends the wait.
                                Some(RuntimeOutboundMessage::Ready(_)) => return Dialled,
                                Some(_) => continue,
                                None => return TopicClosed,
                            }
                        }
                    }
                }
            })
            .await;

            progress = match next {
                Ok(Dialled) => {
                    let transport = crate::runtime_vendor::BusTransport::open(
                        self.deps.bus.clone(),
                        &self.deps.account,
                        session,
                        incarnation,
                    )
                    .await
                    .map_err(|e| RuntimeError::Unavailable(e.to_string()))?;
                    return Ok(Arc::new(transport));
                }
                Ok(Substrate(next)) => next,
                // The out topic ended. Nothing the runtime says can be observed
                // any more, so waiting out the window would prove nothing.
                // Retryable: it is the bus that broke, not the runtime.
                Ok(TopicClosed) => {
                    return Err(RuntimeError::Unavailable(format!(
                        "runtime '{session}' can no longer be listened to"
                    )));
                }
                Err(_) => {
                    return Err(RuntimeError::Unavailable(format!(
                        "runtime '{session}' was not reachable within the acquisition window"
                    )));
                }
            };
        }
    }

    /// A client over the handle the vendor just returned.
    ///
    /// The handle is bound to the vendor's *name*, not to the link that
    /// answered this call: a caller holds the client for a whole run — the
    /// toolbox an agent loop executes against is built once — and a vendor
    /// process that reconnects mid-run comes back on a different link. Binding
    /// to the name means the next tool call finds it; binding to the link meant
    /// every call for the rest of that turn failed on a dead socket (#187).
    ///
    /// The runtime's own id doubles as its main agent's identity: the server
    /// passes the session id as `runtime_id`, and that is also what the agent
    /// journal is keyed by (`agent/<session-uuid>`). A subagent sharing this
    /// runtime derives its own handle with `RuntimeClient::with_agent_id`.
    /// The in-flight map comes from the runtime's slot rather than being minted
    /// here, so every client for one sandbox — this session's and every
    /// subagent's — tracks into the set its reconciler diffs against. A client
    /// with a map of its own would be invisible to that diff, and its calls would
    /// read as orphans and be cancelled.
    fn client(
        session: &str,
        handle: Arc<dyn horsie_runtime_host::RuntimeTransport>,
        in_flight: Arc<InFlight>,
    ) -> RuntimeClient {
        RuntimeClient::from_arc(handle, session, in_flight)
    }

    /// Advisory: the session is going cold. Best effort — a vendor that is not
    /// there simply misses the hint, and nothing about the session changes.
    pub async fn hibernate(&self, session: &str, vendor: &str) {
        self.forget(session);
        if let Ok(link) = self.vendor(vendor) {
            let (progress, _rx) = tokio::sync::mpsc::channel(PROGRESS_BUFFER);
            let _ = link.hibernate(session, progress).await;
        }
    }

    /// The session was deleted; the vendor decides the runtime's fate.
    pub async fn delete(&self, session: &str, vendor: &str) {
        self.forget(session);
        if let Ok(link) = self.vendor(vendor) {
            let (progress, _rx) = tokio::sync::mpsc::channel(PROGRESS_BUFFER);
            let _ = link.delete(session, progress).await;
        }
    }

    /// A cheap handle bound to one session, for whoever needs to execute.
    #[must_use]
    pub fn provider(
        self: &Arc<Self>,
        session: String,
        incarnation: String,
        provisioning: bool,
        vendor: String,
        spec: SessionSpec,
    ) -> RuntimeClientProvider {
        RuntimeClientProvider {
            manager: self.clone(),
            session,
            incarnation,
            provisioning,
            vendor,
            spec,
        }
    }
}

/// The agent's view of the runtime: one method, no lifecycle.
#[derive(Clone)]
pub struct RuntimeClientProvider {
    manager: Arc<RuntimeManager>,
    session: String,
    /// Which provision this provider speaks to. Bound when the provider is
    /// built rather than read per call, so every acquisition in one run
    /// addresses the same sandbox even if the session re-provisions beneath it.
    incarnation: String,
    /// Whether this session's create was still outstanding when the provider
    /// was built.
    ///
    /// Bound here rather than read per call, exactly as the incarnation is: a
    /// run's acquisitions all speak about the same attempt, and a vendor asked
    /// mid-run must not be told a create finished while it was waiting for it.
    provisioning: bool,
    vendor: String,
    /// Held so an acquisition can carry the spec: the server is the only
    /// durable holder of it, and a vendor keeps no copy on disk.
    spec: SessionSpec,
}

impl RuntimeClientProvider {
    /// A working client for this session's runtime, resumed if need be.
    ///
    /// `narrate` carries the vendor's account of the wait to whoever asked, and
    /// is `None` for a caller with nowhere to show it.
    pub async fn get(&self, narrate: Option<NarrationSink>) -> Result<RuntimeClient, RuntimeError> {
        self.manager
            .get(
                &self.session,
                &self.incarnation,
                &self.vendor,
                &self.spec,
                self.provisioning,
                narrate,
            )
            .await
    }
}

/// A `RuntimeManager` over the vendor map the deps carry.
/// Test-only: production builds it once in `main`.
#[cfg(test)]
pub(crate) fn test_runtime_manager(
    vendors: &crate::sessions::spec::RuntimeVendorMap,
) -> std::sync::Arc<crate::runtime_manager::RuntimeManager> {
    std::sync::Arc::new(crate::runtime_manager::RuntimeManager::new(
        crate::runtime_manager::RuntimeDeps {
            vendors: vendors.clone(),
            github_tokens: None,
            plugins: None,
            dial_secret: std::sync::Arc::new(b"test-dial-secret".to_vec()),
            account: "test-account".to_string(),
            bus: std::sync::Arc::new(crate::bus::MemoryBus::new()),
        },
    ))
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
    use crate::runtime_vendor::fake::FakeRuntimeVendor;
    use crate::sessions::spec::{AgentSettings, SessionSpec, WorkspaceDef};
    use std::collections::HashMap;
    use std::sync::RwLock;

    fn session_spec(vendor: &str) -> SessionSpec {
        SessionSpec {
            name: None,
            agent: AgentSettings {
                model: "mock".into(),
                instructions: None,
                allowed_tools: None,
                use_plugins: None,
                max_iterations: None,
                max_retries: 0,
                mcp_servers: vec![],
                memory_spaces: vec![],
                thinking_effort: None,
                max_concurrent_subagents: None,
                auto_compact: None,
                control_plane: None,
            },
            workspaces: vec![WorkspaceDef {
                name: "main".into(),
            }],
            provision: vec![],
            vendor: vendor.into(),
            plugins: vec![],
            origin: crate::sessions::spec::SessionOrigin::User,
            workflow: None,
            environment: None,
            env_vars: vec![],
        }
    }

    fn manager(vendors: RuntimeVendorMap) -> Arc<RuntimeManager> {
        manager_on(vendors, Arc::new(crate::bus::MemoryBus::new()))
    }

    /// For the tests that have to *be* the runtime: readiness arrives on a topic
    /// now, so a test asserting on an acquisition needs the same bus the manager
    /// subscribes to.
    fn manager_on(vendors: RuntimeVendorMap, bus: Arc<dyn crate::bus::Bus>) -> Arc<RuntimeManager> {
        Arc::new(RuntimeManager::new(RuntimeDeps {
            vendors,
            github_tokens: None,
            plugins: None,
            dial_secret: Arc::new(DIAL_SECRET.to_vec()),
            account: "acct-1".to_string(),
            bus,
        }))
    }

    /// Say what a runtime says when it comes up: its handshake, on its own out
    /// topic. Exactly what the pump publishes on the runtime's behalf.
    async fn announce_ready(bus: &Arc<dyn crate::bus::Bus>, runtime: &str, incarnation: &str) {
        crate::bus::topics::runtime_out(bus.clone(), "acct-1", runtime, incarnation)
            .publish(&RuntimeOutboundMessage::Ready(
                horsie_models::runtime::RuntimeReady {
                    runtime_id: runtime.to_string(),
                },
            ))
            .await
            .expect("publishing a handshake");
    }

    /// Long enough for a spawned acquisition to have reached its subscription,
    /// and for a vendor that says nothing to have dropped its sink. Nothing in
    /// these tests is timing-sensitive beyond that: the assertions are about
    /// which events end an acquisition, not how fast.
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

    const DIAL_SECRET: &[u8] = b"test-dial-secret";

    /// The property every later credential rests on: a runtime's environment
    /// carries a token the *server* can verify. Before this, `horsie connect`
    /// signed with a per-process secret the server had never seen, so a dial
    /// token proved nothing to anyone but the vendor that minted it.
    #[tokio::test]
    async fn the_spec_carries_a_dial_token_the_account_secret_verifies() {
        // No vendor: assembling a spec never consults one.
        let manager = manager(Arc::new(RwLock::new(HashMap::new())));
        let spec = manager
            .runtime_spec("sess-1", "i1", &session_spec("v"))
            .await
            .unwrap();
        let token = spec
            .env
            .iter()
            .find(|e| e.name == horsie_models::ENV_CONNECT_TOKEN)
            .expect("the spec must carry a dial token");
        let claims = horsie_support::dial_token::verify(DIAL_SECRET, &token.value).unwrap();
        assert_eq!(claims.runtime_id, "sess-1");
        assert_eq!(claims.user_id, "acct-1");
    }

    /// Two sessions must not be able to wear each other's identity.
    #[tokio::test]
    async fn each_session_gets_a_token_that_only_names_itself() {
        // No vendor: assembling a spec never consults one.
        let manager = manager(Arc::new(RwLock::new(HashMap::new())));
        let one = manager
            .runtime_spec("sess-1", "i1", &session_spec("v"))
            .await
            .unwrap();
        let two = manager
            .runtime_spec("sess-2", "i1", &session_spec("v"))
            .await
            .unwrap();
        let token_of = |s: &RuntimeSpec| {
            s.env
                .iter()
                .find(|e| e.name == horsie_models::ENV_CONNECT_TOKEN)
                .map(|e| e.value.clone())
                .unwrap()
        };
        assert_ne!(token_of(&one), token_of(&two));
        assert_eq!(
            horsie_support::dial_token::verify(DIAL_SECRET, &token_of(&two))
                .unwrap()
                .runtime_id,
            "sess-2"
        );
    }

    fn published(agent: &FakeRuntimeVendor, name: &str) -> RuntimeVendorMap {
        let mut map = HashMap::new();
        map.insert(
            name.to_string(),
            agent.link() as Arc<dyn crate::runtime_vendor::RuntimeVendor>,
        );
        Arc::new(RwLock::new(map))
    }

    #[tokio::test]
    async fn unavailable_when_the_vendor_name_is_not_registered() {
        let m = manager(Arc::new(RwLock::new(HashMap::new())));
        let Err(err) = m
            .get(
                "s1",
                "i1",
                "nope",
                &SessionSpec::for_vendor("v"),
                false,
                None,
            )
            .await
        else {
            panic!("an unregistered vendor must not yield a client")
        };
        assert!(
            matches!(err, RuntimeError::Unavailable(_)),
            "a missing vendor is retryable, never terminal: {err:?}"
        );
    }

    #[tokio::test]
    async fn unavailable_when_the_link_is_disconnected() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
        agent.disconnect();
        // The link notices asynchronously; poll briefly rather than sleep-and-hope.
        let mut err = None;
        for _ in 0..50 {
            match m
                .get("s1", "i1", "v", &SessionSpec::for_vendor("v"), false, None)
                .await
            {
                Err(RuntimeError::Unavailable(e)) => {
                    err = Some(e);
                    break;
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        assert!(err.is_some(), "a dead socket must read as Unavailable");
    }

    #[tokio::test]
    async fn gone_when_the_vendor_has_no_runtime() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
        let Err(err) = m
            .get("s1", "i1", "v", &SessionSpec::for_vendor("v"), false, None)
            .await
        else {
            panic!("a get must never provision")
        };
        assert!(
            matches!(err, RuntimeError::Gone(_)),
            "a live vendor with no runtime is terminal: {err:?}"
        );
    }

    #[tokio::test]
    async fn get_returns_a_client_after_create() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
        m.create("s1", "i1", "v", &session_spec("v"))
            .await
            .expect("create");
        m.get("s1", "i1", "v", &SessionSpec::for_vendor("v"), false, None)
            .await
            .expect("get after create");
        assert_eq!(
            agent.signals(),
            vec!["create:s1".to_string(), "get:s1".to_string()]
        );
    }

    #[tokio::test]
    async fn create_sends_the_vendor_workspace_names_not_paths() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
        m.create("s1", "i1", "v", &session_spec("v"))
            .await
            .expect("create");
        let sent = agent.last_create_request().expect("create request");
        assert_eq!(sent.workspaces, vec!["main".to_string()]);
    }

    #[tokio::test]
    async fn the_environments_variables_reach_the_vendor() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
        let mut spec = session_spec("v");
        spec.env_vars.push(crate::sessions::spec::EnvVarSpec {
            name: "RUST_LOG".into(),
            value: "debug".into(),
        });
        m.create("s1", "i1", "v", &spec).await.expect("create");
        let sent = agent.last_create_request().expect("create request");
        assert_eq!(
            sent.env
                .iter()
                .find(|e| e.name == "RUST_LOG")
                .map(|e| e.value.as_str()),
            Some("debug")
        );
    }

    /// Resolves any name to a hash of itself, so a test can assert on the
    /// manifest without a plugin store.
    struct FakeProvisioner;

    #[async_trait::async_trait]
    impl crate::plugins::PluginProvisioner for FakeProvisioner {
        async fn catalog(
            &self,
            _names: &[String],
        ) -> Vec<horsie_support::plugin::catalog::CatalogEntry> {
            Vec::new()
        }

        async fn resolve(
            &self,
            names: &[String],
        ) -> Result<Vec<crate::plugins::PluginArtifactRef>, String> {
            Ok(names
                .iter()
                .map(|n| crate::plugins::PluginArtifactRef {
                    name: n.clone(),
                    hash: format!("hash-of-{n}"),
                })
                .collect())
        }

        async fn default_names(&self) -> Vec<String> {
            vec![]
        }
    }

    /// A vendor that cannot provision a workspace still gets the bundle
    /// manifest. Bundles are not a workspace: the runtime fetches them over its
    /// own outbound connection into its own plugins dir, which works over a
    /// directory it did not create. `horsie connect` announces
    /// `supports_provisioning: false` and is exactly this case — gating here is
    /// what used to keep skills off the most common self-hosted vendor.
    #[tokio::test]
    async fn a_vendor_that_cannot_provision_still_receives_the_bundle_manifest() {
        let agent = FakeRuntimeVendor::builder("v")
            .supports_provisioning(false)
            .serve_in_process()
            .await
            .unwrap();
        let m = Arc::new(RuntimeManager::new(RuntimeDeps {
            vendors: published(&agent, "v"),
            github_tokens: None,
            plugins: Some(Arc::new(FakeProvisioner)),
            dial_secret: Arc::new(DIAL_SECRET.to_vec()),
            account: "acct-1".to_string(),
            bus: std::sync::Arc::new(crate::bus::MemoryBus::new()),
        }));
        let mut spec = session_spec("v");
        spec.plugins = vec!["superpowers".to_string()];
        m.create("s1", "i1", "v", &spec).await.expect("create");

        let sent = agent.last_create_request().expect("create request");
        let env = |name: &str| {
            sent.env
                .iter()
                .find(|e| e.name == name)
                .map(|e| e.value.clone())
        };
        let manifest = env(horsie_models::ENV_PLUGIN_MANIFEST)
            .expect("a non-provisioning vendor must still be sent the manifest");
        assert!(
            manifest.contains("hash-of-superpowers"),
            "the manifest names the selected bundle: {manifest}"
        );
        // No bundle credential travels beside it any more: the runtime
        // authenticates its fetch with the dial token it already holds.
        assert!(
            env(horsie_models::ENV_CONNECT_TOKEN).is_some(),
            "the dial token is what authorizes the fetch now"
        );
    }

    /// Mints a distinct token every call, so a test can prove credentials are
    /// never reused across a session's lifetime.
    struct CountingMinter {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingMinter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::github::GithubTokenMinter for CountingMinter {
        async fn mint_for(&self, _repo_urls: &[String]) -> Result<Option<String>, String> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(format!("token-{n}")))
        }
    }

    /// No GitHub credential is assembled into the spec at all any more.
    ///
    /// This used to assert the opposite — that a token was minted fresh on
    /// every create, because a cached one goes stale. That was the right worry
    /// and the wrong fix: a token minted at create time is already stale by the
    /// time a machine has been up an hour, and no vendor can rewrite a running
    /// machine's environment to replace it. The runtime now mints per git
    /// operation instead, so nothing here should be reaching for the minter.
    #[tokio::test]
    async fn no_github_credential_is_baked_into_the_spec() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let minter = CountingMinter::new();
        let m = Arc::new(RuntimeManager::new(RuntimeDeps {
            vendors: published(&agent, "v"),
            github_tokens: Some(minter.clone() as Arc<dyn crate::github::GithubTokenMinter>),
            plugins: None,
            dial_secret: Arc::new(DIAL_SECRET.to_vec()),
            account: "acct-1".to_string(),
            bus: std::sync::Arc::new(crate::bus::MemoryBus::new()),
        }));
        let mut spec = session_spec("v");
        spec.provision
            .push(crate::sessions::spec::ProvisionStepSpec {
                name: "checkout".into(),
                uses: "git_checkout".into(),
                with: vec![("url".into(), "https://github.com/o/repo.git".into())],
            });

        m.create("s1", "i1", "v", &spec).await.expect("create");
        let env = agent.last_create_request().expect("create request").env;
        assert!(
            !env.iter().any(|e| e.name == "GITHUB_TOKEN"),
            "a git credential must not ride the environment: it expires there \
             with nothing able to renew it"
        );
        assert!(
            env.iter()
                .any(|e| e.name == horsie_models::ENV_CONNECT_TOKEN),
            "the dial token is what the credential helper authenticates with"
        );
        assert_eq!(
            minter.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "provisioning must not mint a credential nothing will use"
        );
    }

    /// A substrate that has to boot something: `Starting` first, the outcome on
    /// the sink later. Every cloud vendor in the tree behaves this way, and no
    /// websocket-backed double can stand in for one — a `horsie connect` link
    /// only ever answers once its runtime is already up.
    struct BootingVendor {
        /// What an acquisition reports on the sink, in order, after the
        /// `Starting` it returned. A list rather than one outcome so a test can
        /// put an intermediate state in front of the terminal one, which is
        /// what a substrate that provisions after booting actually does.
        outcome: std::sync::Mutex<Vec<horsie_runtime_host::RuntimeProgress>>,
    }

    impl BootingVendor {
        fn with(outcome: horsie_runtime_host::RuntimeProgress) -> Arc<Self> {
            Self::reporting(vec![outcome])
        }

        fn reporting(outcomes: Vec<horsie_runtime_host::RuntimeProgress>) -> Arc<Self> {
            Arc::new(Self {
                outcome: std::sync::Mutex::new(outcomes),
            })
        }

        /// Says nothing on the sink and drops it, the way every in-server vendor
        /// now behaves once the substrate has accepted the machine.
        fn silent() -> Arc<Self> {
            Self::reporting(Vec::new())
        }

        fn ready() -> Arc<Self> {
            Self::with(horsie_runtime_host::RuntimeProgress::Ready(Arc::new(
                StubHandle,
            )))
        }
    }

    struct StubHandle;

    #[async_trait::async_trait]
    impl horsie_runtime_host::RuntimeTransport for StubHandle {
        async fn relay(
            &self,
            _: horsie_models::runtime::RuntimeInboundMessage,
        ) -> Result<
            horsie_models::runtime::RuntimeOutboundMessage,
            horsie_runtime_host::TransportError,
        > {
            Err(horsie_runtime_host::TransportError::Disconnected)
        }
        async fn send_oneway(
            &self,
            _: horsie_models::runtime::RuntimeInboundMessage,
        ) -> Result<(), horsie_runtime_host::TransportError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::runtime_vendor::RuntimeVendor for BootingVendor {
        fn name(&self) -> &str {
            "booting"
        }
        fn capabilities(&self) -> horsie_models::runtime_vendor::RuntimeVendorCapabilities {
            horsie_models::runtime_vendor::RuntimeVendorCapabilities {
                supports_provisioning: true,
            }
        }
        async fn create(
            &self,
            _: &str,
            _: &horsie_models::runtime_vendor::RuntimeSpec,
            _: horsie_runtime_host::RuntimeProgressSink,
        ) -> Result<horsie_runtime_host::RuntimeProgress, RuntimeVendorError> {
            Ok(horsie_runtime_host::RuntimeProgress::Starting {
                detail: "booting".into(),
            })
        }
        async fn get(
            &self,
            runtime_id: &str,
            _spec: &horsie_models::runtime_vendor::RuntimeSpec,
            _provisioning: bool,
            progress: horsie_runtime_host::RuntimeProgressSink,
        ) -> Result<horsie_runtime_host::RuntimeProgress, RuntimeVendorError> {
            let outcome = std::mem::take(&mut *self.outcome.lock().unwrap());
            let id = runtime_id.to_string();
            // After the return value is built, per the ordering rule.
            tokio::spawn(async move {
                for progress_step in outcome {
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    let _ = progress
                        .send(horsie_runtime_host::RuntimeEvent {
                            runtime_id: id.clone(),
                            progress: progress_step,
                        })
                        .await;
                }
            });
            Ok(horsie_runtime_host::RuntimeProgress::Starting {
                detail: "the machine is up; waiting for it to dial back".into(),
            })
        }
        async fn hibernate(
            &self,
            _: &str,
            _: horsie_runtime_host::RuntimeProgressSink,
        ) -> Result<horsie_runtime_host::RuntimeProgress, RuntimeVendorError> {
            Ok(horsie_runtime_host::RuntimeProgress::Stopped)
        }
        async fn delete(
            &self,
            _: &str,
            _: horsie_runtime_host::RuntimeProgressSink,
        ) -> Result<horsie_runtime_host::RuntimeProgress, RuntimeVendorError> {
            Ok(horsie_runtime_host::RuntimeProgress::Gone {
                reason: "deleted".into(),
            })
        }
    }

    fn published_vendor(vendor: Arc<dyn crate::runtime_vendor::RuntimeVendor>) -> RuntimeVendorMap {
        let mut map = HashMap::new();
        map.insert("v".to_string(), vendor);
        Arc::new(RwLock::new(map))
    }

    /// The failure that made every cloud vendor unusable. A vendor whose
    /// substrate boots a machine answers `Starting` and reports `Ready` on the
    /// sink — and the sink's receiver was dropped on the way out of this call,
    /// so the `Ready` went into a closed channel and every acquisition failed
    /// as "not reachable yet", however long the runtime had been up.
    #[tokio::test]
    async fn an_acquisition_follows_a_booting_runtime_to_ready() {
        let vendor = BootingVendor::ready();
        let m = manager(published_vendor(vendor));
        m.get("s1", "i1", "v", &SessionSpec::for_vendor("v"), false, None)
            .await
            .expect("a runtime that comes up on the sink must be handed back");
    }

    /// The other half of the fold: a vendor that gives up says so, and says it
    /// terminally, so the session stops retrying a runtime that is not coming
    /// back.
    #[tokio::test]
    async fn an_acquisition_that_ends_gone_is_terminal() {
        let vendor = BootingVendor::with(horsie_runtime_host::RuntimeProgress::Gone {
            reason: "the machine never dialed back".into(),
        });
        let m = manager(published_vendor(vendor));
        let Err(err) = m
            .get("s1", "i1", "v", &SessionSpec::for_vendor("v"), false, None)
            .await
        else {
            panic!("a runtime reported gone must not yield a client")
        };
        assert!(
            matches!(&err, RuntimeError::Gone(m) if m.contains("never dialed back")),
            "{err:?}"
        );
    }

    /// A vendor that says nothing is the normal case, not a broken one.
    ///
    /// This asserted the opposite until readiness moved onto the topic: a
    /// dropped sink meant "the vendor's background task died" and failed the
    /// acquisition as `Unavailable`. Once a vendor stops waiting for a dial-back
    /// it has nothing left to report at all — fly drops its sink on the way out
    /// of every `get` — so keeping that rule would fail every Fly acquisition
    /// instantly, however healthy the runtime was.
    #[tokio::test]
    async fn an_acquisition_told_nothing_by_its_vendor_waits_for_the_runtime_itself() {
        let bus: Arc<dyn crate::bus::Bus> = Arc::new(crate::bus::MemoryBus::new());
        let m = manager_on(published_vendor(BootingVendor::silent()), bus.clone());
        let acquiring = tokio::spawn({
            let m = m.clone();
            async move {
                m.get("s1", "i1", "v", &SessionSpec::for_vendor("v"), false, None)
                    .await
            }
        });

        // The vendor's sink closes almost at once. If that were an outcome, this
        // acquisition would already have failed — so the first assertion is that
        // it has not.
        tokio::time::sleep(SETTLE).await;
        assert!(
            !acquiring.is_finished(),
            "a vendor with nothing to say must not end an acquisition"
        );

        announce_ready(&bus, "s1", "i1").await;
        acquiring
            .await
            .expect("the acquiring task")
            .expect("the runtime's own announcement must finish the acquisition");
    }

    /// The bug class D4 deletes rather than fixes.
    ///
    /// Readiness used to be a `oneshot`, which has one receiver, so a second
    /// waiter displaced the first — whose background task then read its cancelled
    /// channel as "nobody wants this runtime any more" and destroyed the
    /// container the second waiter was about to be handed. A topic is natively
    /// multi-subscriber: one announcement, two acquisitions, and neither is a
    /// rival of the other.
    #[tokio::test]
    async fn two_acquisitions_of_one_runtime_are_both_answered_by_one_announcement() {
        let bus: Arc<dyn crate::bus::Bus> = Arc::new(crate::bus::MemoryBus::new());
        let m = manager_on(published_vendor(BootingVendor::silent()), bus.clone());
        let both: Vec<_> = (0..2)
            .map(|_| {
                let m = m.clone();
                tokio::spawn(async move {
                    m.get("s1", "i1", "v", &SessionSpec::for_vendor("v"), false, None)
                        .await
                })
            })
            .collect();

        tokio::time::sleep(SETTLE).await;
        announce_ready(&bus, "s1", "i1").await;

        for acquiring in both {
            acquiring
                .await
                .expect("the acquiring task")
                .expect("both acquisitions must be answered by the one announcement");
        }
    }

    /// A session with steps to run has them run on *every* acquisition, not just
    /// the first.
    ///
    /// The server deliberately keeps no memory of having provisioned. It cannot:
    /// a Fly machine with a volume keeps its workspace across a hibernate and a
    /// rebuilt velos container does not, and the vendor contract does not say
    /// which happened. So it asks each time and the steps absorb the repeat.
    #[tokio::test]
    async fn every_acquisition_provisions_the_workspace() {
        let handle = Arc::new(ProvisionRecorder::default());
        let vendors = published_vendor(WarmVendor::over(handle.clone()));
        let m = manager_on(vendors, Arc::new(crate::bus::MemoryBus::new()));
        let spec = spec_with_checkout();

        for _ in 0..2 {
            m.get("s1", "i1", "v", &spec, false, None)
                .await
                .expect("acquiring a runtime that is already up");
        }

        assert_eq!(
            handle.provisions(),
            vec![
                vec!["checkout repo".to_string()],
                vec!["checkout repo".to_string()]
            ],
            "each acquisition sends its own ProvisionWorkspace, with the same steps"
        );
    }

    /// A session with nothing to check out sends nothing. Provisioning is a real
    /// round trip, and an empty one would cost every plain session a relay.
    #[tokio::test]
    async fn a_session_with_no_steps_sends_no_provision_request() {
        let handle = Arc::new(ProvisionRecorder::default());
        let vendors = published_vendor(WarmVendor::over(handle.clone()));
        let m = manager_on(vendors, Arc::new(crate::bus::MemoryBus::new()));

        m.get("s1", "i1", "v", &SessionSpec::for_vendor("v"), false, None)
            .await
            .expect("acquiring");

        assert!(handle.provisions().is_empty());
    }

    /// A failed step fails the acquisition. Fail-whole: a workspace that is only
    /// partly built is not one an agent can be pointed at, and every later
    /// failure would be a confusing consequence of this one.
    #[tokio::test]
    async fn a_failed_step_fails_the_acquisition_with_its_own_reason() {
        let handle = Arc::new(ProvisionRecorder::failing(
            "git_checkout: repository not found",
        ));
        let vendors = published_vendor(WarmVendor::over(handle));
        let m = manager_on(vendors, Arc::new(crate::bus::MemoryBus::new()));

        let Err(err) = m
            .get("s1", "i1", "v", &spec_with_checkout(), false, None)
            .await
        else {
            panic!("a failed provision must not yield a client")
        };
        assert!(
            matches!(&err, RuntimeError::Provision(m) if m.contains("repository not found")),
            "the runtime's own reason has to survive: {err:?}"
        );
    }

    fn spec_with_checkout() -> SessionSpec {
        SessionSpec {
            provision: vec![crate::sessions::spec::ProvisionStepSpec {
                name: "checkout repo".to_string(),
                uses: "git_checkout".to_string(),
                with: vec![("url".to_string(), "file:///fixture".to_string())],
            }],
            ..SessionSpec::for_vendor("v")
        }
    }

    /// A vendor whose runtime is already up, and stays up.
    ///
    /// Answers `Ready(transport)` as its *return value* on every acquisition, so
    /// the manager takes the vendor-owns-the-pipe path and never waits on the
    /// bus. `BootingVendor` cannot stand in: it drains its outcome list per call,
    /// so a second acquisition would be told nothing and would wait out the whole
    /// acquisition window.
    struct WarmVendor(Arc<dyn horsie_runtime_host::RuntimeTransport>);

    impl WarmVendor {
        fn over(transport: Arc<dyn horsie_runtime_host::RuntimeTransport>) -> Arc<Self> {
            Arc::new(Self(transport))
        }
    }

    #[async_trait::async_trait]
    impl crate::runtime_vendor::RuntimeVendor for WarmVendor {
        fn name(&self) -> &str {
            "warm"
        }
        fn capabilities(&self) -> horsie_models::runtime_vendor::RuntimeVendorCapabilities {
            horsie_models::runtime_vendor::RuntimeVendorCapabilities {
                supports_provisioning: true,
            }
        }
        async fn create(
            &self,
            _: &str,
            _: &horsie_models::runtime_vendor::RuntimeSpec,
            _: horsie_runtime_host::RuntimeProgressSink,
        ) -> Result<horsie_runtime_host::RuntimeProgress, RuntimeVendorError> {
            Ok(horsie_runtime_host::RuntimeProgress::Ready(self.0.clone()))
        }
        async fn get(
            &self,
            _: &str,
            _: &horsie_models::runtime_vendor::RuntimeSpec,
            _: bool,
            _: horsie_runtime_host::RuntimeProgressSink,
        ) -> Result<horsie_runtime_host::RuntimeProgress, RuntimeVendorError> {
            Ok(horsie_runtime_host::RuntimeProgress::Ready(self.0.clone()))
        }
        async fn hibernate(
            &self,
            _: &str,
            _: horsie_runtime_host::RuntimeProgressSink,
        ) -> Result<horsie_runtime_host::RuntimeProgress, RuntimeVendorError> {
            Ok(horsie_runtime_host::RuntimeProgress::Stopped)
        }
        async fn delete(
            &self,
            _: &str,
            _: horsie_runtime_host::RuntimeProgressSink,
        ) -> Result<horsie_runtime_host::RuntimeProgress, RuntimeVendorError> {
            Ok(horsie_runtime_host::RuntimeProgress::Gone {
                reason: "deleted".into(),
            })
        }
    }

    /// A transport that answers `ProvisionWorkspace` and records what it was
    /// asked, so a test can see the request the manager actually sent rather
    /// than infer it from a side effect.
    #[derive(Default)]
    struct ProvisionRecorder {
        seen: std::sync::Mutex<Vec<Vec<String>>>,
        /// When set, every provision fails with this reason.
        fail: Option<String>,
    }

    impl ProvisionRecorder {
        fn failing(reason: &str) -> Self {
            Self {
                seen: std::sync::Mutex::new(Vec::new()),
                fail: Some(reason.to_string()),
            }
        }

        fn provisions(&self) -> Vec<Vec<String>> {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl horsie_runtime_host::RuntimeTransport for ProvisionRecorder {
        async fn relay(
            &self,
            message: horsie_models::runtime::RuntimeInboundMessage,
        ) -> Result<
            horsie_models::runtime::RuntimeOutboundMessage,
            horsie_runtime_host::TransportError,
        > {
            match message {
                horsie_models::runtime::RuntimeInboundMessage::ProvisionWorkspace(req) => {
                    let applied: Vec<String> = req.steps.iter().map(|s| s.name.clone()).collect();
                    self.seen
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(applied.clone());
                    let result = match &self.fail {
                        Some(reason) => horsie_models::runtime::ProvisionResult::Err(
                            horsie_models::runtime::ProvisionError {
                                reason: reason.clone(),
                            },
                        ),
                        None => horsie_models::runtime::ProvisionResult::Ok(
                            horsie_models::runtime::ProvisionOk { applied },
                        ),
                    };
                    Ok(
                        horsie_models::runtime::RuntimeOutboundMessage::ProvisionResult(
                            horsie_models::runtime::ProvisionWorkspaceResponse {
                                call_id: req.call_id,
                                result,
                            },
                        ),
                    )
                }
                _ => Err(horsie_runtime_host::TransportError::Disconnected),
            }
        }
        async fn send_oneway(
            &self,
            _: horsie_models::runtime::RuntimeInboundMessage,
        ) -> Result<(), horsie_runtime_host::TransportError> {
            Ok(())
        }
    }

    /// What a create knows and used to throw away. The substrate has just
    /// accepted the runtime and said something about it — "the machine is
    /// booting" — and that sentence is the only account of the wait anyone
    /// gets, because a create deliberately does not stay to watch.
    #[tokio::test]
    async fn a_create_hands_back_what_the_vendor_said_about_the_runtime() {
        let m = manager(published_vendor(BootingVendor::ready()));
        let said = m
            .create("s1", "i1", "v", &session_spec("v"))
            .await
            .expect("create");
        assert_eq!(
            said.as_deref(),
            Some("booting"),
            "the vendor's own words have to survive the create"
        );
    }

    /// A vendor with nothing to narrate says nothing. `horsie connect` answers
    /// `Ready` because its runtime is already up, and inventing a line for that
    /// would put a wait on screen that never happened.
    #[tokio::test]
    async fn a_create_with_nothing_to_report_stays_quiet() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
        let said = m
            .create("s1", "i1", "v", &session_spec("v"))
            .await
            .expect("create");
        assert_eq!(said, None);
    }

    /// The long wait, narrated. An acquisition can sit here for minutes while a
    /// machine resumes, and the vendor describes it the whole way — first in
    /// what it returned, then on its sink. Every one of those states used to be
    /// matched and discarded, so the panel showed nothing at all between the
    /// message going out and the reply coming back.
    #[tokio::test]
    async fn an_acquisition_narrates_every_state_it_waits_through() {
        let vendor = BootingVendor::reporting(vec![
            horsie_runtime_host::RuntimeProgress::Provisioning {
                detail: "running the provision steps".into(),
            },
            horsie_runtime_host::RuntimeProgress::Ready(Arc::new(StubHandle)),
        ]);
        let m = manager(published_vendor(vendor));
        let (tx, mut rx) = tokio::sync::mpsc::channel(NARRATION_BUFFER);
        m.get(
            "s1",
            "i1",
            "v",
            &SessionSpec::for_vendor("v"),
            false,
            Some(tx),
        )
        .await
        .expect("get");

        let mut said = Vec::new();
        while let Ok(line) = rx.try_recv() {
            said.push(line);
        }
        assert_eq!(
            said,
            vec![
                // What `get` returned: the first observation is an observation
                // like any other.
                "the machine is up; waiting for it to dial back".to_string(),
                "running the provision steps".to_string(),
            ],
            "every non-terminal state the fold walked through has to be said"
        );
    }

    /// Being ready is not news, and neither is being gone: one is the end of
    /// the wait and the other is the error the caller is about to be handed.
    #[tokio::test]
    async fn an_outcome_is_not_narration() {
        let m = manager(published_vendor(BootingVendor::with(
            horsie_runtime_host::RuntimeProgress::Gone {
                reason: "the machine never dialed back".into(),
            },
        )));
        let (tx, mut rx) = tokio::sync::mpsc::channel(NARRATION_BUFFER);
        let _ = m
            .get(
                "s1",
                "i1",
                "v",
                &SessionSpec::for_vendor("v"),
                false,
                Some(tx),
            )
            .await;
        let mut said = Vec::new();
        while let Ok(line) = rx.try_recv() {
            said.push(line);
        }
        assert_eq!(
            said,
            vec!["the machine is up; waiting for it to dial back".to_string()],
            "the reason a runtime is gone travels as the error, not as progress"
        );
    }

    #[tokio::test]
    async fn provider_is_a_thin_handle_over_the_same_calls() {
        let agent = FakeRuntimeVendor::builder("v")
            .serve_in_process()
            .await
            .unwrap();
        let m = manager(published(&agent, "v"));
        m.create("s1", "i1", "v", &session_spec("v"))
            .await
            .expect("create");
        let provider = m.provider(
            "s1".to_string(),
            "i1".to_string(),
            false,
            "v".to_string(),
            SessionSpec::for_vendor("v"),
        );
        provider.get(None).await.expect("provider get");
        assert_eq!(
            agent.signals(),
            vec!["create:s1".to_string(), "get:s1".to_string()]
        );
    }
}
