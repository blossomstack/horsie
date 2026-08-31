//! The catalogue of built-in tools a selection can govern.
//!
//! One table, three readers: the HTTP catalogue route renders it, the agent's
//! toolbox filter narrows by it, and the session decides from it whether to
//! build the control-plane layer at all.
//!
//! # What "governed" means
//!
//! A selection is an allowlist, but only over the names *in this table*. A tool
//! the catalogue does not know about is passed through untouched — that is what
//! keeps MCP tools, a plugin's MCP tools, the `memory_*` tools and a workflow
//! step's `submit_result` working when someone narrows their selection to three
//! runtime tools. Those are not oversights:
//!
//! - **MCP** tool names do not exist until something has been connected to, and
//!   a plugin's do not exist until a sandbox is running. A selection saved on
//!   Monday cannot honestly speak for them. They are chosen by selecting the
//!   server, which is a name that *is* stable.
//! - **Memory** tools are gated by which spaces the session may touch, which is
//!   a sharper question than whether the tool exists.
//! - **`submit_result`** is how a workflow step returns at all. A run whose step
//!   cannot submit does not fail, it hangs.
//!
//! The alternative — a hand-maintained never-filter list — has to be remembered
//! every time a layer is added. This way round, forgetting means a new tool is
//! ungovernable, which [`tests::every_tool_constant_is_catalogued_or_excused`]
//! fails on: it scans the crate for tool constants rather than trusting anyone
//! to have added one to a list, because a constant nobody thought about is
//! precisely the case a list cannot cover.

use horsie_models::tools::{ToolAccess, ToolCatalog, ToolGroupView, ToolView};
use std::collections::HashSet;

/// Group keys. Public so a caller can ask about one without matching on a
/// string literal that a rename would leave behind.
pub const GROUP_RUNTIME: &str = "runtime";
pub const GROUP_WORKSPACE: &str = "workspace";
pub const GROUP_PLANNING: &str = "planning";
pub const GROUP_TIMERS: &str = "timers";
pub const GROUP_DELEGATION: &str = "delegation";
pub const GROUP_WORKFLOWS: &str = "workflows";
pub const GROUP_SESSION: &str = "session";
pub const GROUP_AUTHORING: &str = "authoring";
pub const GROUP_CONTROL: &str = "control";

/// The prefix every control-plane tool carries. Mirrors `control::toolbox`'s
/// own constant, which is private to that module.
const CONTROL_PREFIX: &str = "horsie_";

/// One row of the static table, before the generated control group is appended.
struct Row {
    name: &'static str,
    description: &'static str,
    access: ToolAccess,
}

const fn read(name: &'static str, description: &'static str) -> Row {
    Row {
        name,
        description,
        access: ToolAccess::Read,
    }
}

const fn write(name: &'static str, description: &'static str) -> Row {
    Row {
        name,
        description,
        access: ToolAccess::Write,
    }
}

/// `bash` is `Write` because it can be, not because it always is. Same
/// reasoning puts `set_working_dir` and `set_env` here: they change what every
/// later call resolves against, which is a side effect that outlives the call.
const RUNTIME: &[Row] = &[
    write("bash", "Run a shell command in the session's runtime."),
    read("read_file", "Read a file, optionally a line range of it."),
    write("write_file", "Create or overwrite a file."),
    write("find_and_replace", "Replace text within a file."),
    write("replace_lines", "Replace a range of lines in a file."),
    read("list_files", "List a directory's contents."),
    read("glob", "Find files by glob pattern."),
    read("grep", "Search file contents by regex."),
    write(
        "set_working_dir",
        "Set the directory later tool calls resolve against.",
    ),
    write("set_env", "Set or unset an environment variable."),
];

const WORKSPACE: &[Row] = &[
    read("skill", "Load a named skill's full instructions."),
    read(
        "inspect_workspace",
        "Re-scan the workspace: paths, git state, available skills.",
    ),
];

