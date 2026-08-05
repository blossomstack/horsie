use horsie_models::runtime::{PluginSkill, ScannedFile, WorkspaceScan};
use horsie_runtime_client::RuntimeClient;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// Instruction filenames tried in order at the workdir root; first found wins.
const INSTRUCTION_CANDIDATES: &[&str] = &["AGENTS.md", "AGENT.md", "CLAUDE.md"];
/// Glob (relative to the workdir) locating skill definition files.
const SKILLS_GLOB: &str = ".claude/skills/*/SKILL.md";
/// Reserved workspace name addressing the shared plugin library.
pub const SHARED_WORKSPACE: &str = "horsie_shared";

/// The shared plugin library surfaced to an opted-in agent: its skills, as of
/// the spawn-time scan.
///
/// `SessionStart` context used to ride along here on its way into the system
/// prompt. It is a hook record now, translated into a message at its place in
/// the transcript, so the library carries only what it actually holds.
#[derive(Clone, Default)]
pub struct SharedContext {
    pub skills: Arc<SkillSet>,
    /// Absolute path of the library root, when the runtime reported one. Named in
    /// the prompt's shared-skills header so the agent can reach a skill's files
    /// with the ordinary filesystem tools — the library is not a workspace, so an
    /// absolute path is its only handle.
    pub root: Option<String>,
}

/// The shared plugin library as scanned: its skills and its absolute root. The
/// root is what turns each skill's `rel_dir` into an absolute [`Skill::dir`].
#[derive(Default)]
pub struct SharedScan {
    pub skills: SkillSet,
    pub root: Option<String>,
}

impl SharedContext {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// Workspace context surfaced to every agent: one entry per workspace root, each with
/// its project instruction file and skill set, as of the spawn-time scan.
#[derive(Clone, Default)]
pub struct WorkspaceContext {
    pub workspaces: Vec<WorkspaceInfo>,
    /// Runtime OS/arch from the scan (all workspaces share one runtime).
    pub platform: Option<String>,
}

/// One scanned workspace root.
#[derive(Clone)]
pub struct WorkspaceInfo {
    pub name: String,
    pub path: String,
    pub is_git_repo: bool,
    pub instructions: Option<String>,
    pub skills: Arc<SkillSet>,
}

impl WorkspaceContext {
    /// True when the scan contributes nothing to the system prompt. `platform`
    /// counts: it renders a `# Environment` section on its own, so a context
    /// with no workspace roots is not necessarily empty.
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty() && self.platform.is_none()
    }
    /// Names of all scanned workspaces, in scan order.
    pub fn names(&self) -> Vec<String> {
        self.workspaces.iter().map(|w| w.name.clone()).collect()
    }
    /// The workspace with the given name, if scanned.
    pub fn find(&self, name: &str) -> Option<&WorkspaceInfo> {
        self.workspaces.iter().find(|w| w.name == name)
    }
}

/// Skills keyed by name, kept sorted for a stable prompt ordering.
#[derive(Default)]
pub struct SkillSet {
    skills: BTreeMap<String, Skill>,
}

#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    /// The skill's own directory, absolute, so the agent can read sibling
    /// resources with the filesystem tools. `None` only when the scan did not
    /// carry enough to compute one — a shared skill whose library root the
    /// runtime did not report.
    pub dir: Option<String>,
}

impl SkillSet {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }
    pub fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }
    pub fn len(&self) -> usize {
        self.skills.len()
    }
    fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }
}

impl FromIterator<Skill> for SkillSet {
    fn from_iter<I: IntoIterator<Item = Skill>>(iter: I) -> Self {
        Self {
            skills: iter.into_iter().map(|s| (s.name.clone(), s)).collect(),
        }
    }
}

/// Scan workspaces over the runtime and interpret them. `workspace` filters which
/// roots to scan (`None` = all, `Some(name)` = one); `include_shared` also pulls the
/// shared plugin library's skills. On a transport error, warn and return empty —
/// the feature is additive and must not sink a run.
pub async fn scan(
    client: &RuntimeClient,
    workspace: Option<String>,
    include_shared: bool,
) -> (WorkspaceContext, SharedScan) {
    let candidates = INSTRUCTION_CANDIDATES
        .iter()
        .map(|s| s.to_string())
        .collect();
    match client
        .scan_workspace(
            workspace,
            candidates,
            SKILLS_GLOB.to_string(),
            include_shared,
        )
        .await
    {
        Ok(resp) => {
            let shared = interpret_shared(resp.shared_skills, resp.shared_root.as_deref());
            (interpret(resp.workspaces), shared)
        }
        Err(e) => {
            tracing::warn!(error = %e, "workspace scan failed; continuing without it");
            (WorkspaceContext::default(), SharedScan::default())
        }
    }
}

