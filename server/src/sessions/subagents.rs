//! The session's subagent tree: which agent spawned which, and what became of
//! each. Pure data — the session actor folds its journal events through these
//! methods, so live operation and recovery follow the exact same path.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Deepest node the tree may hold. A node *at* this depth cannot spawn.
pub const MAX_SUBAGENT_DEPTH: u32 = 4;

/// Cap on concurrently-active subagents when the session's settings name none.
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: u32 = 8;

/// Error recorded for a subagent that was mid-run when the process died.
pub const INTERRUPTED_ERROR: &str = "interrupted by restart";

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
    pub output: Option<String>,
    pub error: Option<String>,
    /// Whether the parent was sent this node's latest terminal result. Every
    /// completion resets it; every actual send re-marks it — that pair is what
    /// makes notification delivery exactly-once across offloads.
    pub notified: bool,
}

/// The whole tree, keyed by subagent id. Iteration order is uuid order —
/// stable, which is all merged-notification rendering needs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubAgentTree {
    nodes: BTreeMap<Uuid, SubAgentRecord>,
}

/// The message injected into a parent when a child reaches a terminal state.
pub fn notification_text(label: &str, output: Option<&str>, error: Option<&str>) -> String {
    match (output, error) {
        (Some(output), _) => {
            format!(
                "[subagent \"{label}\" completed]\n\n{}",
                truncate_result(output)
            )
        }
        (None, Some(error)) => {
            format!(
                "[subagent \"{label}\" failed]\n\n{}",
                truncate_result(error)
            )
        }
        (None, None) => format!("[subagent \"{label}\" completed]"),
    }
}

impl SubAgentTree {
    pub fn apply_spawned(
        &mut self,
        id: Uuid,
        parent: SubAgentParent,
        label: String,
        task: String,
        depth: u32,
    ) {
        self.nodes.insert(
            id,
            SubAgentRecord {
                parent,
                label,
                task,
                depth,
                status: SubAgentStatus::Running,
                output: None,
                error: None,
                notified: false,
            },
        );
    }

    /// A terminal node started another run (woken to consume child results).
    pub fn apply_running(&mut self, id: Uuid) {
        if let Some(rec) = self.nodes.get_mut(&id) {
            rec.status = SubAgentStatus::Running;
        }
    }

    pub fn apply_completed(&mut self, id: Uuid, output: String) {
        if let Some(rec) = self.nodes.get_mut(&id) {
            rec.status = SubAgentStatus::Completed;
            rec.output = Some(output);
            rec.error = None;
            rec.notified = false;
        }
    }

    pub fn apply_failed(&mut self, id: Uuid, error: String) {
        if let Some(rec) = self.nodes.get_mut(&id) {
            rec.status = SubAgentStatus::Failed;
            rec.error = Some(error);
            rec.notified = false;
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

    /// Terminal results `parent` has not been sent yet, rendered for injection.
    pub fn owed_for(&self, parent: SubAgentParent) -> Vec<(Uuid, String)> {
        self.nodes
            .iter()
            .filter(|(_, rec)| {
                rec.parent == parent && rec.status != SubAgentStatus::Running && !rec.notified
            })
            .map(|(id, rec)| {
                (
                    *id,
                    notification_text(&rec.label, rec.output.as_deref(), rec.error.as_deref()),
                )
            })
            .collect()
    }

    /// Owed results grouped by their subagent parent (Main excluded — the main
    /// agent's owed results merge into its next turn instead of a wake).
    pub fn owed_by_sub_parent(&self) -> BTreeMap<Uuid, Vec<(Uuid, String)>> {
        let mut grouped: BTreeMap<Uuid, Vec<(Uuid, String)>> = BTreeMap::new();
        for (id, rec) in &self.nodes {
            if rec.status == SubAgentStatus::Running || rec.notified {
                continue;
            }
            if let SubAgentParent::SubAgent(parent) = rec.parent {
                grouped.entry(parent).or_default().push((
                    *id,
                    notification_text(&rec.label, rec.output.as_deref(), rec.error.as_deref()),
                ));
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
        tree.apply_spawned(id, parent, "label".into(), "task".into(), depth);
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
        tree.apply_completed(id, "done".into());
        assert_eq!(tree.active_count(), 0);
        // Terminal and not yet notified → owed to Main.
        let owed = tree.owed_for(SubAgentParent::Main);
        assert_eq!(owed.len(), 1);
        assert!(owed[0].1.contains("done"));
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
        tree.apply_completed(id, "first".into());
        tree.apply_notified(id);
        tree.apply_running(id);
        tree.apply_completed(id, "second".into());
        let owed = tree.owed_for(SubAgentParent::Main);
        assert_eq!(owed.len(), 1);
        assert!(owed[0].1.contains("second"));
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
        tree.apply_completed(child, "kid done".into());
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
        tree.apply_completed(a, "ok".into());
        assert_eq!(tree.interrupted(), vec![b]);
    }

    #[test]
    fn renders_a_node_and_a_subtree() {
        let mut tree = SubAgentTree::default();
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        spawn(&mut tree, parent, SubAgentParent::Main, 1);
        spawn(&mut tree, child, SubAgentParent::SubAgent(parent), 2);
        tree.apply_failed(child, "boom".into());
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

    #[test]
    fn notification_text_shapes_completed_and_failed() {
        assert_eq!(
            notification_text("research", Some("answer"), None),
            "[subagent \"research\" completed]\n\nanswer"
        );
        assert_eq!(
            notification_text("research", None, Some("boom")),
            "[subagent \"research\" failed]\n\nboom"
        );
    }

    #[test]
    fn notification_text_caps_a_huge_result() {
        let huge = "x".repeat(MAX_RESULT_BYTES + 10_000);
        let text = notification_text("w", Some(&huge), None);
        assert!(text.contains("[truncated:"), "{text:.200}");
        assert!(text.len() < huge.len());
        // No mid-character split: the kept prefix is valid and bounded.
        assert!(text.len() <= MAX_RESULT_BYTES + 100);
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