const PLANNING: &[Row] = &[write(
    "task_list",
    "Keep a checklist of the work in progress.",
)];

const TIMERS: &[Row] = &[
    write("set_timer", "Sleep, or wake up again on an interval."),
    read("list_timers", "Show this agent's armed timers."),
    write("cancel_timer", "Disarm a timer."),
];

/// Both ways of handing work to another agent, which is why they are one group
/// rather than "subagents" plus a stray. They differ only in where the result
/// goes: a subagent reports back to whoever spawned it, a session talks to
/// the person and never reports back at all.
const DELEGATION: &[Row] = &[
    write(
        "spawn_agent",
        "Delegate work to a subagent and get its result back.",
    ),
    read("subagent_status", "Check on a spawned subagent."),
    write(
        "spawn_subsession",
        "Hand work to a second session the user steers themselves.",
    ),
];

const WORKFLOWS: &[Row] = &[
    write("invoke_workflow", "Start a run of a saved workflow."),
    read("workflow_status", "Check on a workflow run."),
];

const SESSION: &[Row] = &[
    write("set_session_title", "Name this session."),
    write(
        "ask_user",
        "Put a question to the person and wait for an answer.",
    ),
    // Together with `ask_user`, because they are the two halves of talking to
    // the person and differ only in whether the agent stops to hear back. An
    // account that wants neither turns off one group rather than hunting two.
    write(
        crate::sessions::notify_tool::NOTIFY_USER_TOOL,
        "Put a message in the person's inbox without stopping.",
    ),
];

/// Authoring is authority, so this group is out of the default set: a skill
/// written here is loadable by every session the account starts afterwards.
/// Governed rather than gated by a channel of its own — unlike `memory_*`,
/// whose spaces are the sharper question — because the selection is the whole
/// grant and a second flag could only disagree with it.
const AUTHORING: &[Row] = &[
    write(
        crate::plugins::authored::toolbox::PLUGIN_WRITE,
        "Create a plugin to hold skills you author.",
    ),
    write(
        crate::plugins::authored::toolbox::SKILL_WRITE,
        "Write or edit a skill in one of your plugins.",
    ),
    write(
        crate::plugins::authored::toolbox::SKILL_DELETE,
        "Remove a skill you wrote.",
    ),
    read(
        crate::plugins::authored::toolbox::SKILL_LIST,
        "List the skills you have written.",
    ),
    read(
        crate::plugins::authored::toolbox::SKILL_HISTORY,
        "Show a skill's past revisions.",
    ),
    write(
        crate::plugins::authored::toolbox::SKILL_RESTORE,
        "Put a skill back to one of its revisions.",
    ),
];

fn group(
    key: &str,
    label: &str,
    description: &str,
    rows: &[Row],
    in_default_set: bool,
) -> ToolGroupView {
    ToolGroupView {
        key: key.to_string(),
        label: label.to_string(),
        description: description.to_string(),
        tools: rows
            .iter()
            .map(|r| ToolView {
                name: r.name.to_string(),
                description: r.description.to_string(),
                access: r.access.clone(),
                in_default_set,
            })
            .collect(),
    }
}

