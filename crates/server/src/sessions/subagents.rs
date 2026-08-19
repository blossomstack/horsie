//! The session's subagent tree: which agent spawned which, and what became of
//! each. Pure data — the session actor folds its journal events through these
//! methods, so live operation and recovery follow the exact same path.

use horsie_models::agent::SubAgentResultPart;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Deepest node the tree may hold. A node *at* this depth cannot spawn.
pub use crate::sessions::runners::MAX_SUBAGENT_DEPTH;

/// Cap on concurrently-active subagents when the session's settings name none.
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: u32 = 8;

/// Error recorded for a subagent that was mid-run when the process died.
pub const INTERRUPTED_ERROR: &str = "interrupted by restart";

/// Error recorded for a subagent someone stopped.
///
/// Its own wording rather than [`INTERRUPTED_ERROR`]'s, because this one reaches
/// a *model*: the parent reads it as the result of the child it is waiting on,
/// and "interrupted by restart" would have it reason about a crash that never
/// happened.
pub use crate::sessions::runners::message::STOPPED_ERROR;

/// Largest result (output or error) injected into a parent's context or
/// rendered by `subagent_status` — the same bound the runtime puts on a
/// tool's streamed output.
pub const MAX_RESULT_BYTES: usize = 50_000;

/// Cap a result for injection/rendering, marking the cut so the reader knows
/// the answer continues elsewhere (the full transcript is always in the
/// subagent's own history).
fn truncate_result(text: &str) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_RESULT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…\n\n[truncated: {} bytes total]",
        &text[..end],
        text.len()
    )
}

/// Who spawned a subagent: the session's main agent, or another subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SubAgentParent {
    Main,
    SubAgent(Uuid),
}

/// Lifecycle of one node. `Completed`/`Failed` are turn-terminal, not
/// actor-terminal: a node with children may wake again to consume their
/// results and conclude a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubAgentStatus {
    Running,
    Completed,
    Failed,
}

/// One tree node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubAgentRecord {
    pub parent: SubAgentParent,
    pub label: String,
    pub task: String,
    pub depth: u32,
    pub status: SubAgentStatus,
    /// The plugin-declared agent type this node runs as, if any. `None` is the
    /// general-purpose subagent — and is what every node journaled before typed
    /// agents existed reads as.
    #[serde(default)]
    pub agent_type: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    /// Whether the parent was sent this node's latest terminal result. Every
    /// completion resets it; every actual send re-marks it — that pair is what
    /// makes notification delivery exactly-once across offloads.
    pub notified: bool,
    /// When the node was spawned, and when it reached its current terminal
    /// state. Zero on rows journaled before these fields existed — a client
    /// then shows no duration rather than one it made up.
    #[serde(default)]
    pub spawned_at_ms: u64,
    #[serde(default)]
    pub ended_at_ms: u64,
}

/// The whole tree, keyed by subagent id. Iteration order is uuid order —
/// stable, which is all merged-notification rendering needs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubAgentTree {
    nodes: BTreeMap<Uuid, SubAgentRecord>,
}

/// What a parent is handed when a child reaches a terminal state.
///
/// A structured part rather than a rendered string: a client needs to tell a
/// subagent's report from what the person typed, and the two used to arrive as
/// one indistinguishable blob of text. The providers flatten it back through
/// [`SubAgentResultPart::to_wire_text`], so the model's view is unchanged.
fn result_part(id: &Uuid, rec: &SubAgentRecord) -> SubAgentResultPart {
    // Status is the source of truth, not which of output/error happens to be
    // populated: a node that completed once and failed on a later cycle still
    // holds the earlier output.
    let (status, body) = match rec.status {
        SubAgentStatus::Failed => ("failed", rec.error.as_deref().unwrap_or_default()),
        SubAgentStatus::Running | SubAgentStatus::Completed => {
            ("completed", rec.output.as_deref().unwrap_or_default())
        }
    };
    SubAgentResultPart {
        subagent_id: id.to_string(),
        label: rec.label.clone(),
        status: status.to_string(),
        text: truncate_result(body),
        spawned_at_ms: rec.spawned_at_ms,
        ended_at_ms: rec.ended_at_ms,
    }
}

