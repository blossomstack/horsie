use horsie_models::runtime::{
    InspectPullRequestDiffInput, InspectPullRequestInput, ToolError, ToolOutput, ToolResult,
};
use serde_json::Value;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const MAX_BODY_CHARS: usize = 8_000;
const MAX_FILES: usize = 500;
const PAGE_SIZE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRequestRef {
    owner: String,
    repo: String,
    number: u64,
}

pub async fn inspect(working_dir: &Path, input: InspectPullRequestInput) -> ToolResult {
    let reference = match resolve(working_dir, &input.reference).await {
        Ok(reference) => reference,
        Err(reason) => return error(reason),
    };
    let client = GithubClient::new(working_dir, &reference).await;
    let pull = match client.pull(&reference).await {
        Ok(value) => value,
        Err(reason) => return error(reason),
    };
    let checks = pull["head"]["sha"]
        .as_str()
        .map(|sha| client.checks(&reference, sha));
    let checks = match checks {
        Some(checks) => checks.await.ok(),
        None => None,
    };
    ok(render_summary(&pull, checks.as_ref()))
}

pub async fn inspect_diff(working_dir: &Path, input: InspectPullRequestDiffInput) -> ToolResult {
    let reference = match resolve(working_dir, &input.reference).await {
        Ok(reference) => reference,
        Err(reason) => return error(reason),
    };
    let client = GithubClient::new(working_dir, &reference).await;
    let files = match client.files(&reference).await {
        Ok(value) => value,
        Err(reason) => return error(reason),
    };
    match input.path {
        None => ok(render_files(reference.number, &files)),
        Some(path) => files
            .iter()
            .find(|file| file["filename"].as_str() == Some(&path))
            .map(|file| ok(render_file_patch(file, &path)))
            .unwrap_or_else(|| error(format!("pull request does not change '{path}'"))),
    }
}

struct GithubClient {
    client: reqwest::Client,
    token: Option<String>,
}

impl GithubClient {
    async fn new(working_dir: &Path, reference: &PullRequestRef) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("horsie-runtime")
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            token: credential(working_dir, reference).await,
        }
    }

    async fn pull(&self, reference: &PullRequestRef) -> Result<Value, String> {
        self.get(&format!(
            "https://api.github.com/repos/{}/{}/pulls/{}",
            reference.owner, reference.repo, reference.number
        ))
        .await
    }

    async fn checks(&self, reference: &PullRequestRef, sha: &str) -> Result<Value, String> {
        self.get(&format!(
            "https://api.github.com/repos/{}/{}/commits/{sha}/check-runs?per_page={PAGE_SIZE}",
            reference.owner, reference.repo
        ))
        .await
    }

    async fn files(&self, reference: &PullRequestRef) -> Result<Vec<Value>, String> {
        let mut files = Vec::new();
        for page in 1.. {
            let value = self
                .get(&format!(
                    "https://api.github.com/repos/{}/{}/pulls/{}/files?per_page={PAGE_SIZE}&page={page}",
                    reference.owner, reference.repo, reference.number
                ))
                .await?;
            let batch = value
                .as_array()
                .ok_or_else(|| "GitHub returned a non-list file response".to_string())?;
            files.extend(batch.iter().cloned());
            if batch.len() < PAGE_SIZE || files.len() >= MAX_FILES {
                break;
            }
        }
        Ok(files)
    }

    async fn get(&self, url: &str) -> Result<Value, String> {
        let mut request = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|error| error.to_string())?;
        if !status.is_success() {
            let detail = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| value["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
            return Err(format!("GitHub API returned {status}: {detail}"));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("GitHub returned invalid JSON: {error}"))
    }
}

async fn resolve(working_dir: &Path, raw: &str) -> Result<PullRequestRef, String> {
    if let Some(reference) = from_url(raw) {
        return Ok(reference);
    }
    let number = raw
        .trim()
        .trim_start_matches('#')
        .parse::<u64>()
        .map_err(|_| "'reference' must be a pull request number or GitHub pull URL".to_string())?;
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(working_dir)
        .output()
        .await
        .map_err(|error| format!("failed to inspect git origin: {error}"))?;
    if !output.status.success() {
        return Err("a numeric pull request reference requires a git origin".to_string());
    }
    let remote = String::from_utf8_lossy(&output.stdout);
    let (owner, repo) = repo_from_remote(remote.trim()).ok_or_else(|| {
        format!(
            "cannot derive a GitHub repository from origin '{}'; pass a pull request URL",
            remote.trim()
        )
    })?;
    Ok(PullRequestRef {
        owner,
        repo,
        number,
    })
}

fn from_url(raw: &str) -> Option<PullRequestRef> {
    let segments: Vec<&str> = raw.trim_end_matches('/').split('/').collect();
    let pull = segments.iter().position(|segment| *segment == "pull")?;
    Some(PullRequestRef {
        owner: segments.get(pull.checked_sub(2)?)?.to_string(),
        repo: segments
            .get(pull.checked_sub(1)?)?
            .trim_end_matches(".git")
            .to_string(),
        number: segments.get(pull + 1)?.parse().ok()?,
    })
}

fn repo_from_remote(raw: &str) -> Option<(String, String)> {
    let path = raw
        .strip_prefix("git@github.com:")
        .or_else(|| raw.strip_prefix("https://github.com/"))?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let (owner, repo) = path.split_once('/')?;
    Some((owner.to_string(), repo.to_string()))
}

