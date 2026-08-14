//! What a caller does about a request that will never be answered.
//!
//! A tool call has no natural bound. A file read and a twenty-minute build ride
//! the same `invoke`, so any deadline loose enough for the build is useless for
//! the read, and any deadline tight enough for the read murders the build. The
//! bound was being asked of the one thing in the system that has none.
//!
//! So the timer moves onto something that *does* have a bound: the caller
//! periodically asks the runtime what it is executing, and **the answer is a list
//! of ids rather than a boolean**. That turns liveness into a diff against the
//! caller's own list, with three outcomes and only the first common:
//!
//! - the id is in the runtime's list — it is working. Wait, however long.
//! - the id is absent — it finished a moment ago and its result is in flight, or
//!   it was lost. A short grace, cancelled by a result arriving, then that *one*
//!   request fails.
//! - the ping itself goes unanswered inside its own window — fail *every*
//!   outstanding request for that runtime.
//!
//! Two timers, both fixed constants, neither derived from what a tool is doing.
//!
//! **From the caller's end, not at the connection.** horsie#366 proposed this
//! mechanism with the guardian beside the socket, and had to concede a hole it
//! could not close: the guardian's own host going away. Nothing publishes
//! anything then, so callers still needed a coarse backstop deadline. Run from
//! the caller, one probe traverses the entire path — caller, bus, pump, runtime,
//! and back — so a broken bus and a dead sandbox produce the same missing reply
//! and one mechanism covers both.
//!
//! **Cancelling orphans is a goal here, not a side effect.** Ids the runtime
//! reports that the map does not contain are requests nobody is waiting for —
//! the shape a node restart leaves behind — and they are cancelled. A deadline
//! cannot express this at all: there is no caller left to time anything out.

use horsie_runtime_host::{InFlight, RuntimeTransport};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// How long a runtime may take to answer a ping before every request outstanding
/// against it is failed.
///
/// Bounded because a ping is cheap and answered concurrently by the runtime's
/// dispatcher — unlike a tool, which has no natural bound at all.
const PING_WINDOW: Duration = Duration::from_secs(20);

/// How long an id the runtime no longer reports is given to produce the result
/// already in flight.
///
/// Generous on purpose. A ping snapshotted after a tool finished but before its
/// result was published shows the id missing, so this protects a routine ordering
/// case rather than a rare one.
const VANISHED_GRACE: Duration = Duration::from_secs(30);

/// How often an *active* runtime is asked. An idle one is not asked at all, so
/// this is the cost per executing session rather than per live one.
const PING_INTERVAL: Duration = Duration::from_secs(10);