/// Interpret the shared plugin library's skills: parse frontmatter, resolve each
/// skill's directory against `root`, dedupe by name (kept-first across plugins,
/// with a warning).
fn interpret_shared(raw: Vec<PluginSkill>, root: Option<&str>) -> SharedScan {
    let mut skills = BTreeMap::new();
    for ps in raw {
        let scanned = ScannedFile {
            path: ps.rel_dir.clone(),
            content: ps.content,
        };
        match parse_skill(&scanned) {
            Some(mut skill) => {
                skill.dir = root.map(|r| Path::new(r).join(&ps.rel_dir).display().to_string());
                if skills.contains_key(&skill.name) {
                    tracing::warn!(plugin = %ps.plugin, name = %skill.name, "duplicate shared skill name; keeping first");
                } else {
                    skills.insert(skill.name.clone(), skill);
                }
            }
            None => {
                tracing::warn!(plugin = %ps.plugin, "skipping shared skill with invalid frontmatter")
            }
        }
    }
    SharedScan {
        skills: SkillSet { skills },
        root: root.map(str::to_string),
    }
}

fn interpret(raw: Vec<WorkspaceScan>) -> WorkspaceContext {
    let platform = raw.iter().find_map(|w| w.platform.clone());
    WorkspaceContext {
        workspaces: raw.into_iter().map(interpret_one).collect(),
        platform,
    }
}

/// Interpret one workspace's raw scan: instructions verbatim, skills parsed from
/// frontmatter and deduped within this workspace (kept-first). Skills are never merged
/// across workspaces — each `WorkspaceInfo` owns its own set.
fn interpret_one(raw: WorkspaceScan) -> WorkspaceInfo {
    let instructions = raw.instructions.map(|f| f.content);
    let mut skills = BTreeMap::new();
    for file in raw.skills {
        match parse_skill(&file) {
            Some(mut skill) => {
                // The runtime globs skills with an absolute pattern, so the
                // scanned path is absolute and its parent is the skill's own
                // directory — where any sibling resources live.
                skill.dir = Path::new(&file.path)
                    .parent()
                    .map(|p| p.display().to_string());
                if skills.contains_key(&skill.name) {
                    tracing::warn!(workspace = %raw.name, name = %skill.name, "duplicate skill name; keeping first");
                } else {
                    skills.insert(skill.name.clone(), skill);
                }
            }
            None => tracing::warn!(path = %file.path, "skipping skill with invalid frontmatter"),
        }
    }
    WorkspaceInfo {
        name: raw.name,
        path: raw.path,
        is_git_repo: raw.is_git_repo,
        instructions,
        skills: Arc::new(SkillSet { skills }),
    }
}

/// Parse a `SKILL.md` with leading `---` YAML frontmatter into name/description/body.
/// Only flat `key: value` scalars are read (the SKILL.md convention); returns `None`
/// if the fence is missing or `name`/`description` are absent.
fn parse_skill(file: &ScannedFile) -> Option<Skill> {
    let (front, body) = split_frontmatter(&file.content)?;
    let mut name = None;
    let mut description = None;
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(':')?;
        let value = unquote(value.trim());
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            _ => {}
        }
    }
    Some(Skill {
        name: name?,
        description: description?,
        body: body.trim().to_string(),
        dir: None,
    })
}

/// Split `---\n<frontmatter>\n---\n<body>`; returns `(frontmatter, body)`.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---")?;
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    // Find a closing fence line (`---`, ignoring trailing CR/whitespace).
    let mut idx = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let front = &rest[..idx];
            let body = &rest[idx + line.len()..];
            return Some((front, body));
        }
        idx += line.len();
    }
    None
}

fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if s.len() >= 2
        && ((bytes[0] == b'"' && bytes[s.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[s.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Compose the agent's effective system prompt: the agent's own prompt (role),
/// the workspace instructions/skills, and finally the shared-skills listing.
/// Sections are omitted when empty; returns `None` if nothing at all would be
/// emitted.
pub fn compose_system_prompt(
    agent_prompt: Option<&str>,
    ws: &WorkspaceContext,
    shared: Option<&SharedContext>,
) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    if let Some(p) = agent_prompt
        && !p.trim().is_empty()
    {
        sections.push(p.trim().to_string());
    }
    if let Some(p) = &ws.platform {
        sections.push(environment_section(p));
    }
    if !ws.is_empty() {
        sections.push(tool_session_state_section());
    }
    if !ws.workspaces.is_empty() {
        // Where a relative path lands. The runtime resolves a tool call against
        // the agent's `set_working_dir` override, else the first workspace — so
        // this is the one directory the agent starts from, and the reason the
        // tools need no `workspace` argument. How that directory *behaves* is
        // the tool-session-state section's job, not this one's.
        let default_root = ws.workspaces.first().map_or("", |w| w.path.as_str());
        let mut block = format!(
            "# Workspaces\nYour working directory starts at {default_root}; use an \
             absolute path to reach another workspace."
        );
        for w in &ws.workspaces {
            block.push_str(&format!(
                "\n\n## {} — {}{}",
                w.name,
                w.path,
                if w.is_git_repo { " (git)" } else { "" }
            ));
            if let Some(instr) = &w.instructions
                && !instr.trim().is_empty()
            {
                block.push_str(&format!("\n{}", instr.trim()));
            }
            if !w.skills.is_empty() {
                block.push_str(&format!(
                    "\n### Skills (load with the skill tool, workspace=\"{}\")\n{}",
                    w.name,
                    skills_listing(&w.skills, Some(&w.path))
                ));
            }
        }
        sections.push(block);
    }
    if let Some(s) = shared
        && !s.skills.is_empty()
    {
        // The library is not a workspace, so it has no `## name — path` header of
        // its own; its root goes here or the agent never learns where its skills
        // keep their files.
        let header = match &s.root {
            Some(root) => format!("# Shared skills — {root}"),
            None => "# Shared skills".to_string(),
        };
        sections.push(format!(
            "{header}\nShared across all workspaces. Load with the skill tool, \
             workspace=\"{}\".\n{}",
            SHARED_WORKSPACE,
            skills_listing(&s.skills, s.root.as_deref())
        ));
    }
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

/// The `# Tool session state` section: the runtime tools share shell-like state
/// — a working directory and an env overlay — that persists across calls and is
/// changed with `set_working_dir` / `set_env`.
///
/// Stated here, once, rather than in the tool descriptions. The rule spans every
/// runtime tool, so per-tool it would be the same paragraph copied ten times and
/// re-sent with the tool specs on every request; here it rides the cache-stable
/// prompt prefix. It is a section of its own, named for what it governs, because
/// neither neighbour fits: `# Workspaces` is about which directories exist (and
/// env is not a workspace concern), and `# Environment` means which OS userland.
///
/// The prohibitions carry the section. Every mainstream harness gives a bash
/// call a fresh shell, so a model's trained reflex is to re-establish state
/// inline — `cd <dir> && …`, `export VAR=… && …` — on every call. Absent an
/// explicit contradiction that prior wins, and the state tools go unused however
/// plainly they are advertised.
fn tool_session_state_section() -> String {
    "# Tool session state\nYour tool calls share persistent state, like one \
     long-lived shell.\n- Working directory: bash and the file tools all run in \
     it, and relative paths resolve against it. Change it with set_working_dir. \
     Do NOT prefix a command with `cd` — each bash call is a fresh shell, so the \
     `cd` lasts only for that one command.\n- Environment variables: set_env \
     applies to every later bash command. Do NOT repeat `export` in each command."
        .to_string()
}

/// The `# Environment` section: one line telling the model which OS userland
/// its bash/filesystem tools run on, so it doesn't probe by failing (BSD-vs-GNU
/// differences have burned whole turns).
fn environment_section(platform: &str) -> String {
    let os = platform.split('-').next().unwrap_or(platform);
    match os {
        "macos" => "# Environment\nOS: macOS — BSD userland: no GNU `timeout` or \
            `cat -A`; `sed -i` requires an explicit backup argument (`sed -i ''`); \
            GNU coreutils, if installed, are g-prefixed (`gtimeout`, `gsed`)."
            .to_string(),
        "linux" => "# Environment\nOS: Linux — GNU coreutils available.".to_string(),
        other => format!("# Environment\nOS: {other}."),
    }
}

/// The `inspect_workspace` view of the shared plugin library: its skills (name +
/// description), or a note when empty.
pub(crate) fn shared_inspect(skills: &SkillSet, root: Option<&str>) -> String {
    if skills.is_empty() {
        return format!("## {SHARED_WORKSPACE}\nskills: none");
    }
    let header = match root {
        Some(r) => format!("## {SHARED_WORKSPACE} — {r}"),
        None => format!("## {SHARED_WORKSPACE}"),
    };
    format!(
        "{header}\nskills ({}):\n{}",
        skills.len(),
        skills_listing(skills, root)
    )
}

/// Render skills as sorted `- name — <dir>/: description` lines, each directory
/// relative to `root`. The section header above the listing already names the
/// root, so repeating a long absolute prefix on every line of a twenty-skill
/// plugin library would be pure waste. Falls back to `- name: description` when
/// a skill has no directory or sits outside the root.
fn skills_listing(skills: &SkillSet, root: Option<&str>) -> String {
    skills
        .iter()
        .map(|s| match relative_dir(s, root) {
            Some(rel) => format!("- {} — {}/: {}", s.name, rel, s.description),
            None => format!("- {}: {}", s.name, s.description),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A skill's directory relative to `root`, or `None` when either is absent or the
/// skill does not sit under the root.
fn relative_dir(skill: &Skill, root: Option<&str>) -> Option<String> {
    let dir = skill.dir.as_deref()?;
    let root = root?;
    Path::new(dir)
        .strip_prefix(root)
        .ok()
        .map(|p| p.display().to_string())
        .filter(|p| !p.is_empty())
}

/// The `inspect_workspace` tool result: the live catalog for the scanned workspaces —
/// each with its path, git flag, instruction-file presence, and skills (name +
/// description only, never bodies).
pub(crate) fn inspect_result(ws: &WorkspaceContext) -> String {
    if ws.workspaces.is_empty() {
        return "No workspaces found.".to_string();
    }
    let mut out = String::new();
    for (i, w) in ws.workspaces.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format!(
            "## {} — {}{}\ninstructions: {}",
            w.name,
            w.path,
            if w.is_git_repo { " (git)" } else { "" },
            if w.instructions.is_some() {
                "present"
            } else {
                "none"
            },
        ));
        if w.skills.is_empty() {
            out.push_str("\nskills: none");
        } else {
            out.push_str(&format!(
                "\nskills ({}):\n{}",
                w.skills.len(),
                skills_listing(&w.skills, Some(&w.path))
            ));
        }
    }
    out
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

    fn file(path: &str, content: &str) -> ScannedFile {
        ScannedFile {
            path: path.into(),
            content: content.into(),
        }
    }

    #[test]
    fn parses_valid_skill() {
        let s = parse_skill(&file(
            ".claude/skills/x/SKILL.md",
            "---\nname: git-bisect\ndescription: Find the bad commit\n---\nDo the bisect.\n",
        ))
        .unwrap();
        assert_eq!(s.name, "git-bisect");
        assert_eq!(s.description, "Find the bad commit");
        assert_eq!(s.body, "Do the bisect.");
    }

    #[test]
    fn description_with_colon_keeps_full_value() {
        let s = parse_skill(&file(
            "p",
            "---\nname: n\ndescription: Use when X: do Y\n---\nbody",
        ))
        .unwrap();
        assert_eq!(s.description, "Use when X: do Y");
    }

    #[test]
    fn strips_quotes() {
        let s = parse_skill(&file("p", "---\nname: \"n\"\ndescription: 'd'\n---\nb")).unwrap();
        assert_eq!(s.name, "n");
        assert_eq!(s.description, "d");
    }

    #[test]
    fn missing_fence_is_none() {
        assert!(parse_skill(&file("p", "name: n\ndescription: d\nbody")).is_none());
    }

    #[test]
    fn missing_required_key_is_none() {
        assert!(parse_skill(&file("p", "---\nname: n\n---\nbody")).is_none());
    }

    fn ws_scan(name: &str, instructions: Option<&str>, skills: Vec<ScannedFile>) -> WorkspaceScan {
        WorkspaceScan {
            name: name.into(),
            path: format!("/ws/{name}"),
            is_git_repo: false,
            instructions: instructions.map(|c| file("AGENTS.md", c)),
            skills,
            platform: None,
        }
    }

    #[test]
    fn interpret_skips_bad_and_dedupes_within_workspace() {
        let raw = vec![ws_scan(
            "w",
            Some("proj"),
            vec![
                file(
                    "a/SKILL.md",
                    "---\nname: a\ndescription: first\n---\nbody-a",
                ),
                file("b/SKILL.md", "no frontmatter"),
                file(
                    "c/SKILL.md",
                    "---\nname: a\ndescription: dup\n---\nbody-dup",
                ),
            ],
        )];
        let ctx = interpret(raw);
        let w = ctx.find("w").unwrap();
        assert_eq!(w.instructions.as_deref(), Some("proj"));
        assert_eq!(w.skills.names(), vec!["a".to_string()]);
        assert_eq!(w.skills.get("a").unwrap().description, "first");
    }

    #[test]
    fn interpret_keeps_same_skill_name_across_workspaces() {
        let skill = |desc: &str| {
            file(
                "s/SKILL.md",
                &format!("---\nname: dup\ndescription: {desc}\n---\nbody"),
            )
        };
        let ctx = interpret(vec![
            ws_scan("alpha", None, vec![skill("from-alpha")]),
            ws_scan("beta", None, vec![skill("from-beta")]),
        ]);
        assert_eq!(
            ctx.find("alpha")
                .unwrap()
                .skills
                .get("dup")
                .unwrap()
                .description,
            "from-alpha"
        );
        assert_eq!(
            ctx.find("beta")
                .unwrap()
                .skills
                .get("dup")
                .unwrap()
                .description,
            "from-beta"
        );
    }

    #[test]
    fn compose_is_role_first_with_one_block_per_workspace() {
        let ctx = interpret(vec![
            ws_scan(
                "alpha",
                Some("alpha rules"),
                vec![file(
                    "s/SKILL.md",
                    "---\nname: a-skill\ndescription: do a\n---\nb",
                )],
            ),
            ws_scan("beta", None, vec![]),
        ]);
        let prompt = compose_system_prompt(Some("You are a coder."), &ctx, None).unwrap();
        let role = prompt.find("You are a coder.").unwrap();
        let header = prompt.find("# Workspaces").unwrap();
        let alpha = prompt.find("## alpha").unwrap();
        let beta = prompt.find("## beta").unwrap();
        assert!(role < header && header < alpha && alpha < beta);
        assert!(prompt.contains("alpha rules"));
        assert!(prompt.contains("- a-skill: do a"));
        assert!(prompt.contains("workspace=\"alpha\""));
    }

    #[test]
    fn inspect_lists_workspaces_or_reports_empty() {
        assert_eq!(
            inspect_result(&WorkspaceContext::default()),
            "No workspaces found."
        );
        let ctx = interpret(vec![ws_scan(
            "alpha",
            Some("rules"),
            vec![file(
                "s/SKILL.md",
                "---\nname: a\ndescription: first\n---\nx",
            )],
        )]);
        let out = inspect_result(&ctx);
        assert!(out.contains("## alpha — /ws/alpha"));
        assert!(out.contains("instructions: present"));
        assert!(out.contains("skills (1):"));
        assert!(out.contains("- a: first"));
    }

    #[test]
    fn compose_empty_context_is_none() {
        let ctx = WorkspaceContext::default();
        assert!(compose_system_prompt(None, &ctx, None).is_none());
        assert_eq!(
            compose_system_prompt(Some("just role"), &ctx, None).as_deref(),
            Some("just role")
        );
    }

    /// The one place the sticky-state rule is stated. It governs every runtime
    /// tool, so it lives here rather than being copied into ten tool
    /// descriptions that ride every request — and it is named for what it
    /// governs, so a model looking for "how do I set an env var" can find it.
    #[test]
    fn compose_states_tool_session_state_ahead_of_the_workspace_list() {
        let ctx = interpret(vec![ws_scan("alpha", None, vec![])]);
        let prompt = compose_system_prompt(None, &ctx, None).unwrap();
        let state = prompt.find("# Tool session state").unwrap();
        assert!(state < prompt.find("# Workspaces").unwrap(), "{prompt}");
        // Both halves, and the negative instruction that is the whole point:
        // without it the model falls back to its stateless-shell prior and
        // re-establishes the state inline on every single call.
        assert!(prompt.contains("set_working_dir"), "{prompt}");
        assert!(prompt.contains("`cd`"), "{prompt}");
        assert!(prompt.contains("set_env"), "{prompt}");
        assert!(prompt.contains("`export`"), "{prompt}");
    }

    /// No runtime scanned means no runtime tools, so the rule would describe
    /// tools the agent does not have.
    #[test]
    fn compose_omits_tool_session_state_without_a_runtime_scan() {
        let ctx = WorkspaceContext::default();
        let prompt = compose_system_prompt(Some("just role"), &ctx, None).unwrap();
        assert!(!prompt.contains("# Tool session state"), "{prompt}");
    }

    #[test]
    fn environment_section_renders_platform() {
        let ctx = WorkspaceContext {
            workspaces: vec![],
            platform: Some("macos-aarch64".to_string()),
        };
        let prompt = compose_system_prompt(Some("You are a coder."), &ctx, None).unwrap();
        assert!(prompt.contains("# Environment"), "{prompt}");
        assert!(prompt.contains("BSD userland"), "{prompt}");
    }

    fn plugin_skill(name: &str, rel_dir: &str, desc: &str) -> PluginSkill {
        PluginSkill {
            plugin: "sp".into(),
            rel_dir: rel_dir.into(),
            content: format!("---\nname: {name}\ndescription: {desc}\n---\nbody-{name}"),
        }
    }

    #[test]
    fn interpret_shared_sets_dir_and_dedupes() {
        let scan = interpret_shared(
            vec![
                plugin_skill("brainstorming", "sp/skills/brainstorming", "explore"),
                plugin_skill("brainstorming", "other/skills/brainstorming", "dup"),
            ],
            Some("/opt/plugins"),
        );
        assert_eq!(scan.skills.names(), vec!["brainstorming".to_string()]);
        let s = scan.skills.get("brainstorming").unwrap();
        assert_eq!(s.description, "explore"); // kept-first
        assert_eq!(
            s.dir.as_deref(),
            Some("/opt/plugins/sp/skills/brainstorming")
        );
        assert_eq!(scan.root.as_deref(), Some("/opt/plugins"));
    }

    /// An older runtime reports no library root; the skill still loads, it just
    /// carries no path to its siblings.
    #[test]
    fn interpret_shared_leaves_dir_unset_without_a_root() {
        let scan = interpret_shared(
            vec![plugin_skill(
                "brainstorming",
                "sp/skills/brainstorming",
                "d",
            )],
            None,
        );
        assert!(scan.skills.get("brainstorming").unwrap().dir.is_none());
        assert!(scan.root.is_none());
    }

    #[test]
    fn workspace_skill_dir_is_its_own_directory() {
        let ctx = interpret(vec![WorkspaceScan {
            name: "api".into(),
            path: "/ws/api".into(),
            is_git_repo: true,
            instructions: None,
            skills: vec![ScannedFile {
                path: "/ws/api/.claude/skills/deploy/SKILL.md".into(),
                content: "---\nname: deploy\ndescription: Ship it\n---\nbody".into(),
            }],
            platform: None,
        }]);
        let skill = ctx.workspaces[0].skills.get("deploy").unwrap();
        assert_eq!(skill.dir.as_deref(), Some("/ws/api/.claude/skills/deploy"));
    }

    #[test]
    fn compose_orders_the_role_before_the_shared_skills() {
        let ctx = WorkspaceContext::default();
        let scan = interpret_shared(
            vec![plugin_skill("tdd", "sp/skills/tdd", "write tests first")],
            Some("/opt/plugins"),
        );
        let shared = SharedContext {
            skills: Arc::new(scan.skills),
            root: scan.root,
        };
        let prompt = compose_system_prompt(Some("You are a coder."), &ctx, Some(&shared)).unwrap();
        let role = prompt.find("You are a coder.").unwrap();
        let shared_hdr = prompt.find("# Shared skills").unwrap();
        assert!(role < shared_hdr);
        // `SessionStart` context used to be prepended here as a "# Session
        // bootstrap" section. It is a translated hook record now, so the system
        // prompt has nothing to say about it.
        assert!(!prompt.contains("# Session bootstrap"));
        assert!(prompt.contains("workspace=\"horsie_shared\""));
        // The header names the library root, so the per-skill path can be relative.
        assert!(
            prompt.contains("# Shared skills — /opt/plugins"),
            "{prompt}"
        );
        assert!(
            prompt.contains("- tdd — sp/skills/tdd/: write tests first"),
            "{prompt}"
        );
    }

    #[test]
    fn shared_inspect_lists_or_reports_empty() {
        assert!(shared_inspect(&SkillSet::default(), None).contains("skills: none"));
        let scan = interpret_shared(
            vec![plugin_skill("tdd", "sp/skills/tdd", "d")],
            Some("/opt/plugins"),
        );
        let out = shared_inspect(&scan.skills, scan.root.as_deref());
        assert!(out.contains("## horsie_shared — /opt/plugins"), "{out}");
        assert!(out.contains("- tdd — sp/skills/tdd/: d"), "{out}");
    }
}
