use super::LocalTool;
use crate::{api::Client, config::Config};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use rmcp::{
    ErrorData,
    model::{CallToolResponse, CallToolResult, ContentBlock, JsonObject, Tool, ToolAnnotations},
};
use serde::Deserialize;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

pub(super) const NAME: &str = "wait_for_completion";
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 60; // every min
const DEFAULT_TIMEOUT_SECONDS: u64 = 30 * 60; // default up to 30 min
const MAX_POLL_INTERVAL_SECONDS: u64 = 5 * 60;
const MAX_TIMEOUT_SECONDS: u64 = 2 * 60 * 60;

pub(super) struct WaitForTool {
    config: Config,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitForInput {
    project_id: String,
    job_id: Option<String>,
    run_id: Option<String>,
    test_plan_execution_id: Option<String>,
    poll_interval_seconds: Option<u64>,
    timeout_seconds: Option<u64>,
}

// what to wait for
#[derive(Debug, Clone, Copy)]
enum WaitTargetKind {
    Job,
    Run,
    TestPlanExecution,
}

impl WaitTargetKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Job => "job",
            Self::Run => "run",
            Self::TestPlanExecution => "test_plan_execution",
        }
    }
}

#[derive(Debug)]
struct WaitTarget {
    kind: WaitTargetKind,
    id: String,
}

impl WaitTarget {
    fn from_input(input: &WaitForInput) -> Result<Self> {
        let targets = [
            (WaitTargetKind::Job, input.job_id.as_deref()),
            (WaitTargetKind::Run, input.run_id.as_deref()),
            (
                WaitTargetKind::TestPlanExecution,
                input.test_plan_execution_id.as_deref(),
            ),
        ];
        let supplied: Vec<_> = targets
            .into_iter()
            .filter_map(|(kind, id)| id.map(|id| (kind, id.trim())))
            .collect();
        anyhow::ensure!(
            supplied.len() == 1,
            "supply exactly one of job_id, run_id, or test_plan_execution_id"
        );
        let (kind, id) = supplied[0];
        anyhow::ensure!(!id.is_empty(), "the target ID cannot be empty");
        Ok(Self {
            kind,
            id: id.to_string(),
        })
    }

    async fn fetch(&self, client: &Client) -> Result<Value> {
        match self.kind {
            WaitTargetKind::Job => client.get_job(&self.id).await,
            WaitTargetKind::Run => client.get_run(&self.id).await,
            WaitTargetKind::TestPlanExecution => client.get_test_plan_execution(&self.id).await,
        }
        .context("failed to fetch wait target status")
    }

    fn status(&self, resource: &Value) -> Result<String> {
        let field = match self.kind {
            WaitTargetKind::Run => "state",
            WaitTargetKind::Job | WaitTargetKind::TestPlanExecution => "status",
        };
        resource[field]
            .as_str()
            .map(ToString::to_string)
            .with_context(|| {
                format!(
                    "the {} response did not contain a string '{field}' status",
                    self.kind.as_str()
                )
            })
    }

    fn is_complete(status: &str) -> bool {
        status.eq_ignore_ascii_case("completed")
    }
}

impl WaitForTool {
    pub(super) fn new(config: Config) -> Self {
        Self { config }
    }

    async fn wait(&self, input: WaitForInput) -> Result<CallToolResult> {
        anyhow::ensure!(
            !input.project_id.trim().is_empty(),
            "project_id cannot be empty"
        );
        let poll_interval_seconds = input
            .poll_interval_seconds
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS);
        let timeout_seconds = input.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        anyhow::ensure!(
            poll_interval_seconds > 0 && poll_interval_seconds <= MAX_POLL_INTERVAL_SECONDS,
            "poll_interval_seconds must be between 1 and {MAX_POLL_INTERVAL_SECONDS}"
        );
        anyhow::ensure!(
            timeout_seconds > 0 && timeout_seconds <= MAX_TIMEOUT_SECONDS,
            "timeout_seconds must be between 1 and {MAX_TIMEOUT_SECONDS}"
        );
        let target = WaitTarget::from_input(&input)?;
        let config = Config::with_credential(
            self.config.credential.clone(),
            &self.config.api_url,
            Some(input.project_id.trim().to_string()),
        )?;
        let client = Client::new(config)?;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(timeout_seconds);
        let poll_interval = Duration::from_secs(poll_interval_seconds);
        let mut polls = 0_u64;
        let mut latest = None;
        let mut last_status = None;