impl SubAgentTree {
    #[allow(clippy::too_many_arguments)]
    pub fn apply_spawned(
        &mut self,
        id: Uuid,
        parent: SubAgentParent,
        label: String,
        task: String,
        depth: u32,
        at_ms: u64,
        agent_type: Option<String>,
    ) {
        self.nodes.insert(
            id,
            SubAgentRecord {
                parent,
                label,
                task,
                depth,
                status: SubAgentStatus::Running,
                agent_type,
                output: None,
                error: None,
                notified: false,
                spawned_at_ms: at_ms,
                ended_at_ms: 0,
            },
        );
    }

    /// A terminal node started another run (woken to consume child results).
    /// The span restarts with it: a node that concludes twice reports the cycle
    /// the parent is being told about, not the whole life of the node.
    pub fn apply_running(&mut self, id: Uuid, at_ms: u64) {
        if let Some(rec) = self.nodes.get_mut(&id) {
            rec.status = SubAgentStatus::Running;
            rec.spawned_at_ms = at_ms;
            rec.ended_at_ms = 0;
        }
    }

    pub fn apply_completed(&mut self, id: Uuid, output: String, at_ms: u64) {
        if let Some(rec) = self.nodes.get_mut(&id) {
            rec.status = SubAgentStatus::Completed;
            rec.output = Some(output);
            rec.error = None;
            rec.notified = false;
            rec.ended_at_ms = at_ms;
        }
    }

    pub fn apply_failed(&mut self, id: Uuid, error: String, at_ms: u64) {
        if let Some(rec) = self.nodes.get_mut(&id) {
            rec.status = SubAgentStatus::Failed;
            rec.error = Some(error);
            rec.notified = false;
            rec.ended_at_ms = at_ms;
        }
    }

    pub fn apply_notified(&mut self, id: Uuid) {
        if let Some(rec) = self.nodes.get_mut(&id) {
            rec.notified = true;
        }
    }

    pub fn get(&self, id: &Uuid) -> Option<&SubAgentRecord> {
        self.nodes.get(id)
    }

    /// Whether this tree holds no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Every node id, for re-spawning resident actors at recovery.
    pub fn ids(&self) -> Vec<Uuid> {
        self.nodes.keys().copied().collect()
    }

    /// The depth a parent sits at: the main agent is 0, its children 1.
    /// `None` for an unknown subagent — a caller that is not in the tree.
    pub fn depth_of(&self, parent: SubAgentParent) -> Option<u32> {
        match parent {
            SubAgentParent::Main => Some(0),
            SubAgentParent::SubAgent(id) => self.nodes.get(&id).map(|rec| rec.depth),
        }
    }

    /// Nodes currently mid-run — the concurrency limit's measure.
    pub fn active_count(&self) -> u32 {
        self.nodes
            .values()
            .filter(|rec| rec.status == SubAgentStatus::Running)
            .count() as u32
    }

    pub fn has_active(&self) -> bool {
        self.nodes
            .values()
            .any(|rec| rec.status == SubAgentStatus::Running)
    }

    pub fn is_running(&self, id: &Uuid) -> bool {
        self.nodes
            .get(id)
            .is_some_and(|rec| rec.status == SubAgentStatus::Running)
    }

