use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CiMetadata {
    pub system: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// Detect and collect CI/CD metadata from environment variables
#[must_use]
pub fn collect_ci_metadata() -> Option<CiMetadata> {
    // GitHub Actions
    if std::env::var("GITHUB_ACTIONS").ok().as_deref() == Some("true") {
        return Some(CiMetadata {
            system: "github-actions".to_string(),
            build_number: std::env::var("GITHUB_RUN_NUMBER").ok(),
            job_name: std::env::var("GITHUB_WORKFLOW").ok(),
            run_id: std::env::var("GITHUB_RUN_ID").ok(),
            run_url: std::env::var("GITHUB_SERVER_URL").ok().and_then(|url| {
                std::env::var("GITHUB_REPOSITORY").ok().and_then(|repo| {
                    std::env::var("GITHUB_RUN_ID")
                        .ok()
                        .map(|id| format!("{url}/{repo}/actions/runs/{id}"))
                })
            }),
            triggered_by: std::env::var("GITHUB_ACTOR").ok(),
            agent: std::env::var("RUNNER_NAME").ok(),
        });
    }

    // Jenkins
    if std::env::var("JENKINS_HOME").is_ok() || std::env::var("JENKINS_URL").is_ok() {
        return Some(CiMetadata {
            system: "jenkins".to_string(),
            build_number: std::env::var("BUILD_NUMBER").ok(),
            job_name: std::env::var("JOB_NAME").ok(),
            run_id: std::env::var("BUILD_ID").ok(),
            run_url: std::env::var("BUILD_URL").ok(),
            triggered_by: std::env::var("BUILD_USER").ok(),
            agent: std::env::var("NODE_NAME").ok(),
        });
    }

    // GitLab CI
    if std::env::var("GITLAB_CI").ok().as_deref() == Some("true") {
        return Some(CiMetadata {
            system: "gitlab-ci".to_string(),
            build_number: std::env::var("CI_PIPELINE_IID").ok(),
            job_name: std::env::var("CI_JOB_NAME").ok(),
            run_id: std::env::var("CI_PIPELINE_ID").ok(),
            run_url: std::env::var("CI_PIPELINE_URL").ok(),
            triggered_by: std::env::var("GITLAB_USER_LOGIN").ok(),
            agent: std::env::var("CI_RUNNER_DESCRIPTION").ok(),
        });
    }

    // CircleCI
    if std::env::var("CIRCLECI").ok().as_deref() == Some("true") {
        return Some(CiMetadata {
            system: "circleci".to_string(),
            build_number: std::env::var("CIRCLE_BUILD_NUM").ok(),
            job_name: std::env::var("CIRCLE_JOB").ok(),
            run_id: std::env::var("CIRCLE_WORKFLOW_ID").ok(),
            run_url: std::env::var("CIRCLE_BUILD_URL").ok(),
            triggered_by: std::env::var("CIRCLE_USERNAME").ok(),
            agent: std::env::var("CIRCLE_NODE_INDEX").ok(),
        });
    }

    // Travis CI
    if std::env::var("TRAVIS").ok().as_deref() == Some("true") {
        return Some(CiMetadata {
            system: "travis".to_string(),
            build_number: std::env::var("TRAVIS_BUILD_NUMBER").ok(),
            job_name: std::env::var("TRAVIS_JOB_NAME").ok(),
            run_id: std::env::var("TRAVIS_JOB_ID").ok(),
            run_url: std::env::var("TRAVIS_BUILD_WEB_URL").ok(),
            triggered_by: None,
            agent: None,
        });
    }

    // Azure Pipelines
    if std::env::var("TF_BUILD").ok().as_deref() == Some("True") {
        return Some(CiMetadata {
            system: "azure-pipelines".to_string(),
            build_number: std::env::var("BUILD_BUILDNUMBER").ok(),
            job_name: std::env::var("BUILD_DEFINITIONNAME").ok(),
            run_id: std::env::var("BUILD_BUILDID").ok(),
            run_url: std::env::var("SYSTEM_TEAMFOUNDATIONCOLLECTIONURI")
                .ok()
                .and_then(|uri| {
                    std::env::var("SYSTEM_TEAMPROJECT")
                        .ok()
                        .and_then(|project| {
                            std::env::var("BUILD_BUILDID")
                                .ok()
                                .map(|id| format!("{uri}{project}/_build/results?buildId={id}"))
                        })
                }),
            triggered_by: std::env::var("BUILD_REQUESTEDFOR").ok(),
            agent: std::env::var("AGENT_NAME").ok(),
        });
    }

    // Bitrise
    if std::env::var("BITRISE_IO").ok().as_deref() == Some("true") {
        return Some(CiMetadata {
            system: "bitrise".to_string(),
            build_number: std::env::var("BITRISE_BUILD_NUMBER").ok(),
            job_name: std::env::var("BITRISE_TRIGGERED_WORKFLOW_ID").ok(),
            run_id: std::env::var("BITRISE_BUILD_SLUG").ok(),
            run_url: std::env::var("BITRISE_BUILD_URL").ok(),
            triggered_by: std::env::var("BITRISE_TRIGGERED_WORKFLOW_TITLE").ok(),
            agent: None,
        });
    }

    None
}