/// Reconcile one runtime's outstanding calls until the task is aborted.
///
/// Dropped by the manager when the runtime is hibernated or deleted; there is no
/// other exit, because a runtime that stops answering is the case this exists to
/// report rather than a reason to stop looking.
pub async fn reconcile(transport: Arc<dyn RuntimeTransport>, in_flight: Arc<InFlight>) {
    // Ids the runtime stopped reporting, and when we first noticed. Kept across
    // rounds: the grace is a window, not a single observation.
    let mut vanished: HashMap<String, Instant> = HashMap::new();
    let mut probe: u64 = 0;

    loop {
        tokio::time::sleep(PING_INTERVAL).await;

        // An idle runtime costs no messages at all, which is what makes this
        // scale with executing sessions rather than live ones. Nothing can have
        // vanished either, since nothing is outstanding.
        let outstanding = in_flight.all();
        if outstanding.is_empty() {
            vanished.clear();
            continue;
        }

        probe += 1;
        let call_id = format!("ping-{probe}");
        let answer = tokio::time::timeout(PING_WINDOW, transport.ping(&call_id)).await;
        let Ok(Ok(executing)) = answer else {
            // Unanswered, or answered with an error. Either way nothing on this
            // runtime can be waited for any longer, and *every* outstanding call
            // is failed rather than left parked for the life of the process.
            //
            // A dead sandbox and a broken bus are indistinguishable here, and
            // deliberately treated alike: the round trip covers both, so one
            // response covers both.
            tracing::warn!(
                outstanding = outstanding.len(),
                "a runtime did not answer a ping; failing everything outstanding against it"
            );
            for id in &outstanding {
                transport.abandon(id).await;
            }
            vanished.clear();
            continue;
        };

        // Ids the runtime is running that nobody here is waiting for. The shape a
        // node restart leaves behind: the caller is gone, so no deadline could
        // ever have reported them, and they hold a sandbox's resources for
        // output that has nowhere to go.
        for id in &executing {
            if in_flight.issuer_of(id).is_none() {
                tracing::info!(call_id = %id, "cancelling a call the runtime is running for nobody");
                let _ = transport.cancel(id).await;
            }
        }

        let now = Instant::now();
        for id in &outstanding {
            if executing.contains(id) {
                // Still working. Any grace it had accrued is void — this is the
                // ordinary case for a long build, and it must never accumulate
                // its way to a failure.
                vanished.remove(id);
                continue;
            }
            match vanished.get(id) {
                // Its result is very likely already in flight: a ping
                // snapshotted between a tool finishing and its reply being
                // published shows exactly this.
                None => {
                    vanished.insert(id.clone(), now);
                }
                Some(since) if now.duration_since(*since) >= VANISHED_GRACE => {
                    tracing::warn!(
                        call_id = %id,
                        "a runtime stopped reporting this call and no result arrived; failing it"
                    );
                    transport.abandon(id).await;
                    vanished.remove(id);
                }
                Some(_) => {}
            }
        }
        // Ids that were answered while we waited are no longer outstanding, so
        // they must not keep a grace timer alive into a later incarnation of the
        // same id.
        vanished.retain(|id, _| in_flight.issuer_of(id).is_some());
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
    use horsie_runtime_host::{MockTransport, TransportProbe};

    /// Let `d` of the loop's time pass. Every test here runs on a paused clock,
    /// which tokio fast-forwards whenever nothing is runnable — so these sleeps
    /// cost no wall-clock, and the constants above can stay the production ones
    /// rather than being shrunk for the tests that check them.
    async fn elapse(d: Duration) {
        tokio::time::sleep(d).await;
        tokio::task::yield_now().await;
    }

    /// One round has definitely happened, and only one.
    const ONE_ROUND: Duration = Duration::from_secs(15);

    /// The common case, and the one a deadline gets wrong. A build the runtime is
    /// still executing must survive any number of rounds.
    #[tokio::test(start_paused = true)]
    async fn a_long_call_the_runtime_is_still_running_is_never_failed() {
        let transport = Arc::new(MockTransport::ok(""));
        let in_flight = Arc::new(InFlight::new());
        in_flight.track("the-build", "a1");
        *transport.in_flight().lock().unwrap() = vec!["the-build".to_string()];

        let task = tokio::spawn(reconcile(transport.clone(), in_flight.clone()));
        elapse(PING_INTERVAL * 10).await;

        assert_eq!(
            in_flight.issuer_of("the-build").as_deref(),
            Some("a1"),
            "a call the runtime reports as running must never be abandoned"
        );
        task.abort();
    }

    /// An id the runtime stops reporting gets its grace, and only then fails.
    #[tokio::test(start_paused = true)]
    async fn an_id_that_vanishes_is_failed_after_grace() {
        let transport = Arc::new(MockTransport::ok(""));
        let in_flight = Arc::new(InFlight::new());
        in_flight.track("gone", "a1");
        // The runtime reports nothing: the call is not on it any more.

        let task = tokio::spawn(reconcile(transport.clone(), in_flight.clone()));

        elapse(ONE_ROUND).await;
        assert_eq!(
            transport.abandoned(),
            Vec::<String>::new(),
            "the first observation starts the grace, it does not end the call"
        );

        elapse(VANISHED_GRACE + PING_INTERVAL * 2).await;
        assert_eq!(
            transport.abandoned(),
            vec!["gone".to_string()],
            "an id still missing after the grace has to fail"
        );
        task.abort();
    }

    /// The routine ordering case the grace exists for: a ping snapshotted after
    /// the tool finished but before its result was published.
    #[tokio::test(start_paused = true)]
    async fn a_result_landing_during_grace_cancels_the_timer() {
        let transport = Arc::new(MockTransport::ok(""));
        let in_flight = Arc::new(InFlight::new());
        in_flight.track("finishing", "a1");

        let task = tokio::spawn(reconcile(transport.clone(), in_flight.clone()));
        elapse(ONE_ROUND).await;

        // The result arrives: whoever was awaiting it untracks the call.
        in_flight.untrack("finishing");
        elapse(VANISHED_GRACE + PING_INTERVAL * 2).await;

        assert_eq!(
            transport.abandoned(),
            Vec::<String>::new(),
            "a call that was answered must not be failed by a grace it had already left"
        );
        task.abort();
    }

    /// One unanswered ping fails everything, because nothing on that runtime can
    /// be waited for any more — and failing one call at a time would leave the
    /// rest parked for the life of the process.
    #[tokio::test(start_paused = true)]
    async fn one_unanswered_ping_fails_every_outstanding_call() {
        let transport = Arc::new(MockTransport::ok("").swallowing_pings());
        let in_flight = Arc::new(InFlight::new());
        for (call, agent) in [("c1", "a1"), ("c2", "a2"), ("c3", "a1")] {
            in_flight.track(call, agent);
        }

        let task = tokio::spawn(reconcile(transport.clone(), in_flight.clone()));
        elapse(PING_INTERVAL + PING_WINDOW + PING_INTERVAL).await;

        let mut failed = transport.abandoned();
        failed.sort();
        assert_eq!(
            failed,
            vec!["c1".to_string(), "c2".to_string(), "c3".to_string()],
            "every agent's calls fail together: the runtime is unreachable for all of them"
        );
        task.abort();
    }

    /// D8, and the clearest argument for reconciling rather than bounding: there
    /// is no caller left to time this out.
    #[tokio::test(start_paused = true)]
    async fn a_call_the_runtime_runs_for_nobody_is_cancelled() {
        let probe = TransportProbe::default();
        let transport = Arc::new(MockTransport::ok("").observed_by(&probe));
        let in_flight = Arc::new(InFlight::new());
        // Something must be outstanding, or an idle runtime is not polled at all.
        in_flight.track("mine", "a1");
        *transport.in_flight().lock().unwrap() = vec!["mine".to_string(), "orphan-1".to_string()];

        let task = tokio::spawn(reconcile(transport.clone(), in_flight.clone()));
        elapse(PING_INTERVAL * 2).await;

        assert!(
            probe.cancels().contains(&"orphan-1".to_string()),
            "an id nobody is waiting for must be cancelled, got {:?}",
            probe.cancels()
        );
        assert!(
            !probe.cancels().contains(&"mine".to_string()),
            "a call somebody *is* waiting for must not be"
        );
        task.abort();
    }

    /// An idle runtime costs nothing, which is what makes the traffic scale with
    /// executing sessions rather than live ones.
    #[tokio::test(start_paused = true)]
    async fn an_idle_runtime_is_not_pinged() {
        let transport = Arc::new(MockTransport::ok(""));
        let in_flight = Arc::new(InFlight::new());

        let task = tokio::spawn(reconcile(transport.clone(), in_flight.clone()));
        elapse(PING_INTERVAL * 3).await;

        assert_eq!(
            transport.pings(),
            0,
            "a runtime with nothing outstanding must not be asked anything"
        );
        task.abort();
    }
}