        loop {
            // wait for next poll
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(timeout_result(
                    &target,
                    polls,
                    started.elapsed(),
                    last_status.as_deref(),
                    latest,
                ));
            }

            let fetched = match tokio::time::timeout(remaining, target.fetch(&client)).await {
                Ok(Ok(resource)) => resource,
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    return Ok(timeout_result(
                        &target,
                        polls,
                        started.elapsed(),
                        last_status.as_deref(),
                        latest,
                    ));
                }
            };
            let status = target.status(&fetched)?;
            polls += 1;

            // do a poll using nunu api
            if WaitTarget::is_complete(&status) {
                return Ok(completed_result(
                    &target,
                    polls,
                    started.elapsed(),
                    &status,
                    fetched,
                ));
            }
            last_status = Some(status);
            latest = Some(fetched);

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(timeout_result(
                    &target,
                    polls,
                    started.elapsed(),
                    last_status.as_deref(),
                    latest,
                ));
            }
            tokio::time::sleep(poll_interval.min(remaining)).await;
        }
    }
}

#[async_trait]
impl LocalTool for WaitForTool {
    fn name(&self) -> &'static str {
        NAME
    }

    fn definition(&self) -> Tool {
        definition()
    }

    async fn call(
        &self,
        arguments: Option<JsonObject>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let arguments = arguments.unwrap_or_default();
        let input = serde_json::from_value::<WaitForInput>(serde_json::Value::Object(arguments))
            .map_err(|error| {
                ErrorData::invalid_params(format!("invalid {NAME} arguments: {error}"), None)
            })?;
        let result = match self.wait(input).await {
            Ok(result) => result,
            Err(error) => CallToolResult::error(vec![ContentBlock::text(format!(
                "Waiting for Nunu execution failed: {error}"
            ))]),
        };
        Ok(result.into())
    }
}

fn completed_result(
    target: &WaitTarget,
    polls: u64,
    elapsed: Duration,
    status: &str,
    resource: Value,
) -> CallToolResult {
    let structured =
        structured_content("completed", target, polls, elapsed, status, Some(resource));
    let mut result = CallToolResult::success(vec![ContentBlock::text(format!(
        "{} '{}' completed after {polls} poll(s).",
        target.kind.as_str(),
        target.id
    ))]);
    result.structured_content = Some(Value::Object(structured));
    result
}

fn timeout_result(
    target: &WaitTarget,
    polls: u64,
    elapsed: Duration,
    last_status: Option<&str>,
    resource: Option<Value>,
) -> CallToolResult {
    let status_text = last_status.unwrap_or("unknown");
    let structured = structured_content("timed_out", target, polls, elapsed, status_text, resource);
    let mut result = CallToolResult::success(vec![ContentBlock::text(format!(
        "Timed out waiting for {} '{}' after {} second(s); last status: {status_text}.",
        target.kind.as_str(),
        target.id,
        elapsed.as_secs()
    ))]);
    result.structured_content = Some(Value::Object(structured));
    result
}

