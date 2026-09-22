use std::{ffi::OsString, sync::Arc};

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{APP_VERSION, civitai::DownloadRequest, state::AppState};

#[derive(Clone)]
pub struct McpBridge {
    state: Arc<AppState>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl McpBridge {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchModelsInput {
    /// Free-text Civitai model query.
    pub query: String,
    /// Optional Civitai model type, for example Checkpoint or LORA.
    #[serde(rename = "type")]
    pub model_type: Option<String>,
    /// Optional exact Civitai base-model family.
    pub base_model: Option<String>,
    /// Results per page, between 1 and 100. Defaults to 20.
    pub limit: Option<u8>,
    /// Cursor returned by an earlier search.
    pub cursor: Option<String>,
    /// Include NSFW catalog results. Defaults to false.
    #[serde(default)]
    pub nsfw: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ModelIdInput {
    pub model_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VersionIdInput {
    pub version_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JobIdInput {
    /// UUID returned by civitai_download.
    pub job_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListJobsInput {
    /// Maximum number of newest jobs to return. Defaults to 20.
    pub limit: Option<u16>,
}

#[derive(Debug, Serialize)]
struct ServerStatusOutput {
    app_version: &'static str,
    mcp_enabled: bool,
    comfyui_root: Option<String>,
    comfyui_url: Option<String>,
    comfyui_url_source: String,
    comfyui_url_verified: bool,
    civitai_cli: crate::state::CliStatus,
    active_downloads: usize,
}

#[tool_router]
impl McpBridge {
    #[tool(
        name = "server_status",
        description = "Return MCP switch state, detected ComfyUI root/URL, Civitai CLI status, and active download count. Secrets are never returned."
    )]
    async fn server_status(&self) -> Result<CallToolResult, McpError> {
        let config = self.state.config();
        let jobs = self.state.list_jobs();
        let url_status = self.state.comfyui_url_status();
        structured(ServerStatusOutput {
            app_version: APP_VERSION,
            mcp_enabled: config.mcp_enabled,
            comfyui_root: self
                .state
                .effective_comfyui_root()
                .map(|path| path.display().to_string()),
            comfyui_url: url_status.url,
            comfyui_url_source: url_status.source,
            comfyui_url_verified: url_status.verified,
            civitai_cli: self.state.cli_status(),
            active_downloads: jobs
                .iter()
                .filter(|job| {
                    matches!(
                        job.status,
                        crate::state::JobStatus::Queued | crate::state::JobStatus::Running
                    )
                })
                .count(),
        })
    }

    #[tool(
        name = "comfyui_discover",
        description = "Rescan this Windows host for ComfyUI installations and return ranked candidates. A manual path configured in the local panel has priority."
    )]
    async fn comfyui_discover(&self) -> Result<CallToolResult, McpError> {
        let paths = self.state.refresh_discovery();
        let url = self.state.refresh_comfyui_url().await;
        structured(json!({"paths": paths, "url": url}))
    }

    #[tool(
        name = "civitai_search_models",
        description = "Search the public Civitai model catalog through the Civitai CLI installed on this host."
    )]
    async fn civitai_search_models(
        &self,
        Parameters(input): Parameters<SearchModelsInput>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(error) = validate_query(&input.query, "query") {
            return Ok(tool_error(error));
        }
        let limit = input.limit.unwrap_or(20).clamp(1, 100);
        let mut args = vec![
            OsString::from("models"),
            OsString::from("search"),
            OsString::from("--query"),
            OsString::from(input.query),
            OsString::from("--limit"),
            OsString::from(limit.to_string()),
        ];
        if let Some(value) = input.model_type {
            if let Err(error) = validate_query(&value, "type") {
                return Ok(tool_error(error));
            }
            args.extend([OsString::from("--type"), OsString::from(value)]);
        }
        if let Some(value) = input.base_model {
            if let Err(error) = validate_query(&value, "base_model") {
                return Ok(tool_error(error));
            }
            args.extend([OsString::from("--base-model"), OsString::from(value)]);
        }
        if let Some(value) = input.cursor {
            if let Err(error) = validate_query(&value, "cursor") {
                return Ok(tool_error(error));
            }
            args.extend([OsString::from("--cursor"), OsString::from(value)]);
        }
        if input.nsfw {
            args.push(OsString::from("--nsfw"));
        }

        match self.state.run_civitai_json(args).await {
            Ok(value) => Ok(CallToolResult::structured(value)),
            Err(error) => Ok(tool_error(format!("Civitai 搜索失败：{error:#}"))),
        }
    }

