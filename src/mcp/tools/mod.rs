mod upload_build;
mod wait_for;

use crate::config::Config;
use async_trait::async_trait;
use rmcp::{
    ErrorData,
    model::{CallToolRequestParams, CallToolResponse, JsonObject, Tool},
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

#[async_trait]
trait LocalTool: Send + Sync {
    fn name(&self) -> &'static str;

    fn definition(&self) -> Tool;

    async fn call(
        &self,
        arguments: Option<JsonObject>,
    ) -> std::result::Result<CallToolResponse, ErrorData>;
}

#[derive(Clone)]
pub(super) struct LocalToolRegistry {
    tools: Arc<HashMap<&'static str, Arc<dyn LocalTool>>>,
}

impl LocalToolRegistry {
    pub(super) fn standard(config: Config, allowed_root: PathBuf) -> Self {
        Self::new([
            Arc::new(upload_build::UploadBuildTool::new(
                config.clone(),
                allowed_root,
            )) as Arc<dyn LocalTool>,
            Arc::new(wait_for::WaitForTool::new(config)) as Arc<dyn LocalTool>,
        ])
    }

    fn new(tools: impl IntoIterator<Item = Arc<dyn LocalTool>>) -> Self {
        Self {
            tools: Arc::new(tools.into_iter().map(|tool| (tool.name(), tool)).collect()),
        }
    }

    pub(super) fn merged_with(&self, remote_tools: Vec<Tool>) -> Vec<Tool> {
        let mut tools: Vec<_> = remote_tools
            .into_iter()
            .filter(|tool| !self.tools.contains_key(tool.name.as_ref()))
            .collect();
        tools.extend(self.tools.values().map(|tool| tool.definition()));
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    pub(super) async fn call(
        &self,
        request: &CallToolRequestParams,
    ) -> Option<std::result::Result<CallToolResponse, ErrorData>> {
        let tool = self.tools.get(request.name.as_ref())?;
        Some(tool.call(request.arguments.clone()).await)
    }
}
