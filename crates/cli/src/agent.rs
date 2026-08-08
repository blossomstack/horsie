//! `horsie agent …` commands: list/get agent presets and invoke one with a
//! message, printing the new session id and its web link.

use crate::error::CliError;
use crate::server_client::ServerClient;
use horsie_models::agents::{AgentInvokeRequest, AgentView};
use horsie_models::environments::EnvironmentSpec;

/// Clip `s` to `max` display columns, marking elision with an ellipsis. Used
/// for table cells (agent descriptions, marketplace descriptions) that
/// routinely run to several hundred characters.
pub fn truncate(s: &str, max: usize) -> String {
    let flat = s.replace(['\n', '\r'], " ");
    if flat.chars().count() <= max {
        return flat;
    }
    let kept: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

pub async fn list(server: &str) -> Result<(), CliError> {
    let agents = ServerClient::new(server).await?.list_agents().await?;
    print!("{}", render_agent_table(&agents));
    Ok(())
}

pub async fn get(server: &str, name: &str) -> Result<(), CliError> {
    let agent = ServerClient::new(server).await?.get_agent(name).await?;
    print!("{}", render_agent_detail(&agent));
    Ok(())
}

/// Invoke an agent: the server creates the session and queues the message;
/// we print the session id and its web link as soon as it answers.
pub async fn invoke(
    server: &str,
    name: &str,
    message: String,
    environment: EnvironmentSpec,
    session_name: Option<String>,
) -> Result<(), CliError> {
    let client = ServerClient::new(server).await?;
    let res = client
        .invoke_agent(
            name,
            &AgentInvokeRequest {
                message,
                environment,
                name: session_name,
            },
        )
        .await?;
    print!("{}", render_invoke(client.base(), &res.session.id));
    Ok(())
}

fn render_agent_table(agents: &[AgentView]) -> String {
    if agents.is_empty() {
        return "no agents\n".to_string();
    }
    let mut out = format!(
        "{:<20} {:<14} {:>6} {:>4} {:>7}  DESCRIPTION\n",
        "NAME", "MODEL", "SKILLS", "MCP", "MEMORY"
    );
    for a in agents {
        out.push_str(&format!(
            "{:<20} {:<14} {:>6} {:>4} {:>7}  {}\n",
            truncate(&a.name, 20),
            truncate(&a.model, 14),
            a.plugins.len(),
            a.mcp_servers.len(),
            a.memory_spaces.len(),
            truncate(&a.description, 60),
        ));
    }
    out
}

fn render_agent_detail(a: &AgentView) -> String {
    let mut out = format!(
        "name        {}\ndescription {}\nmodel       {}\n",
        a.name, a.description, a.model,
    );
    if let Some(e) = a.thinking_effort.as_deref() {
        out.push_str(&format!("thinking    {e}\n"));
    }
    if !a.plugins.is_empty() {
        out.push_str(&format!("skills      {}\n", a.plugins.join(", ")));
    }
    if !a.mcp_servers.is_empty() {
        out.push_str(&format!("mcp         {}\n", a.mcp_servers.join(", ")));
    }
    if !a.memory_spaces.is_empty() {
        out.push_str(&format!("memory      {}\n", a.memory_spaces.join(", ")));
    }
    out
}

/// Two lines: the bare id (script-friendly) and the clickable web link.
fn render_invoke(base: &str, session_id: &str) -> String {
    format!("session {session_id}\n{base}/sessions/{session_id}\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn agent(name: &str) -> AgentView {
        AgentView {
            name: name.into(),
            description: "reviews PRs".into(),
            model: "sonnet".into(),
            plugins: vec!["superpowers".into()],
            mcp_servers: vec![],
            memory_spaces: vec!["default".into()],
            thinking_effort: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[test]
    fn empty_table_says_no_agents() {
        assert_eq!(render_agent_table(&[]), "no agents\n");
    }

    #[test]
    fn table_has_header_and_one_row_per_agent() {
        let out = render_agent_table(&[agent("reviewer"), agent("fixer")]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("NAME"));
        assert!(lines[1].contains("reviewer"));
        assert!(lines[1].contains("sonnet"));
        assert!(lines[2].contains("fixer"));
    }

    #[test]
    fn detail_lists_skills_and_memory() {
        let out = render_agent_detail(&agent("reviewer"));
        assert!(out.contains("name        reviewer"));
        assert!(out.contains("skills      superpowers"));
        assert!(out.contains("memory      default"));
        assert!(!out.contains("mcp "), "empty lists are omitted: {out}");
    }

    #[test]
    fn invoke_output_is_id_then_link() {
        let out = render_invoke("http://127.0.0.1:3789", "abc-123");
        assert_eq!(
            out,
            "session abc-123\nhttp://127.0.0.1:3789/sessions/abc-123\n"
        );
    }

    #[test]
    fn truncate_marks_elision_and_flattens_newlines() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a much longer description", 10), "a much lo…");
        assert_eq!(truncate("line\nbreak", 20), "line break");
    }
}
