use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VcsMetadata {
    #[serde(rename = "type")]
    pub vcs_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_url: Option<String>,
    pub commit: CommitInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<PullRequestInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CommitInfo {
    pub hash: String,
    pub short_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PullRequestInfo {
    pub number: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_branch: Option<String>,
}

/// Collects VCS (Version Control System) metadata from the current Git repository
///
/// Tries CI environment variables first (Jenkins, GitHub Actions, GitLab CI),
/// then falls back to running git commands.
///
/// Returns `None` if not in a Git repository or if Git metadata cannot be collected
#[must_use]
pub fn collect_git_metadata() -> Option<VcsMetadata> {
    // Try Jenkins Git Plugin first (highest priority for Jenkins environments)
    if let Some(vcs) = collect_jenkins_git_metadata() {
        return Some(vcs);
    }

    // Try GitHub Actions
    if let Some(vcs) = collect_github_actions_git_metadata() {
        return Some(vcs);
    }

    // Try GitLab CI
    if let Some(vcs) = collect_gitlab_ci_git_metadata() {
        return Some(vcs);
    }

    // Fall back to running git commands
    collect_git_metadata_from_commands()
}

/// Collect Jenkins Git Plugin metadata from environment variables
fn collect_jenkins_git_metadata() -> Option<VcsMetadata> {
    let git_commit = std::env::var("GIT_COMMIT").ok()?;

    // Clean branch name (remove "origin/" prefix if present)
    let git_branch = std::env::var("GIT_BRANCH")
        .ok()
        .map(|b| b.trim_start_matches("origin/").to_string());

    let git_url = std::env::var("GIT_URL").ok();
    let provider = git_url.as_ref().and_then(|url| detect_git_provider(url));

    // Check for PR/MR information (Jenkins multibranch pipeline)
    let pr = std::env::var("CHANGE_ID").ok().and_then(|change_id| {
        change_id.parse::<u32>().ok().map(|number| PullRequestInfo {
            number,
            title: std::env::var("CHANGE_TITLE").ok(),
            url: std::env::var("CHANGE_URL").ok(),
            source_branch: std::env::var("CHANGE_BRANCH").ok(),
            target_branch: std::env::var("CHANGE_TARGET").ok(),
        })
    });

    Some(VcsMetadata {
        vcs_type: "git".to_string(),
        provider,
        repository_url: git_url,
        commit: CommitInfo {
            hash: git_commit.clone(),
            short_hash: git_commit.chars().take(7).collect(),
            message: None, // Not available in Jenkins env vars
            author: std::env::var("GIT_AUTHOR_EMAIL")
                .ok()
                .or_else(|| std::env::var("GIT_AUTHOR_NAME").ok()),
            timestamp: None, // Not available in Jenkins env vars
        },
        branch: git_branch,
        tag: None, // Could check GIT_TAG if available in some Jenkins setups
        pr,
    })
}

/// Collect GitHub Actions git metadata from environment variables
fn collect_github_actions_git_metadata() -> Option<VcsMetadata> {
    if std::env::var("GITHUB_ACTIONS").ok()?.as_str() != "true" {
        return None;
    }

    let github_sha = std::env::var("GITHUB_SHA").ok()?;
    let github_ref = std::env::var("GITHUB_REF").ok()?;

    let branch = if github_ref.starts_with("refs/heads/") {
        Some(github_ref.trim_start_matches("refs/heads/").to_string())
    } else {
        std::env::var("GITHUB_REF_NAME").ok()
    };

    let tag = if github_ref.starts_with("refs/tags/") {
        Some(github_ref.trim_start_matches("refs/tags/").to_string())
    } else {
        None
    };

    let repository = std::env::var("GITHUB_REPOSITORY").ok();
    let repository_url = repository
        .as_ref()
        .map(|repo| format!("https://github.com/{repo}"));

    // PR information
    let pr = if std::env::var("GITHUB_EVENT_NAME").ok().as_deref() == Some("pull_request") {
        github_ref
            .strip_prefix("refs/pull/")
            .and_then(|s| s.split('/').next())
            .and_then(|pr_num| pr_num.parse::<u32>().ok())
            .map(|number| {
                let server_url = std::env::var("GITHUB_SERVER_URL")
                    .unwrap_or_else(|_| "https://github.com".to_string());
                let repo = std::env::var("GITHUB_REPOSITORY").unwrap_or_default();

                PullRequestInfo {
                    number,
                    title: None, // Not easily accessible in env vars
                    url: Some(format!("{server_url}/{repo}/pull/{number}")),
                    source_branch: std::env::var("GITHUB_HEAD_REF").ok(),
                    target_branch: std::env::var("GITHUB_BASE_REF").ok(),
                }
            })
    } else {
        None
    };

    Some(VcsMetadata {
        vcs_type: "git".to_string(),
        provider: Some("github".to_string()),
        repository_url,
        commit: CommitInfo {
            hash: github_sha.clone(),
            short_hash: github_sha.chars().take(7).collect(),
            message: None, // Not available in GitHub Actions env vars
            author: std::env::var("GITHUB_ACTOR").ok(),
            timestamp: None, // Not available in GitHub Actions env vars
        },
        branch,
        tag,
        pr,
    })
}

/// Collect GitLab CI metadata from environment variables
fn collect_gitlab_ci_git_metadata() -> Option<VcsMetadata> {
    if std::env::var("GITLAB_CI").ok()?.as_str() != "true" {
        return None;
    }

    let commit_sha = std::env::var("CI_COMMIT_SHA").ok()?;
    let branch = std::env::var("CI_COMMIT_BRANCH").ok();
    let tag = std::env::var("CI_COMMIT_TAG").ok();
    let repository_url = std::env::var("CI_PROJECT_URL").ok();

    // MR (merge request) information
    let pr = std::env::var("CI_MERGE_REQUEST_IID")
        .ok()
        .and_then(|mr_iid| {
            mr_iid.parse::<u32>().ok().map(|number| PullRequestInfo {
                number,
                title: std::env::var("CI_MERGE_REQUEST_TITLE").ok(),
                url: std::env::var("CI_MERGE_REQUEST_PROJECT_URL")
                    .ok()
                    .map(|base| format!("{base}/-/merge_requests/{number}")),
                source_branch: std::env::var("CI_MERGE_REQUEST_SOURCE_BRANCH_NAME").ok(),
                target_branch: std::env::var("CI_MERGE_REQUEST_TARGET_BRANCH_NAME").ok(),
            })
        });

    Some(VcsMetadata {
        vcs_type: "git".to_string(),
        provider: Some("gitlab".to_string()),
        repository_url,
        commit: CommitInfo {
            hash: commit_sha.clone(),
            short_hash: std::env::var("CI_COMMIT_SHORT_SHA")
                .unwrap_or_else(|_| commit_sha.chars().take(7).collect()),
            message: std::env::var("CI_COMMIT_MESSAGE").ok(),
            author: std::env::var("CI_COMMIT_AUTHOR").ok(),
            timestamp: std::env::var("CI_COMMIT_TIMESTAMP").ok(),
        },
        branch,
        tag,
        pr,
    })
}

/// Fall back to running git commands when not in CI or CI doesn't provide git info
fn collect_git_metadata_from_commands() -> Option<VcsMetadata> {
    if !is_git_repo() {
        return None;
    }

    let hash = git_command(&["rev-parse", "HEAD"])?;
    let short_hash = git_command(&["rev-parse", "--short=7", "HEAD"])
        .unwrap_or_else(|| hash.chars().take(7).collect());

    let remote_url = git_command(&["config", "--get", "remote.origin.url"]);
    let provider = remote_url.as_ref().and_then(|url| detect_git_provider(url));

    Some(VcsMetadata {
        vcs_type: "git".to_string(),
        provider,
        repository_url: remote_url,
        commit: CommitInfo {
            hash,
            short_hash,
            message: git_command(&["log", "-1", "--pretty=%s"]),
            author: git_command(&["log", "-1", "--pretty=%an <%ae>"]),
            timestamp: git_command(&["log", "-1", "--pretty=%cI"]),
        },
        branch: git_command(&["rev-parse", "--abbrev-ref", "HEAD"]),
        tag: git_command(&["describe", "--tags", "--exact-match"]),
        pr: None, // PR info not available from git commands alone
    })
}

fn git_command(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;

    if output.status.success() {
        let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    } else {
        None
    }
}

fn is_git_repo() -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn detect_git_provider(url: &str) -> Option<String> {
    if url.contains("github.com") {
        Some("github".to_string())
    } else if url.contains("gitlab.com") {
        Some("gitlab".to_string())
    } else if url.contains("bitbucket.org") {
        Some("bitbucket".to_string())
    } else if url.contains("dev.azure.com") || url.contains("visualstudio.com") {
        Some("azure-devops".to_string())
    } else {
        None
    }
}