    /// Nodes still `Running` — at recovery, ones the process died under.
    pub fn interrupted(&self) -> Vec<Uuid> {
        self.nodes
            .iter()
            .filter(|(_, rec)| rec.status == SubAgentStatus::Running)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Terminal results `parent` has not been sent yet.
    pub fn owed_for(&self, parent: SubAgentParent) -> Vec<(Uuid, SubAgentResultPart)> {
        self.nodes
            .iter()
            .filter(|(_, rec)| {
                rec.parent == parent && rec.status != SubAgentStatus::Running && !rec.notified
            })
            .map(|(id, rec)| (*id, result_part(id, rec)))
            .collect()
    }

    /// Owed results grouped by their subagent parent (Main excluded — the main
    /// agent's owed results ride its next turn instead of a wake).
    pub fn owed_by_sub_parent(&self) -> BTreeMap<Uuid, Vec<(Uuid, SubAgentResultPart)>> {
        let mut grouped: BTreeMap<Uuid, Vec<(Uuid, SubAgentResultPart)>> = BTreeMap::new();
        for (id, rec) in &self.nodes {
            if rec.status == SubAgentStatus::Running || rec.notified {
                continue;
            }
            if let SubAgentParent::SubAgent(parent) = rec.parent {
                grouped
                    .entry(parent)
                    .or_default()
                    .push((*id, result_part(id, rec)));
            }
        }
        grouped
    }

    /// One node's detail for the `subagent_status` tool.
    pub fn render_node(&self, id: &Uuid) -> Option<String> {
        let rec = self.nodes.get(id)?;
        let status = match rec.status {
            SubAgentStatus::Running => "running",
            SubAgentStatus::Completed => "completed",
            SubAgentStatus::Failed => "failed",
        };
        let mut out = format!(
            "subagent \"{}\" ({id}) — {status}, depth {}",
            rec.label, rec.depth
        );
        if let Some(output) = &rec.output {
            out.push_str(&format!("\n\noutput:\n{}", truncate_result(output)));
        }
        if let Some(error) = &rec.error {
            out.push_str(&format!("\n\nerror:\n{}", truncate_result(error)));
        }
        Some(out)
    }

    /// Whether `caller` may inspect node `id`: the main agent sees the whole
    /// tree; a subagent sees itself and its own subtree, never siblings.
    pub fn visible_to(&self, caller: SubAgentParent, id: &Uuid) -> bool {
        match caller {
            SubAgentParent::Main => self.nodes.contains_key(id),
            SubAgentParent::SubAgent(root) => *id == root || self.descends_from(id, root),
        }
    }

    /// The subtree under `from` as an indented list, for `subagent_status`
    /// with no id. Root nodes (parent = Main) sit at indent zero when `from`
    /// is Main.
    pub fn render_subtree(&self, from: SubAgentParent) -> String {
        let mut out = String::new();
        let base = match from {
            SubAgentParent::Main => 0,
            SubAgentParent::SubAgent(id) => self.nodes.get(&id).map(|r| r.depth).unwrap_or(0),
        };
        for (id, rec) in &self.nodes {
            let dominated = match from {
                SubAgentParent::Main => true,
                // The caller's descendants — the caller itself is not its own
                // subagent, so the root node is excluded.
                SubAgentParent::SubAgent(root) => *id != root && self.descends_from(id, root),
            };
            if !dominated {
                continue;
            }
            let status = match rec.status {
                SubAgentStatus::Running => "running",
                SubAgentStatus::Completed => "completed",
                SubAgentStatus::Failed => "failed",
            };
            let indent = "  ".repeat(rec.depth.saturating_sub(base).saturating_sub(1) as usize);
            out.push_str(&format!("{indent}- \"{}\" ({id}) [{status}]\n", rec.label));
        }
        if out.is_empty() {
            out.push_str("No subagents.\n");
        }
        out
    }

    /// Every distinct parent in this tree, for a caller walking owed results.
    pub fn parents(&self) -> Vec<SubAgentParent> {
        let mut seen: Vec<SubAgentParent> = self.nodes.values().map(|r| r.parent).collect();
        seen.sort();
        seen.dedup();
        seen
    }

    fn descends_from(&self, id: &Uuid, root: Uuid) -> bool {
        let mut cur = *id;
        loop {
            if cur == root {
                return true;
            }
            match self.nodes.get(&cur).map(|rec| rec.parent) {
                Some(SubAgentParent::SubAgent(parent)) => cur = parent,
                _ => return false,
            }
        }
    }
}

/// Which agent roots a subagent tree. A conversation has exactly one; a
/// workflow run has one per step execution, keyed by that step's agent id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TreeOwner {
    Main,
    Step(Uuid),
}

/// One finished subagent's result that its parent has not been sent.
#[derive(Debug, Clone, PartialEq)]
pub struct OwedResult {
    pub child: Uuid,
    pub parent: SubAgentParent,
    pub owner: TreeOwner,
    pub part: SubAgentResultPart,
}