async fn credential(working_dir: &Path, reference: &PullRequestRef) -> Option<String> {
    let mut child = Command::new("git")
        .args(["credential", "fill"])
        .current_dir(working_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let query = format!(
        "protocol=https\nhost=github.com\npath={}/{}.git\n\n",
        reference.owner, reference.repo
    );
    child
        .stdin
        .as_mut()?
        .write_all(query.as_bytes())
        .await
        .ok()?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("password=").map(str::to_string))
}

fn render_summary(pull: &Value, checks: Option<&Value>) -> String {
    let mut output = format!(
        "#{}: {}\n{} by {} · {} → {} · mergeable: {} · +{}/-{}\n{}",
        pull["number"].as_u64().unwrap_or(0),
        pull["title"].as_str().unwrap_or("untitled"),
        pull["state"].as_str().unwrap_or("unknown"),
        pull["user"]["login"].as_str().unwrap_or("unknown"),
        pull["base"]["ref"].as_str().unwrap_or("?"),
        pull["head"]["ref"].as_str().unwrap_or("?"),
        pull["mergeable"]
            .as_bool()
            .map_or("unknown", |value| if value { "yes" } else { "no" }),
        pull["additions"].as_u64().unwrap_or(0),
        pull["deletions"].as_u64().unwrap_or(0),
        pull["html_url"].as_str().unwrap_or("")
    );
    if let Some(checks) = checks {
        let summary = checks["check_runs"].as_array().into_iter().flatten().fold(
            (0, 0, 0),
            |(passed, failed, pending), check| match check["conclusion"].as_str() {
                Some("success" | "neutral" | "skipped") => (passed + 1, failed, pending),
                Some("failure" | "timed_out" | "action_required") => (passed, failed + 1, pending),
                _ => (passed, failed, pending + 1),
            },
        );
        output.push_str(&format!(
            "\nChecks: {} passed, {} failed, {} pending/other",
            summary.0, summary.1, summary.2
        ));
    }
    if let Some(body) = pull["body"].as_str().filter(|body| !body.trim().is_empty()) {
        output.push_str("\n\n## Description\n");
        output.extend(body.chars().take(MAX_BODY_CHARS));
        if body.chars().count() > MAX_BODY_CHARS {
            output.push_str("\n[… description truncated …]");
        }
    }
    output
}

fn render_files(number: u64, files: &[Value]) -> String {
    let mut output = format!("#{number} changes {} file(s):", files.len());
    for file in files.iter().take(MAX_FILES) {
        output.push_str(&format!(
            "\n- {} (+{}/-{})",
            file["filename"].as_str().unwrap_or("?"),
            file["additions"].as_u64().unwrap_or(0),
            file["deletions"].as_u64().unwrap_or(0)
        ));
    }
    output
}

fn render_file_patch(file: &Value, path: &str) -> String {
    let status = file["status"].as_str().unwrap_or("modified");
    let additions = file["additions"].as_u64().unwrap_or(0);
    let deletions = file["deletions"].as_u64().unwrap_or(0);
    let patch = file["patch"]
        .as_str()
        .unwrap_or("[patch unavailable from GitHub; file may be binary or too large]");
    format!("{path} ({status}, +{additions}/-{deletions})\n\n{patch}")
}

fn ok(stdout: String) -> ToolResult {
    ToolResult::Ok(ToolOutput {
        stdout,
        stderr: String::new(),
        exit_code: 0,
        artifacts: Vec::new(),
        original_output_bytes: 0,
        spilled_output_bytes: 0,
    })
}

fn error(reason: String) -> ToolResult {
    ToolResult::Err(ToolError { reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_https_and_ssh_remotes() {
        assert_eq!(
            repo_from_remote("https://github.com/acme/repo.git"),
            Some(("acme".to_string(), "repo".to_string()))
        );
        assert_eq!(
            repo_from_remote("git@github.com:acme/repo.git"),
            Some(("acme".to_string(), "repo".to_string()))
        );
        assert_eq!(
            from_url("https://github.com/acme/repo/pull/12"),
            Some(PullRequestRef {
                owner: "acme".to_string(),
                repo: "repo".to_string(),
                number: 12,
            })
        );
    }

    #[test]
    fn summary_collapses_check_rollups() {
        let pull = serde_json::json!({
            "number": 12,
            "title": "small",
            "state": "open",
            "user": {"login": "dev"},
            "base": {"ref": "main"},
            "head": {"ref": "topic"},
            "mergeable": true,
            "additions": 10,
            "deletions": 3,
            "html_url": "https://github.com/acme/repo/pull/12",
            "body": "why"
        });
        let checks = serde_json::json!({"check_runs": [
            {"conclusion": "success"},
            {"conclusion": "failure"},
            {"conclusion": null}
        ]});
        let rendered = render_summary(&pull, Some(&checks));
        assert!(rendered.contains("1 passed, 1 failed, 1 pending/other"));
        assert!(rendered.contains("## Description\nwhy"));
    }

    #[test]
    fn file_listing_is_compact() {
        let files = vec![serde_json::json!({
            "filename": "src/lib.rs",
            "additions": 4,
            "deletions": 1
        })];
        assert_eq!(
            render_files(12, &files),
            "#12 changes 1 file(s):\n- src/lib.rs (+4/-1)"
        );
    }
}
