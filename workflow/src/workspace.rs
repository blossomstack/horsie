use models::runtime::{ScannedFile, WorkspaceScan};
use runtime_client::RuntimeClient;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Instruction filenames tried in order at the workdir root; first found wins.
const INSTRUCTION_CANDIDATES: &[&str] = &["AGENTS.md", "AGENT.md", "CLAUDE.md"];
/// Glob (relative to the workdir) locating skill definition files.
const SKILLS_GLOB: &str = ".claude/skills/*/SKILL.md";

/// Workspace context surfaced to every agent: the project instruction file and the
/// set of available skills, both as of the spawn-time scan.
#[derive(Clone, Default)]
pub struct WorkspaceContext {
    pub instructions: Option<String>,
    pub skills: Arc<SkillSet>,
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

/// Scan the workspace over the runtime and interpret it. On a transport error,
/// warn and return an empty context — the feature is additive and must not sink a run.
pub async fn scan(client: &RuntimeClient) -> WorkspaceContext {
    let candidates = INSTRUCTION_CANDIDATES
        .iter()
        .map(|s| s.to_string())
        .collect();
    match client
        .scan_workspace(candidates, SKILLS_GLOB.to_string())
        .await
    {
        Ok(raw) => interpret(raw),
        Err(e) => {
            tracing::warn!(error = %e, "workspace scan failed; continuing without it");
            WorkspaceContext::default()
        }
    }
}

fn interpret(raw: WorkspaceScan) -> WorkspaceContext {
    let instructions = raw.instructions.map(|f| f.content);
    let mut skills = BTreeMap::new();
    for file in raw.skills {
        match parse_skill(&file) {
            Some(skill) => {
                if skills.contains_key(&skill.name) {
                    tracing::warn!(name = %skill.name, "duplicate skill name; keeping first");
                } else {
                    skills.insert(skill.name.clone(), skill);
                }
            }
            None => tracing::warn!(path = %file.path, "skipping skill with invalid frontmatter"),
        }
    }
    WorkspaceContext {
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

/// Compose the agent's effective system prompt: its own prompt first (role), then
/// the workspace instructions, then the available-skills listing. Sections are
/// omitted when empty; returns `None` if nothing at all would be emitted.
pub fn compose_system_prompt(agent_prompt: Option<&str>, ws: &WorkspaceContext) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    if let Some(p) = agent_prompt
        && !p.trim().is_empty()
    {
        sections.push(p.trim().to_string());
    }
    if let Some(instr) = &ws.instructions
        && !instr.trim().is_empty()
    {
        sections.push(format!("# Workspace context\n{}", instr.trim()));
    }
    if !ws.skills.is_empty() {
        sections.push(format!(
            "# Available skills\nLoad a skill's full instructions with the `skill` tool before relying on it.\n{}",
            skills_listing(&ws.skills)
        ));
    }
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
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

/// The `list_skills` tool result: a count + catalog, or an empty-state line.
pub(crate) fn list_skills_result(skills: &SkillSet) -> String {
    if skills.is_empty() {
        "No skills found in the workspace.".to_string()
    } else {
        format!(
            "{} skills available:\n{}",
            skills.len(),
            skills_listing(skills)
        )
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

    #[test]
    fn interpret_skips_bad_and_dedupes() {
        let raw = WorkspaceScan {
            instructions: Some(file("AGENTS.md", "proj")),
            skills: vec![
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
        };
        let ctx = interpret(raw);
        assert_eq!(ctx.instructions.as_deref(), Some("proj"));
        assert_eq!(ctx.skills.names(), vec!["a".to_string()]);
        assert_eq!(ctx.skills.get("a").unwrap().description, "first");
    }

    #[test]
    fn compose_is_role_first_and_omits_empty() {
        let ctx = WorkspaceContext {
            instructions: Some("project rules".into()),
            skills: Arc::new(SkillSet::from_iter([Skill {
                name: "git-bisect".into(),
                description: "find bad commit".into(),
                body: "b".into(),
            }])),
        };
        let prompt = compose_system_prompt(Some("You are a coder."), &ctx).unwrap();
        let role = prompt.find("You are a coder.").unwrap();
        let ctx_pos = prompt.find("# Workspace context").unwrap();
        let skills_pos = prompt.find("# Available skills").unwrap();
        assert!(role < ctx_pos && ctx_pos < skills_pos);
        assert!(prompt.contains("- git-bisect: find bad commit"));
    }

    #[test]
    fn list_skills_result_lists_or_reports_empty() {
        let empty = SkillSet::default();
        assert_eq!(
            list_skills_result(&empty),
            "No skills found in the workspace."
        );

        let set = SkillSet::from_iter([
            Skill {
                name: "a".into(),
                description: "first".into(),
                body: "x".into(),
            },
            Skill {
                name: "b".into(),
                description: "second".into(),
                body: "y".into(),
            },
        ]);
        let out = list_skills_result(&set);
        assert!(out.starts_with("2 skills available:\n"));
        assert!(out.contains("- a: first"));
        assert!(out.contains("- b: second"));
    }

    #[test]
    fn compose_empty_context_is_none() {
        let ctx = WorkspaceContext::default();
        assert!(compose_system_prompt(None, &ctx).is_none());
        assert_eq!(
            compose_system_prompt(Some("just role"), &ctx).as_deref(),
            Some("just role")
        );
    }
}