/// Every subagent this session holds, whatever kind of session it is.
///
/// Keyed by owner rather than nested inside the session's mode, which is the
/// whole point. The previous shape put the tree *inside* `SessionModeState`, so
/// it needed two accessors: one that returned the conversation's tree and one
/// that spanned a run's per-step trees. Every write used the second and every
/// read used the first, which returned an empty tree for a run — so in a
/// workflow a subagent's outcome was dropped, the concurrency cap was
/// unenforced, and an offload could unload a session with a step's subagent
/// mid-run.
///
/// Here there is no accessor that can see one kind's subagents and miss
/// another's. The aggregates below span every tree by construction, so they are
/// right for a run the day they are written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubAgentForest {
    trees: BTreeMap<TreeOwner, SubAgentTree>,
}

impl SubAgentForest {
    pub fn tree(&self, owner: TreeOwner) -> Option<&SubAgentTree> {
        self.trees.get(&owner)
    }

    /// The owner's tree, created on first spawn.
    pub fn tree_mut(&mut self, owner: TreeOwner) -> &mut SubAgentTree {
        self.trees.entry(owner).or_default()
    }

    /// Which tree holds this node.
    pub fn owner_of(&self, node: Uuid) -> Option<TreeOwner> {
        self.trees
            .iter()
            .find(|(_, t)| t.get(&node).is_some())
            .map(|(owner, _)| *owner)
    }

    /// The tree a spawn by `caller` belongs in. `root` is what this session's
    /// own "Main" means right now — [`TreeOwner::Main`] for a conversation, the
    /// step in flight for a run. It is the only kind-shaped fact the forest is
    /// ever told, and it arrives as a value rather than as a branch.
    pub fn owner_for(&self, caller: SubAgentParent, root: TreeOwner) -> Option<TreeOwner> {
        match caller {
            SubAgentParent::Main => Some(root),
            SubAgentParent::SubAgent(id) => self.owner_of(id),
        }
    }

    pub fn node(&self, id: Uuid) -> Option<&SubAgentRecord> {
        self.trees.values().find_map(|t| t.get(&id))
    }

    /// Every node id in the session, for re-spawning resident actors.
    pub fn ids(&self) -> Vec<Uuid> {
        self.trees.values().flat_map(SubAgentTree::ids).collect()
    }

    pub fn is_running(&self, id: Uuid) -> bool {
        self.node(id)
            .is_some_and(|rec| rec.status == SubAgentStatus::Running)
    }

    // --- whole-forest aggregates ---

    /// Nodes mid-run anywhere in the session — the concurrency limit's measure.
    pub fn active_count(&self) -> u32 {
        self.trees.values().map(SubAgentTree::active_count).sum()
    }

    /// Whether anything is mid-run. What decides an offload is unsafe.
    pub fn has_active(&self) -> bool {
        self.trees.values().any(SubAgentTree::has_active)
    }

    /// Nodes still `Running` — at recovery, ones the process died under.
    pub fn interrupted(&self) -> Vec<Uuid> {
        self.trees
            .values()
            .flat_map(SubAgentTree::interrupted)
            .collect()
    }