    #[tool(
        name = "civitai_get_model",
        description = "Get one Civitai model and its full version list through the local Civitai CLI."
    )]
    async fn civitai_get_model(
        &self,
        Parameters(ModelIdInput { model_id }): Parameters<ModelIdInput>,
    ) -> Result<CallToolResult, McpError> {
        if model_id == 0 {
            return Ok(tool_error("model_id 必须大于 0"));
        }
        let args = vec![
            OsString::from("models"),
            OsString::from("get"),
            OsString::from(model_id.to_string()),
        ];
        match self.state.run_civitai_json(args).await {
            Ok(value) => Ok(CallToolResult::structured(value)),
            Err(error) => Ok(tool_error(format!("读取 Civitai 模型失败：{error:#}"))),
        }
    }

    #[tool(
        name = "civitai_get_version",
        description = "Get one exact Civitai model version, including its downloadable files, sizes, and hashes."
    )]
    async fn civitai_get_version(
        &self,
        Parameters(VersionIdInput { version_id }): Parameters<VersionIdInput>,
    ) -> Result<CallToolResult, McpError> {
        if version_id == 0 {
            return Ok(tool_error("version_id 必须大于 0"));
        }
        let args = vec![
            OsString::from("model-versions"),
            OsString::from("get"),
            OsString::from(version_id.to_string()),
        ];
        match self.state.run_civitai_json(args).await {
            Ok(value) => Ok(CallToolResult::structured(value)),
            Err(error) => Ok(tool_error(format!("读取 Civitai 版本失败：{error:#}"))),
        }
    }

    #[tool(
        name = "civitai_plan_download",
        description = "Resolve a model download without transferring bytes. Returns Civitai CLI target paths, sizes, SHA256 values, and authentication notes. target_dir is a path on this MCP host, not on the calling agent."
    )]
    async fn civitai_plan_download(
        &self,
        Parameters(request): Parameters<DownloadRequest>,
    ) -> Result<CallToolResult, McpError> {
        match self.state.plan_download(request).await {
            Ok(output) => structured(output),
            Err(error) => Ok(tool_error(format!("下载计划失败：{error:#}"))),
        }
    }

    #[tool(
        name = "civitai_download",
        description = "Start one asynchronous Civitai model download on this MCP host. With no target_dir, files are routed into the detected ComfyUI tree. An explicit target_dir must pass the local panel's allowlist. Returns a job UUID immediately."
    )]
    async fn civitai_download(
        &self,
        Parameters(mut request): Parameters<DownloadRequest>,
    ) -> Result<CallToolResult, McpError> {
        request.dry_run = false;
        match self.state.start_download(request) {
            Ok(job) => structured(job),
            Err(error) => Ok(tool_error(format!("无法启动下载：{error:#}"))),
        }
    }

    #[tool(
        name = "civitai_download_status",
        description = "Return the current state and captured output for a download job UUID."
    )]
    async fn civitai_download_status(
        &self,
        Parameters(input): Parameters<JobIdInput>,
    ) -> Result<CallToolResult, McpError> {
        let id = match Uuid::parse_str(&input.job_id) {
            Ok(id) => id,
            Err(_) => return Ok(tool_error("job_id 不是有效 UUID")),
        };
        match self.state.job(id) {
            Some(job) => structured(job),
            None => Ok(tool_error(format!("下载任务不存在：{id}"))),
        }
    }

    #[tool(
        name = "civitai_downloads_list",
        description = "List the newest local Civitai download jobs."
    )]
    async fn civitai_downloads_list(
        &self,
        Parameters(input): Parameters<ListJobsInput>,
    ) -> Result<CallToolResult, McpError> {
        let limit = usize::from(input.limit.unwrap_or(20).clamp(1, 100));
        structured(
            self.state
                .list_jobs()
                .into_iter()
                .take(limit)
                .collect::<Vec<_>>(),
        )
    }

    #[tool(
        name = "civitai_download_cancel",
        description = "Cancel a queued or running local Civitai download job. Partial .part files remain managed by the Civitai CLI."
    )]
    async fn civitai_download_cancel(
        &self,
        Parameters(input): Parameters<JobIdInput>,
    ) -> Result<CallToolResult, McpError> {
        let id = match Uuid::parse_str(&input.job_id) {
            Ok(id) => id,
            Err(_) => return Ok(tool_error("job_id 不是有效 UUID")),
        };
        match self.state.cancel_job(id) {
            Ok(job) => structured(job),
            Err(error) => Ok(tool_error(format!("取消失败：{error:#}"))),
        }
    }
}

#[tool_handler]
impl ServerHandler for McpBridge {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("comfyui-mcp-bridge", APP_VERSION)
                    .with_title("ComfyUI MCP Bridge")
                    .with_description(
                    "Portable Civitai model download bridge for a local ComfyUI installation"
                        .to_owned(),
                ),
            )
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_instructions(
                "Use civitai_plan_download before large or unfamiliar downloads. Paths refer to the Windows machine running this MCP server. Omit target_dir for automatic ComfyUI model routing. Civitai credentials stay local and are never returned. Downloads are asynchronous: call civitai_download_status with the returned job_id."
                    .to_owned(),
            )
    }
}

fn structured(value: impl Serialize) -> Result<CallToolResult, McpError> {
    match serde_json::to_value(value) {
        Ok(value) => Ok(CallToolResult::structured(value)),
        Err(error) => Err(McpError::internal_error(
            "无法序列化工具结果",
            Some(json!({"error": error.to_string()})),
        )),
    }
}

fn tool_error(message: impl ToString) -> CallToolResult {
    let message = message.to_string();
    CallToolResult::structured_error(json!({
        "ok": false,
        "error": message,
    }))
}

fn validate_query(value: &str, field: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{field} 不能为空"));
    }
    if value.len() > 1_024 {
        return Err(format!("{field} 过长"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{field} 不能包含控制字符"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_validation_rejects_empty_control_and_huge_values() {
        assert!(validate_query("", "query").is_err());
        assert!(validate_query("hello\nworld", "query").is_err());
        assert!(validate_query(&"x".repeat(1_025), "query").is_err());
        assert!(validate_query("pony diffusion", "query").is_ok());
    }
}