/// The control group, built from the same `operations()` the tools and the
/// routes are built from, so a resource added there appears here without anyone
/// remembering to add it.
///
/// One tool per resource, each taking an `action`, so a resource is `Read`
/// only when every action it exposes to the model is a `Get`. In practice that
/// is none of them — which is the honest answer, since `horsie_sessions` can
/// create a session.
fn control_group() -> ToolGroupView {
    let mut by_resource: Vec<(&'static str, bool)> = Vec::new();
    for operation in crate::control::operations()
        .iter()
        .filter(|o| o.expose != crate::control::Expose::Api)
    {
        let writes = operation.method != crate::control::Method::Get;
        match by_resource
            .iter_mut()
            .find(|(r, _)| *r == operation.resource)
        {
            Some((_, w)) => *w |= writes,
            None => by_resource.push((operation.resource, writes)),
        }
    }
    ToolGroupView {
        key: GROUP_CONTROL.to_string(),
        // Lowercase, like the product name everywhere else — the heading is
        // rendered uppercase by the picker's own styling, so this is the
        // written form, and the docs lint holds prose to the same rule.
        label: "horsie".to_string(),
        description:
            "Manage this horsie server — its agents, workflows, routines, environments, models \
             and runtimes. Changes take effect immediately and are not confirmed first, so this \
             is authority, not convenience: selecting any of these grants it."
                .to_string(),
        tools: by_resource
            .into_iter()
            .map(|(resource, writes)| ToolView {
                name: control_tool_name(resource),
                // Read from the same fact the access badge is: a resource with
                // no write action cannot be managed, and saying it can is the
                // one thing a one-line description has room to get wrong.
                description: if writes {
                    format!("Read and manage this server's {resource}.")
                } else {
                    format!("Read this server's {resource}.")
                },
                access: if writes {
                    ToolAccess::Write
                } else {
                    ToolAccess::Read
                },
                in_default_set: false,
            })
            .collect(),
    }
}

/// Every built-in tool this server offers, grouped for selection.
#[must_use]
pub fn catalog() -> ToolCatalog {
    ToolCatalog {
        groups: vec![
            group(
                GROUP_RUNTIME,
                "Files & shell",
                "Read and change the files in the session's runtime, and run commands there.",
                RUNTIME,
                true,
            ),
            group(
                GROUP_WORKSPACE,
                "Workspace",
                "See what the workspace holds, and load a skill's instructions on demand.",
                WORKSPACE,
                true,
            ),
            group(
                GROUP_PLANNING,
                "Planning",
                "Track multi-step work as a visible checklist.",
                PLANNING,
                true,
            ),
            group(
                GROUP_TIMERS,
                "Timers",
                "Wait, or wake up on a schedule, without holding a turn open.",
                TIMERS,
                true,
            ),
            group(
                GROUP_DELEGATION,
                "Delegation",
                "Hand a piece of work to another agent — one that reports back, or one \
                 that carries on with the user.",
                DELEGATION,
                true,
            ),
            group(
                GROUP_WORKFLOWS,
                "Workflows",
                "Start and follow runs of the workflows saved on this server.",
                WORKFLOWS,
                true,
            ),
            group(
                GROUP_SESSION,
                "Session",
                "Name the session, and ask the person a question mid-turn.",
                SESSION,
                true,
            ),
            group(
                GROUP_AUTHORING,
                "Authoring",
                "Write skills of your own and keep them, so a later session can load \
                 what you worked out in this one. Anything written here is offered to \
                 every session this account starts, so selecting these grants that.",
                AUTHORING,
                false,
            ),
            control_group(),
        ],
    }
}

/// The tool that manages one control-plane resource.
#[must_use]
pub fn control_tool_name(resource: &str) -> String {
    format!("{CONTROL_PREFIX}{resource}")
}

/// Every name the catalogue governs. A tool outside this set is never filtered.
#[must_use]
pub fn governed() -> HashSet<String> {
    catalog()
        .groups
        .into_iter()
        .flat_map(|g| g.tools)
        .map(|t| t.name)
        .collect()
}

/// What an absent selection means: every group except the control plane.
#[must_use]
pub fn default_set() -> HashSet<String> {
    catalog()
        .groups
        .into_iter()
        .flat_map(|g| g.tools)
        .filter(|t| t.in_default_set)
        .map(|t| t.name)
        .collect()
}

/// The tools a selection permits.
///
/// `None` is the default set rather than "everything", which is what stops an
/// unset field from handing out the control plane. A caller that genuinely
/// wants no tools passes `Some(&[])` — a distinction the wire keeps because
/// absent and empty are different values there.
#[must_use]
pub fn resolve(selection: Option<&[String]>) -> HashSet<String> {
    match selection {
        None => default_set(),
        Some(names) => names.iter().cloned().collect(),
    }
}

/// Whether a selection asks for the control plane. The grant is the selection:
/// there is no separate bit that could disagree with it.
#[must_use]
pub fn grants_control_plane(selection: Option<&[String]>) -> bool {
    resolve(selection)
        .iter()
        .any(|n| n.starts_with(CONTROL_PREFIX))
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

    #[test]
    fn an_absent_selection_never_grants_the_control_plane() {
        assert!(!grants_control_plane(None));
        let default = default_set();
        assert!(default.contains("bash"), "the default set is not empty");
        assert!(
            !default.iter().any(|n| n.starts_with(CONTROL_PREFIX)),
            "an unset selection must not reach the control plane"
        );
    }

    #[test]
    fn naming_a_control_tool_is_the_grant() {
        assert!(grants_control_plane(Some(&["horsie_agents".to_string()])));
        assert!(!grants_control_plane(Some(&["bash".to_string()])));
    }

    #[test]
    fn an_empty_selection_is_not_an_absent_one() {
        assert!(resolve(Some(&[])).is_empty());
        assert!(!resolve(None).is_empty());
    }

    /// A resource the model can reach must be selectable, and one it cannot
    /// must not appear in the picker at all.
    ///
    /// Both directions, because each is a different mistake. A resource that
    /// gained a tool without a catalogue row would be ungovernable; one that is
    /// `Expose::Api` throughout — as `authored-plugins` is, since agents author
    /// through the `authoring` group instead — would otherwise be offered as a
    /// grant that hands out nothing.
    #[test]
    fn a_control_resource_is_catalogued_exactly_when_it_offers_a_tool() {
        let group = control_group();
        let names: HashSet<String> = group.tools.iter().map(|t| t.name.clone()).collect();
        for resource in crate::control::resources() {
            let reachable = resource
                .operations()
                .iter()
                .any(|o| o.expose != crate::control::Expose::Api);
            assert_eq!(
                names.contains(&control_tool_name(resource.name())),
                reachable,
                "control resource '{}' is catalogued as {}, but exposes {} to the model",
                resource.name(),
                if reachable { "absent" } else { "present" },
                if reachable { "a tool" } else { "nothing" }
            );
        }
    }

    /// Every tool the runtime host advertises must be selectable.
    ///
    /// Dynamic on purpose: this is the list that grows, and a tool added to
    /// `add_runtime_tools` without a catalogue row would be one nobody could
    /// ever turn off.
    #[test]
    fn every_runtime_tool_is_catalogued() {
        let client = horsie_runtime_host::RuntimeClient::detached(
            horsie_runtime_host::MockTransport::ok(""),
            "catalogue-test",
        );
        let toolbox =
            horsie_runtime_host::add_runtime_tools(horsie_agentcore::ToolboxImpl::new(), client);
        let catalogued = governed();
        for spec in horsie_agentcore::Toolbox::specs(&toolbox) {
            assert!(
                catalogued.contains(&spec.name),
                "runtime tool '{}' has no row in the catalogue, so no selection \
                 can turn it off",
                spec.name
            );
        }
    }

    /// The tools that live in a layer of their own, anchored to the constant
    /// each layer dispatches on. A rename that misses the catalogue fails here.
    #[test]
    fn every_advertised_tool_is_accounted_for() {
        let catalogued = governed();
        for name in [
            crate::agent_loop::SKILL_TOOL,
            crate::agent_loop::INSPECT_WORKSPACE_TOOL,
            crate::agent_loop::TASK_LIST_TOOL,
            crate::sessions::spawn_tool::SPAWN_AGENT_TOOL,
            crate::sessions::spawn_tool::SUBAGENT_STATUS_TOOL,
            crate::sessions::sub_session_tool::SPAWN_SUBSESSION_TOOL,
            crate::sessions::invoke_workflow_tool::INVOKE_WORKFLOW_TOOL,
            crate::sessions::invoke_workflow_tool::WORKFLOW_STATUS_TOOL,
            crate::sessions::title_tool::SET_SESSION_TITLE_TOOL,
            crate::sessions::ask_tool::ASK_USER_TOOL,
        ] {
            assert!(
                catalogued.contains(name),
                "'{name}' is advertised to models but is not selectable"
            );
        }
        for spec in crate::agent_loop::timer_tool_specs() {
            assert!(
                catalogued.contains(&spec.name),
                "timer tool '{}' is not selectable",
                spec.name
            );
        }
    }

    /// Tools that are advertised but deliberately not selectable, with the
    /// reason. Adding a name here is a decision; the scan below is what forces
    /// it to be made.
    const UNGOVERNED: &[(&str, &str)] = &[(
        "submit_result",
        "how a workflow step returns at all — a run whose step cannot submit \
         does not fail, it hangs",
    )];

    /// Every tool constant in this crate is either catalogued or excused.
    ///
    /// Read from the source rather than from a composed toolbox, because
    /// composing one needs a runtime, a session and an account — and the
    /// failure this is for happens long before any of those exist: somebody
    /// adds a layer with a new tool and never learns that tool selection is a
    /// thing. The list-of-constants test above cannot catch that, since a
    /// constant nobody thought to add to it is exactly the case. Coarse, like
    /// `control::http::tests::every_route_is_classified`, and for the same
    /// reason.
    #[test]
    fn every_tool_constant_is_catalogued_or_excused() {
        let catalogued = governed();
        let excused: HashSet<&str> = UNGOVERNED.iter().map(|(name, _)| *name).collect();
        let mut missing = Vec::new();

        for path in rust_sources(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
            let source = std::fs::read_to_string(&path).expect("read source");
            for name in tool_constants(&source) {
                if !catalogued.contains(&name) && !excused.contains(name.as_str()) {
                    missing.push(format!("{name} ({})", path.display()));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these tools are advertised to models but no selection governs them. \
             Add a row to the catalogue, or name them in UNGOVERNED with the \
             reason: {missing:?}"
        );
    }

    /// Every `.rs` under `dir`.
    fn rust_sources(dir: std::path::PathBuf) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).expect("read dir").flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    out.push(p);
                }
            }
        }
        out
    }

    /// The values of `const <NAME>_TOOL: &str = "...";` declarations.
    fn tool_constants(source: &str) -> Vec<String> {
        source
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let rest = line
                    .strip_prefix("pub const ")
                    .or(line.strip_prefix("const "))?;
                let (name, rest) = rest.split_once(": &str = ")?;
                if !name.ends_with("_TOOL") {
                    return None;
                }
                rest.trim()
                    .strip_prefix('"')?
                    .split('"')
                    .next()
                    .map(str::to_string)
            })
            .collect()
    }

    /// The other half of the contract: these must stay *out*, or narrowing a
    /// selection would silently disable something chosen through a different
    /// channel. See this module's header for why each one.
    #[test]
    fn the_ungoverned_tools_stay_ungoverned() {
        let catalogued = governed();
        for name in [
            "memory_load",
            "memory_create",
            "memory_update",
            "memory_delete",
            "memory_list",
            crate::sessions::workflow::SUBMIT_RESULT_TOOL,
            "mcp__notion__search",
        ] {
            assert!(
                !catalogued.contains(name),
                "'{name}' is governed by tool selection, but it is gated by its \
                 own channel — narrowing a selection would now disable it"
            );
        }
    }

    #[test]
    fn group_keys_and_tool_names_are_unique() {
        let cat = catalog();
        let mut keys = HashSet::new();
        let mut names = HashSet::new();
        for g in cat.groups {
            assert!(keys.insert(g.key.clone()), "duplicate group key {}", g.key);
            for t in g.tools {
                assert!(names.insert(t.name.clone()), "duplicate tool {}", t.name);
            }
        }
    }
}