    /// Every terminal result no parent has been sent, across every tree.
    pub fn owed(&self) -> Vec<OwedResult> {
        let mut out = Vec::new();
        for (owner, tree) in &self.trees {
            for parent in tree.parents() {
                for (child, part) in tree.owed_for(parent) {
                    out.push(OwedResult {
                        child,
                        parent,
                        owner: *owner,
                        part,
                    });
                }
            }
        }
        out
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

    fn spawn(tree: &mut SubAgentTree, id: Uuid, parent: SubAgentParent, depth: u32) {
        tree.apply_spawned(id, parent, "label".into(), "task".into(), depth, 100, None);
    }

    #[test]
    fn spawn_fold_records_parent_depth_and_status() {
        let mut tree = SubAgentTree::default();
        let id = Uuid::new_v4();
        spawn(&mut tree, id, SubAgentParent::Main, 1);
        let rec = tree.get(&id).unwrap();
        assert_eq!(rec.parent, SubAgentParent::Main);
        assert_eq!(rec.depth, 1);
        assert_eq!(rec.status, SubAgentStatus::Running);
        assert!(tree.has_active());
        assert_eq!(tree.active_count(), 1);
    }

    #[test]
    fn completed_then_notified_makes_a_node_not_owed() {
        let mut tree = SubAgentTree::default();
        let id = Uuid::new_v4();
        spawn(&mut tree, id, SubAgentParent::Main, 1);
        tree.apply_completed(id, "done".into(), 400);
        assert_eq!(tree.active_count(), 0);
        // Terminal and not yet notified → owed to Main.
        let owed = tree.owed_for(SubAgentParent::Main);
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].1.text, "done");
        tree.apply_notified(id);
        assert!(tree.owed_for(SubAgentParent::Main).is_empty());
    }

    #[test]
    fn a_second_completion_re_owes_the_parent() {
        // A parent subagent wakes when its children finish, then concludes
        // again: every completion is a fresh result its own parent must hear.
        let mut tree = SubAgentTree::default();
        let id = Uuid::new_v4();
        spawn(&mut tree, id, SubAgentParent::Main, 1);
        tree.apply_completed(id, "first".into(), 400);
        tree.apply_notified(id);
        tree.apply_running(id, 500);
        tree.apply_completed(id, "second".into(), 900);
        let owed = tree.owed_for(SubAgentParent::Main);
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].1.text, "second");
    }

    #[test]
    fn depth_of_main_is_zero_and_unknown_parent_is_none() {
        let mut tree = SubAgentTree::default();
        assert_eq!(tree.depth_of(SubAgentParent::Main), Some(0));
        assert_eq!(
            tree.depth_of(SubAgentParent::SubAgent(Uuid::new_v4())),
            None
        );
        let id = Uuid::new_v4();
        spawn(&mut tree, id, SubAgentParent::Main, 1);
        assert_eq!(tree.depth_of(SubAgentParent::SubAgent(id)), Some(1));
    }

    #[test]
    fn owed_groups_by_sub_parent() {
        let mut tree = SubAgentTree::default();
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        spawn(&mut tree, parent, SubAgentParent::Main, 1);
        spawn(&mut tree, child, SubAgentParent::SubAgent(parent), 2);
        tree.apply_completed(child, "kid done".into(), 400);
        let grouped = tree.owed_by_sub_parent();
        assert_eq!(grouped.len(), 1);
        let (pid, owed) = grouped.into_iter().next().unwrap();
        assert_eq!(pid, parent);
        assert_eq!(owed[0].0, child);
    }

    #[test]
    fn interrupted_lists_running_nodes() {
        let mut tree = SubAgentTree::default();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        spawn(&mut tree, a, SubAgentParent::Main, 1);
        spawn(&mut tree, b, SubAgentParent::Main, 1);
        tree.apply_completed(a, "ok".into(), 400);
        assert_eq!(tree.interrupted(), vec![b]);
    }

    #[test]
    fn renders_a_node_and_a_subtree() {
        let mut tree = SubAgentTree::default();
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        spawn(&mut tree, parent, SubAgentParent::Main, 1);
        spawn(&mut tree, child, SubAgentParent::SubAgent(parent), 2);
        tree.apply_failed(child, "boom".into(), 400);
        let node = tree.render_node(&child).unwrap();
        assert!(node.contains("boom"), "{node}");
        assert!(node.contains("failed"), "{node}");
        let subtree = tree.render_subtree(SubAgentParent::SubAgent(parent));
        assert!(subtree.contains("label"), "{subtree}");
        assert!(
            tree.render_subtree(SubAgentParent::SubAgent(child))
                .contains("No subagents")
        );
    }

    /// The part is what a client reads; `to_wire_text` is what the model still
    /// reads. Both are pinned, because the two used to be the same string.
    #[test]
    fn an_owed_part_shapes_completed_and_failed() {
        let mut tree = SubAgentTree::default();
        let done = Uuid::new_v4();
        let boom = Uuid::new_v4();
        tree.apply_spawned(
            done,
            SubAgentParent::Main,
            "research".into(),
            "t".into(),
            1,
            100,
            None,
        );
        tree.apply_spawned(
            boom,
            SubAgentParent::Main,
            "research".into(),
            "t".into(),
            1,
            100,
            None,
        );
        tree.apply_completed(done, "answer".into(), 400);
        tree.apply_failed(boom, "boom".into(), 400);
        let owed = tree.owed_for(SubAgentParent::Main);
        let by_status = |s: &str| {
            owed.iter()
                .find(|(_, p)| p.status == s)
                .map(|(_, p)| p.clone())
                .unwrap()
        };
        let completed = by_status("completed");
        assert_eq!(completed.text, "answer");
        assert_eq!(
            completed.to_wire_text(),
            "[subagent \"research\" completed]\n\nanswer"
        );
        let failed = by_status("failed");
        assert_eq!(failed.text, "boom");
        assert_eq!(
            failed.to_wire_text(),
            "[subagent \"research\" failed]\n\nboom"
        );
    }

    /// A node that concluded once and failed on a later cycle still holds the
    /// earlier output. Status decides which body the parent hears, so the stale
    /// success cannot mask the failure.
    #[test]
    fn a_later_failure_reports_the_failure_not_the_earlier_output() {
        let mut tree = SubAgentTree::default();
        let id = Uuid::new_v4();
        spawn(&mut tree, id, SubAgentParent::Main, 1);
        tree.apply_completed(id, "first pass".into(), 400);
        tree.apply_notified(id);
        tree.apply_running(id, 500);
        tree.apply_failed(id, "second pass blew up".into(), 900);
        let owed = tree.owed_for(SubAgentParent::Main);
        assert_eq!(owed[0].1.status, "failed");
        assert_eq!(owed[0].1.text, "second pass blew up");
    }

    #[test]
    fn a_terminal_node_records_when_it_started_and_finished() {
        let mut tree = SubAgentTree::default();
        let id = Uuid::new_v4();
        spawn(&mut tree, id, SubAgentParent::Main, 1);
        tree.apply_completed(id, "done".into(), 400);
        let owed = tree.owed_for(SubAgentParent::Main);
        assert_eq!((owed[0].1.spawned_at_ms, owed[0].1.ended_at_ms), (100, 400));
        assert_eq!(owed[0].1.subagent_id, id.to_string());
        assert_eq!(owed[0].1.label, "label");
    }

    /// A woken node reports the cycle its parent is being told about, not the
    /// whole life of the node — otherwise a parent that waits an hour between
    /// wakes reads as an hour of work.
    #[test]
    fn a_second_cycle_reports_its_own_span() {
        let mut tree = SubAgentTree::default();
        let id = Uuid::new_v4();
        spawn(&mut tree, id, SubAgentParent::Main, 1);
        tree.apply_completed(id, "first".into(), 400);
        tree.apply_notified(id);
        tree.apply_running(id, 5_000);
        tree.apply_completed(id, "second".into(), 5_200);
        let owed = tree.owed_for(SubAgentParent::Main);
        assert_eq!(
            (owed[0].1.spawned_at_ms, owed[0].1.ended_at_ms),
            (5_000, 5_200)
        );
    }

    /// Rows journaled before spans were kept must still load.
    #[test]
    fn a_record_without_timestamps_deserializes() {
        let json = r#"{"parent":"Main","label":"a","task":"t","depth":1,
            "status":"Completed","output":"o","error":null,"notified":false}"#;
        let rec: SubAgentRecord = serde_json::from_str(json).unwrap();
        assert_eq!((rec.spawned_at_ms, rec.ended_at_ms), (0, 0));
    }

    #[test]
    fn an_owed_part_caps_a_huge_result() {
        let mut tree = SubAgentTree::default();
        let id = Uuid::new_v4();
        let huge = "x".repeat(MAX_RESULT_BYTES + 10_000);
        tree.apply_spawned(
            id,
            SubAgentParent::Main,
            "w".into(),
            "t".into(),
            1,
            100,
            None,
        );
        tree.apply_completed(id, huge.clone(), 400);
        let text = tree.owed_for(SubAgentParent::Main)[0].1.to_wire_text();
        assert!(text.contains("[truncated:"), "{text:.200}");
        assert!(text.len() < huge.len());
        // No mid-character split: the kept prefix is valid and bounded.
        assert!(text.len() <= MAX_RESULT_BYTES + 100);
    }

    fn forest_with_two_trees() -> (SubAgentForest, Uuid, Uuid, Uuid) {
        let mut f = SubAgentForest::default();
        let step = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        f.tree_mut(TreeOwner::Main).apply_spawned(
            a,
            SubAgentParent::Main,
            "a".into(),
            "t".into(),
            1,
            100,
            None,
        );
        f.tree_mut(TreeOwner::Step(step)).apply_spawned(
            b,
            SubAgentParent::Main,
            "b".into(),
            "t".into(),
            1,
            100,
            None,
        );
        (f, step, a, b)
    }

    /// The five queries that were wrong before the forest: each has to span
    /// every tree, or a workflow run answers as though it had no subagents.
    #[test]
    fn aggregates_span_every_tree() {
        let (f, _step, a, b) = forest_with_two_trees();
        assert_eq!(f.active_count(), 2);
        assert!(f.has_active());
        let mut interrupted = f.interrupted();
        interrupted.sort();
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(interrupted, expected);
    }

    #[test]
    fn a_node_is_found_whichever_tree_holds_it() {
        let (f, step, a, b) = forest_with_two_trees();
        assert_eq!(f.node(a).unwrap().label, "a");
        assert_eq!(f.node(b).unwrap().label, "b");
        assert_eq!(f.owner_of(a), Some(TreeOwner::Main));
        assert_eq!(f.owner_of(b), Some(TreeOwner::Step(step)));
        assert_eq!(f.owner_of(Uuid::new_v4()), None);
    }

    #[test]
    fn owed_results_carry_the_tree_that_owes_them() {
        let (mut f, step, _a, b) = forest_with_two_trees();
        f.tree_mut(TreeOwner::Step(step))
            .apply_completed(b, "done".into(), 400);
        let owed = f.owed();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].child, b);
        assert_eq!(owed[0].parent, SubAgentParent::Main);
        assert_eq!(owed[0].owner, TreeOwner::Step(step));
        assert_eq!(owed[0].part.text, "done");
    }

    /// A step's spawn belongs in that step's tree; a subagent's belongs in
    /// whichever tree already holds the subagent. This is the whole of what the
    /// session has to tell the forest about kinds.
    #[test]
    fn owner_for_resolves_a_caller_against_the_root_in_play() {
        let (f, step, a, _b) = forest_with_two_trees();
        assert_eq!(
            f.owner_for(SubAgentParent::Main, TreeOwner::Step(step)),
            Some(TreeOwner::Step(step))
        );
        assert_eq!(
            f.owner_for(SubAgentParent::SubAgent(a), TreeOwner::Step(step)),
            Some(TreeOwner::Main)
        );
        assert_eq!(
            f.owner_for(SubAgentParent::SubAgent(Uuid::new_v4()), TreeOwner::Main),
            None
        );
    }

    #[test]
    fn an_empty_forest_answers_every_aggregate() {
        let f = SubAgentForest::default();
        assert_eq!(f.active_count(), 0);
        assert!(!f.has_active());
        assert!(f.interrupted().is_empty());
        assert!(f.owed().is_empty());
        assert!(f.ids().is_empty());
    }

    #[test]
    fn a_node_is_visible_to_main_to_itself_and_to_its_ancestors() {
        let mut tree = SubAgentTree::default();
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let other = Uuid::new_v4();
        spawn(&mut tree, parent, SubAgentParent::Main, 1);
        spawn(&mut tree, child, SubAgentParent::SubAgent(parent), 2);
        spawn(&mut tree, other, SubAgentParent::Main, 1);

        assert!(tree.visible_to(SubAgentParent::Main, &child));
        assert!(tree.visible_to(SubAgentParent::SubAgent(parent), &child));
        assert!(tree.visible_to(SubAgentParent::SubAgent(parent), &parent));
        // A sibling branch and an unknown id are not.
        assert!(!tree.visible_to(SubAgentParent::SubAgent(other), &child));
        assert!(!tree.visible_to(SubAgentParent::Main, &Uuid::new_v4()));
    }
}