fn structured_content(
    status: &str,
    target: &WaitTarget,
    polls: u64,
    elapsed: Duration,
    last_status: &str,
    resource: Option<Value>,
) -> serde_json::Map<String, Value> {
    let mut structured = serde_json::Map::new();
    structured.insert("status".to_string(), Value::String(status.to_string()));
    structured.insert(
        "target_type".to_string(),
        Value::String(target.kind.as_str().to_string()),
    );
    structured.insert("target_id".to_string(), Value::String(target.id.clone()));
    structured.insert("polls".to_string(), serde_json::json!(polls));
    structured.insert(
        "elapsed_seconds".to_string(),
        serde_json::json!(elapsed.as_secs_f64()),
    );
    structured.insert(
        "last_status".to_string(),
        Value::String(last_status.to_string()),
    );
    structured.insert("resource".to_string(), resource.unwrap_or(Value::Null));
    structured
}

fn definition() -> Tool {
    let input_schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "project_id": {
                "type": "string",
                "minLength": 1,
                "description": "ID of the Nunu project containing the job, run, or test plan execution."
            },
            "job_id": {
                "type": "string",
                "minLength": 1,
                "description": "Wait for one job to reach COMPLETED. Supply exactly one target ID."
            },
            "run_id": {
                "type": "string",
                "minLength": 1,
                "description": "Wait for one multiplayer run to reach COMPLETED. Supply exactly one target ID."
            },
            "test_plan_execution_id": {
                "type": "string",
                "minLength": 1,
                "description": "Wait for one test plan execution to reach completed. Supply exactly one target ID."
            },
            "poll_interval_seconds": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_POLL_INTERVAL_SECONDS,
                "default": DEFAULT_POLL_INTERVAL_SECONDS,
                "description": "Seconds to sleep between status checks. Defaults to 60."
            },
            "timeout_seconds": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_TIMEOUT_SECONDS,
                "default": DEFAULT_TIMEOUT_SECONDS,
                "description": "Hard upper bound for the wait. Defaults to 1800 seconds."
            }
        },
        "required": ["project_id"],
        "oneOf": [
            { "required": ["job_id"] },
            { "required": ["run_id"] },
            { "required": ["test_plan_execution_id"] }
        ]
    });
    let output_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "status": { "type": "string", "enum": ["completed", "timed_out"] },
            "target_type": { "type": "string", "enum": ["job", "run", "test_plan_execution"] },
            "target_id": { "type": "string" },
            "polls": { "type": "integer" },
            "elapsed_seconds": { "type": "number" },
            "last_status": { "type": "string" },
            "resource": {}
        },
        "required": ["status", "target_type", "target_id", "polls", "elapsed_seconds", "last_status", "resource"]
    });
    Tool::new(
        NAME,
        "Wait for a Nunu job, run, or test plan execution to complete by polling its status. Returns the final resource or the latest resource when the hard timeout is reached.",
        input_schema.as_object().cloned().unwrap_or_default(),
    )
    .with_title("Wait for completion")
    .with_raw_output_schema(Arc::new(
        output_schema.as_object().cloned().unwrap_or_default(),
    ))
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_requires_exactly_one_id() {
        let input = WaitForInput {
            project_id: "project-1".to_string(),
            job_id: Some("job-1".to_string()),
            run_id: None,
            test_plan_execution_id: None,
            poll_interval_seconds: None,
            timeout_seconds: None,
        };
        let target = WaitTarget::from_input(&input).expect("job target");
        assert_eq!(target.kind.as_str(), "job");
        assert_eq!(target.id, "job-1");

        let mut invalid = input;
        invalid.run_id = Some("run-1".to_string());
        assert!(WaitTarget::from_input(&invalid).is_err());
    }

    #[test]
    fn definition_advertises_bounded_wait_options() {
        let tool = definition();
        assert_eq!(
            tool.input_schema["required"],
            serde_json::json!(["project_id"])
        );
        assert_eq!(
            tool.input_schema["properties"]["poll_interval_seconds"]["default"],
            DEFAULT_POLL_INTERVAL_SECONDS
        );
        assert_eq!(
            tool.input_schema["properties"]["timeout_seconds"]["maximum"],
            MAX_TIMEOUT_SECONDS
        );
        assert_eq!(
            tool.annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint),
            Some(true)
        );
    }
}
