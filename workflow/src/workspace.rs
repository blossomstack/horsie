use horsie_models::runtime::{PluginSkill, ScannedFile, WorkspaceScan};
use horsie_runtime_client::RuntimeClient;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Instruction filenames tried in order at the workdir root; first found wins.
const INSTRUCTION_CANDIDATES: &[&str] = &["AGENTS.md", "AGENT.md", "CLAUDE.md"];
/// Glob (relative to the workdir) locating skill definition files.
const SKILLS_GLOB: &str = ".claude/skills/*/SKILL.md";
/// Reserved workspace name addressing the shared plugin library.
pub const SHARED_WORKSPACE: &str = "horsie_shared";

/// The shared plugin library surfaced to an opted-in agent: its skills plus the
/// `SessionStart` bootstrap context, as of the spawn-time scan.
#[derive(Clone, Default)]
pub struct SharedContext {
    pub skills: Arc<SkillSet>,
    pub bootstrap: Option<String>,
}

impl SharedContext {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.bootstrap.is_none()
    }
}

/// Workspace context surfaced to every agent: one entry per workspace root, each with
/// its project instruction file and skill set, as of the spawn-time scan.
#[derive(Clone, Default)]
pub struct WorkspaceContext {
    pub workspaces: Vec<WorkspaceInfo>,
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
    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
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
    /// For a shared (plugin) skill: its directory relative to the `horsie_shared`
    /// root, so the agent can read sibling resources. `None` for workspace skills.
    pub rel_dir: Option<String>,
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
) -> (WorkspaceContext, SkillSet) {
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
        Ok((raw, shared)) => (interpret(raw), interpret_shared(shared)),
        Err(e) => {
            tracing::warn!(error = %e, "workspace scan failed; continuing without it");
            (WorkspaceContext::default(), SkillSet::default())
        }
    }
}

/// Interpret the shared plugin library's skills: parse frontmatter, attach each
/// skill's `rel_dir`, dedupe by name (kept-first across plugins, with a warning).
fn interpret_shared(raw: Vec<PluginSkill>) -> SkillSet {
    let mut skills = BTreeMap::new();
    for ps in raw {
        let scanned = ScannedFile {
            path: ps.rel_dir.clone(),
            content: ps.content,
        };
        match parse_skill(&scanned) {
            Some(mut skill) => {
                skill.rel_dir = Some(ps.rel_dir);
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
    SkillSet { skills }
}

fn interpret(raw: Vec<WorkspaceScan>) -> WorkspaceContext {
    WorkspaceContext {
        workspaces: raw.into_iter().map(interpret_one).collect(),
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
            Some(skill) => {
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
        rel_dir: None,
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

/// Compose the agent's effective system prompt: the `SessionStart` bootstrap (if any)
/// first, then its own prompt (role), the workspace instructions/skills, and finally
/// the shared-skills listing. Sections are omitted when empty; returns `None` if
/// nothing at all would be emitted.
pub fn compose_system_prompt(
    agent_prompt: Option<&str>,
    ws: &WorkspaceContext,
    shared: Option<&SharedContext>,
) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    if let Some(s) = shared
        && let Some(boot) = &s.bootstrap
        && !boot.trim().is_empty()
    {
        sections.push(format!("# Session bootstrap\n{}", boot.trim()));
    }
    if let Some(p) = agent_prompt
        && !p.trim().is_empty()
    {
        sections.push(p.trim().to_string());
    }
    if !ws.workspaces.is_empty() {
        let mut block = String::from(
            "# Workspaces\nFilesystem, bash, and skill tools take a `workspace` argument naming one of these (omit it only when there is exactly one).",
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
                    skills_listing(&w.skills)
                ));
            }
        }
        sections.push(block);
    }
    if let Some(s) = shared
        && !s.skills.is_empty()
    {
        sections.push(format!(
            "# Shared skills (load with the skill tool, workspace=\"{}\")\n{}",
            SHARED_WORKSPACE,
            skills_listing(&s.skills)
        ));
    }
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

/// The `inspect_workspace` view of the shared plugin library: its skills (name +
/// description), or a note when empty.
pub(crate) fn shared_inspect(skills: &SkillSet) -> String {
    if skills.is_empty() {
        return format!("## {SHARED_WORKSPACE}\nskills: none");
    }
    format!(
        "## {}\nskills ({}):\n{}",
        SHARED_WORKSPACE,
        skills.len(),
        skills_listing(skills)
    )
}

/// Render skills as sorted `- name: description` lines. Shared by the prompt's
/// `# Available skills` block and the `list_skills` tool result.
fn skills_listing(skills: &SkillSet) -> String {
    skills
        .iter()
        .map(|s| format!("- {}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n")
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
                skills_listing(&w.skills)
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

    fn plugin_skill(name: &str, rel_dir: &str, desc: &str) -> PluginSkill {
        PluginSkill {
            plugin: "sp".into(),
            rel_dir: rel_dir.into(),
            content: format!("---\nname: {name}\ndescription: {desc}\n---\nbody-{name}"),
        }
    }

    #[test]
    fn interpret_shared_sets_rel_dir_and_dedupes() {
        let set = interpret_shared(vec![
            plugin_skill("brainstorming", "sp/skills/brainstorming", "explore"),
            plugin_skill("brainstorming", "other/skills/brainstorming", "dup"),
        ]);
        assert_eq!(set.names(), vec!["brainstorming".to_string()]);
        let s = set.get("brainstorming").unwrap();
        assert_eq!(s.description, "explore"); // kept-first
        assert_eq!(s.rel_dir.as_deref(), Some("sp/skills/brainstorming"));
    }

    #[test]
    fn compose_prepends_bootstrap_and_appends_shared_skills() {
        let ctx = WorkspaceContext::default();
        let skills = interpret_shared(vec![plugin_skill(
            "tdd",
            "sp/skills/tdd",
            "write tests first",
        )]);
        let shared = SharedContext {
            skills: Arc::new(skills),
            bootstrap: Some("USE SKILLS".into()),
        };
        let prompt = compose_system_prompt(Some("You are a coder."), &ctx, Some(&shared)).unwrap();
        let boot = prompt.find("# Session bootstrap").unwrap();
        let role = prompt.find("You are a coder.").unwrap();
        let shared_hdr = prompt.find("# Shared skills").unwrap();
        assert!(boot < role && role < shared_hdr);
        assert!(prompt.contains("USE SKILLS"));
        assert!(prompt.contains("workspace=\"horsie_shared\""));
        assert!(prompt.contains("- tdd: write tests first"));
    }

    #[test]
    fn shared_inspect_lists_or_reports_empty() {
        assert!(shared_inspect(&SkillSet::default()).contains("skills: none"));
        let skills = interpret_shared(vec![plugin_skill("tdd", "sp/skills/tdd", "d")]);
        let out = shared_inspect(&skills);
        assert!(out.contains("## horsie_shared"));
        assert!(out.contains("- tdd: d"));
    }
}
