use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    bundled::ensure_bundled_civitai,
    civitai::{CivitaiCli, CliInfo, CliOutput, DownloadRequest},
    comfyui_url::{ComfyUrlStatus, discover_comfyui_url},
    config::{AppConfig, generate_token},
    discovery::{DiscoveryReport, discover_comfyui, validate_comfyui_root},
    logging::AppLogger,
    path_utils::strip_windows_verbatim_prefix,
};

#[derive(Debug, Clone, Serialize)]
pub struct CliStatus {
    pub available: bool,
    pub path: Option<PathBuf>,
    pub launcher: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
}

impl CliStatus {
    fn pending() -> Self {
        Self {
            available: false,
            path: None,
            launcher: None,
            version: None,
            error: Some("尚未检测".to_owned()),
        }
    }

    fn success(info: CliInfo) -> Self {
        Self {
            available: true,
            path: Some(info.path),
            launcher: Some(format!("{:?}", info.launcher).to_lowercase()),
            version: Some(info.version),
            error: None,
        }
    }

    fn failure(error: impl ToString) -> Self {
        Self {
            available: false,
            path: None,
            launcher: None,
            version: None,
            error: Some(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadJob {
    pub id: Uuid,
    pub status: JobStatus,
    pub model_label: String,
    pub target: Option<PathBuf>,
    pub request: DownloadRequest,
    pub created_at: String,
    pub updated_at: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
}

impl DownloadJob {
    fn new(request: DownloadRequest, effective_root: Option<PathBuf>) -> Self {
        let now = now();
        let model_label = request
            .version_id
            .map(|id| format!("version:{id}"))
            .or_else(|| request.model_id.map(|id| format!("model:{id}")))
            .unwrap_or_else(|| "unknown".to_owned());
        Self {
            id: Uuid::new_v4(),
            status: JobStatus::Queued,
            model_label,
            target: request.target_dir.clone().or(effective_root),
            request,
            created_at: now.clone(),
            updated_at: now,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: None,
        }
    }
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub logger: AppLogger,
    admin_token: String,
    bundled_cli_path: PathBuf,
    config: RwLock<AppConfig>,
    discovery: RwLock<DiscoveryReport>,
    comfyui_url_status: RwLock<ComfyUrlStatus>,
    cli_status: RwLock<CliStatus>,
    resolved_cli: RwLock<Option<CivitaiCli>>,
    jobs: RwLock<HashMap<Uuid, DownloadJob>>,
    job_cancellations: RwLock<HashMap<Uuid, CancellationToken>>,
    cli_slots: Semaphore,
    cli_probe_commit: Mutex<()>,
    cli_probe_generation: AtomicU64,
    last_auth_warning_ms: AtomicU64,
    pub shutdown: CancellationToken,
}

impl AppState {
    pub fn load(data_dir: PathBuf) -> Result<Arc<Self>> {
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("无法创建便携数据目录 {}", data_dir.display()))?;
        let config_path = data_dir.join("config.json");
        let config = AppConfig::load_or_create(&config_path)
            .with_context(|| format!("无法读取配置 {}", config_path.display()))?;
        let logger = AppLogger::new(&data_dir)?;
        let bundled_cli_path =
            ensure_bundled_civitai(&data_dir).context("无法解包内置 Civitai CLI")?;
        let discovery = discover_comfyui(config.comfyui_path.as_deref());
        let comfyui_url_status = ComfyUrlStatus::pending(Some(&config.comfyui_url));
        let selected = discovery
            .selected
            .as_ref()
            .map(|candidate| candidate.path.display().to_string())
            .unwrap_or_else(|| "未找到".to_owned());
        logger.info(format!("ComfyUI 自动发现完成，当前选择：{selected}"));
        logger.info(format!(
            "内置 Civitai CLI 已就绪：{}",
            bundled_cli_path.display()
        ));

        Ok(Arc::new(Self {
            data_dir,
            config_path,
            logger,
            admin_token: generate_token(),
            bundled_cli_path,
            config: RwLock::new(config),
            discovery: RwLock::new(discovery),
            comfyui_url_status: RwLock::new(comfyui_url_status),
            cli_status: RwLock::new(CliStatus::pending()),
            resolved_cli: RwLock::new(None),
            jobs: RwLock::new(HashMap::new()),
            job_cancellations: RwLock::new(HashMap::new()),
            cli_slots: Semaphore::new(4),
            cli_probe_commit: Mutex::new(()),
            cli_probe_generation: AtomicU64::new(0),
            last_auth_warning_ms: AtomicU64::new(0),
            shutdown: CancellationToken::new(),
        }))
    }

    pub fn config(&self) -> AppConfig {
        self.config.read().expect("config lock poisoned").clone()
    }

    /// Ephemeral secret used only by the loopback management panel.
    ///
    /// It intentionally differs from the LAN MCP token so a reverse proxy
    /// that forwards loopback traffic cannot turn an MCP credential into an
    /// administrative credential.
    pub fn admin_token(&self) -> &str {
        &self.admin_token
    }

    pub fn replace_config(&self, config: AppConfig) -> Result<()> {
        let _commit = self
            .cli_probe_commit
            .lock()
            .expect("CLI probe commit lock poisoned");
        let cli_changed = self.config().civitai_cli_path != config.civitai_cli_path;
        config.save(&self.config_path)?;
        *self.config.write().expect("config lock poisoned") = config;
        if cli_changed {
            self.cli_probe_generation.fetch_add(1, Ordering::SeqCst);
            *self
                .resolved_cli
                .write()
                .expect("resolved CLI lock poisoned") = None;
        }
        Ok(())
    }

    pub fn set_mcp_enabled(&self, enabled: bool) -> Result<()> {
        let mut config = self.config();
        config.mcp_enabled = enabled;
        self.replace_config(config)?;
        self.logger.info(if enabled {
            "MCP 服务已启用"
        } else {
            "MCP 服务已停用"
        });
        Ok(())
    }

    pub fn regenerate_access_token(&self) -> Result<String> {
        let mut config = self.config();
        let token = config.regenerate_token();
        self.replace_config(config)?;
        self.logger.warn("MCP 访问令牌已重新生成，旧令牌立即失效");
        Ok(token)
    }

    pub fn log_auth_rejection(&self, message: impl AsRef<str>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let previous = self.last_auth_warning_ms.load(Ordering::Relaxed);
        if now.saturating_sub(previous) < 5_000 {
            return;
        }
        if self
            .last_auth_warning_ms
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            self.logger.warn(message);
        }
    }

    pub fn discovery(&self) -> DiscoveryReport {
        self.discovery
            .read()
            .expect("discovery lock poisoned")
            .clone()
    }

    pub fn refresh_discovery(&self) -> DiscoveryReport {
        let manual = self.config().comfyui_path;
        let report = discover_comfyui(manual.as_deref());
        let selected = report
            .selected
            .as_ref()
            .map(|candidate| candidate.path.display().to_string())
            .unwrap_or_else(|| "未找到".to_owned());
        self.logger
            .info(format!("重新扫描 ComfyUI 完成，当前选择：{selected}"));
        *self.discovery.write().expect("discovery lock poisoned") = report.clone();
        report
    }

    pub fn comfyui_url_status(&self) -> ComfyUrlStatus {
        self.comfyui_url_status
            .read()
            .expect("ComfyUI URL status lock poisoned")
            .clone()
    }

    pub async fn refresh_comfyui_url(&self) -> ComfyUrlStatus {
        let manual_url = self.config().comfyui_url;
        let status = discover_comfyui_url(Some(&manual_url)).await;
        // Do not let an older, slower probe overwrite a just-saved manual URL.
        if self.config().comfyui_url != manual_url {
            return self.comfyui_url_status();
        }
        let url = status.url.as_deref().unwrap_or("未找到");
        self.logger.info(format!(
            "ComfyUI HTTP 发现完成：{url}（来源 {}，验证 {}）",
            status.source,
            if status.verified {
                "通过"
            } else {
                "未通过"
            }
        ));
        *self
            .comfyui_url_status
            .write()
            .expect("ComfyUI URL status lock poisoned") = status.clone();
        status
    }

    pub fn effective_comfyui_url(&self) -> Option<String> {
        self.comfyui_url_status().url
    }

    pub fn effective_comfyui_root(&self) -> Option<PathBuf> {
        let config = self.config();
        if let Some(path) = config
            .comfyui_path
            .filter(|path| validate_comfyui_root(path))
        {
            return Some(
                fs::canonicalize(&path)
                    .map(strip_windows_verbatim_prefix)
                    .unwrap_or(path),
            );
        }
        self.discovery().selected.map(|candidate| candidate.path)
    }

    pub fn cli_status(&self) -> CliStatus {
        self.cli_status
            .read()
            .expect("cli status lock poisoned")
            .clone()
    }

    pub async fn refresh_cli(&self) -> CliStatus {
        let (generation, configured) = {
            let _commit = self
                .cli_probe_commit
                .lock()
                .expect("CLI probe commit lock poisoned");
            let generation = self.cli_probe_generation.fetch_add(1, Ordering::SeqCst) + 1;
            (generation, self.config().civitai_cli_path)
        };
        let _permit = self
            .cli_slots
            .acquire()
            .await
            .expect("CLI semaphore is never closed");
        let probe = self.probe_preferred_cli(configured.clone()).await;
        let _commit = self
            .cli_probe_commit
            .lock()
            .expect("CLI probe commit lock poisoned");
        if self.cli_probe_generation.load(Ordering::SeqCst) != generation
            || self.config().civitai_cli_path != configured
        {
            return self.cli_status();
        }
        let status = match probe {
            Ok((cli, info)) => {
                self.logger.info(format!(
                    "Civitai CLI 可用：{} ({})",
                    info.path.display(),
                    info.version.trim()
                ));
                *self
                    .resolved_cli
                    .write()
                    .expect("resolved CLI lock poisoned") = Some(cli);
                CliStatus::success(info)
            }
            Err(error) => {
                *self
                    .resolved_cli
                    .write()
                    .expect("resolved CLI lock poisoned") = None;
                self.logger.warn(format!("Civitai CLI 检测失败：{error:#}"));
                CliStatus::failure(format!("{error:#}"))
            }
        };
        *self.cli_status.write().expect("cli status lock poisoned") = status.clone();
        status
    }

    pub fn list_jobs(&self) -> Vec<DownloadJob> {
        let mut jobs: Vec<_> = self
            .jobs
            .read()
            .expect("jobs lock poisoned")
            .values()
            .cloned()
            .collect();
        jobs.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        jobs
    }

    pub fn job(&self, id: Uuid) -> Option<DownloadJob> {
        self.jobs
            .read()
            .expect("jobs lock poisoned")
            .get(&id)
            .cloned()
    }

    pub fn cancel_job(&self, id: Uuid) -> Result<DownloadJob> {
        let job = self
            .job(id)
            .ok_or_else(|| anyhow!("下载任务不存在：{id}"))?;
        if !job.status.is_active() {
            bail!("下载任务 {id} 已结束，不能取消");
        }
        let token = self
            .job_cancellations
            .read()
            .expect("job cancellations lock poisoned")
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("下载任务 {id} 没有活动进程"))?;
        token.cancel();
        self.logger.warn(format!("已请求取消下载任务 {id}"));
        Ok(job)
    }

    pub fn start_download(self: &Arc<Self>, request: DownloadRequest) -> Result<DownloadJob> {
        request.validate()?;
        let config = self.config();
        let effective_root = self.effective_comfyui_root();
        let cli = self.resolve_civitai_cli()?;

        request.build_validated_download_args(
            effective_root.as_deref(),
            &config.allowed_download_roots,
            config.allow_custom_download_dirs,
        )?;

        let job = DownloadJob::new(request, effective_root.clone());
        let id = job.id;
        let cancellation = CancellationToken::new();
        {
            // The active-job check and insertion must be one write-locked
            // transaction; otherwise two simultaneous MCP calls can both
            // observe an empty queue and start separate downloads.
            let mut jobs = self.jobs.write().expect("jobs lock poisoned");
            if jobs.values().any(|job| job.status.is_active()) {
                bail!("已有下载任务在运行；当前版本默认只允许一个并发下载");
            }
            self.job_cancellations
                .write()
                .expect("job cancellations lock poisoned")
                .insert(id, cancellation.clone());
            jobs.insert(id, job.clone());
        }

        let state = Arc::clone(self);
        tokio::spawn(async move {
            let permit = tokio::select! {
                _ = cancellation.cancelled() => None,
                permit = state.cli_slots.acquire() => Some(
                    permit.expect("CLI semaphore is never closed")
                ),
            };
            let Some(_permit) = permit else {
                state.update_job(id, |job| {
                    job.status = JobStatus::Cancelled;
                    job.error = Some("用户取消".to_owned());
                });
                state
                    .job_cancellations
                    .write()
                    .expect("job cancellations lock poisoned")
                    .remove(&id);
                state.trim_job_history();
                return;
            };

            state.update_job(id, |job| job.status = JobStatus::Running);
            state.logger.info(format!(
                "开始下载任务 {id}（{}）",
                state.job(id).map(|job| job.model_label).unwrap_or_default()
            ));

            let request = state.job(id).expect("job exists").request;

            let result = tokio::select! {
                _ = cancellation.cancelled() => None,
                result = cli.download(
                    &request,
                    effective_root.as_deref(),
                    &config.allowed_download_roots,
                    config.allow_custom_download_dirs,
                ) => Some(result),
            };

            match result {
                None => {
                    state.update_job(id, |job| {
                        job.status = JobStatus::Cancelled;
                        job.error = Some("用户取消".to_owned());
                    });
                    state.logger.warn(format!("下载任务 {id} 已取消"));
                }
                Some(Ok(output)) => {
                    state.finish_job_success(id, output);
                    state.logger.info(format!("下载任务 {id} 已完成"));
                }
                Some(Err(error)) => {
                    let message = format!("{error:#}");
                    state.update_job(id, |job| {
                        job.status = JobStatus::Failed;
                        job.error = Some(message.clone());
                    });
                    state.logger.error(format!("下载任务 {id} 失败：{message}"));
                }
            }
            state
                .job_cancellations
                .write()
                .expect("job cancellations lock poisoned")
                .remove(&id);
            state.trim_job_history();
        });

        Ok(job)
    }

    pub async fn plan_download(&self, mut request: DownloadRequest) -> Result<CliOutput> {
        let _permit = self
            .cli_slots
            .acquire()
            .await
            .expect("CLI semaphore is never closed");
        request.dry_run = true;
        let config = self.config();
        let root = self.effective_comfyui_root();
        let cli = self.resolve_civitai_cli()?;
        cli.download(
            &request,
            root.as_deref(),
            &config.allowed_download_roots,
            config.allow_custom_download_dirs,
        )
        .await
    }

    pub async fn run_civitai_json(&self, args: Vec<OsString>) -> Result<Value> {
        let _permit = self
            .cli_slots
            .acquire()
            .await
            .expect("CLI semaphore is never closed");
        let cli = self.resolve_civitai_cli()?;
        cli.run_json(args).await
    }

    fn resolve_civitai_cli(&self) -> Result<CivitaiCli> {
        if let Some(cli) = self
            .resolved_cli
            .read()
            .expect("resolved CLI lock poisoned")
            .clone()
        {
            return Ok(cli);
        }
        let configured = self.config().civitai_cli_path;
        if let Some(path) = configured {
            CivitaiCli::discover(Some(&path))
        } else {
            // Until the asynchronous startup probe has selected and cached a
            // working PATH CLI, the embedded executable is the safe fallback.
            CivitaiCli::from_path(&self.bundled_cli_path)
        }
    }

    async fn probe_preferred_cli(
        &self,
        configured: Option<PathBuf>,
    ) -> Result<(CivitaiCli, CliInfo)> {
        match CivitaiCli::discover(configured.as_deref()) {
            Ok(cli) => match cli.probe().await {
                Ok(info) => Ok((cli, info)),
                Err(error) if configured.is_none() => {
                    self.logger.warn(format!(
                        "PATH 中的 Civitai CLI 无法运行，将尝试内置版本：{error:#}"
                    ));
                    self.probe_bundled_cli().await.with_context(|| {
                        format!("PATH CLI 无法运行（{error:#}），内置 CLI 也不可用")
                    })
                }
                Err(error) => Err(error),
            },
            Err(error) if configured.is_none() => self
                .probe_bundled_cli()
                .await
                .with_context(|| format!("未找到可用的系统 CLI（{error:#}），内置 CLI 也不可用")),
            Err(error) => Err(error),
        }
    }

    async fn probe_bundled_cli(&self) -> Result<(CivitaiCli, CliInfo)> {
        let cli = CivitaiCli::from_path(&self.bundled_cli_path)?;
        let info = cli.probe().await?;
        Ok((cli, info))
    }

    fn update_job(&self, id: Uuid, update: impl FnOnce(&mut DownloadJob)) {
        if let Some(job) = self.jobs.write().expect("jobs lock poisoned").get_mut(&id) {
            update(job);
            job.updated_at = now();
        }
    }

    fn finish_job_success(&self, id: Uuid, output: CliOutput) {
        self.update_job(id, |job| {
            job.status = JobStatus::Completed;
            job.exit_code = Some(output.exit_code);
            job.stdout = truncate_output(output.stdout);
            job.stderr = truncate_output(output.stderr);
        });
    }

    fn trim_job_history(&self) {
        let mut jobs = self.jobs.write().expect("jobs lock poisoned");
        if jobs.len() <= 100 {
            return;
        }
        let mut terminal: Vec<_> = jobs
            .values()
            .filter(|job| !job.status.is_active())
            .map(|job| (job.created_at.clone(), job.id))
            .collect();
        terminal.sort();
        let remove_count = jobs.len().saturating_sub(100);
        for (_, id) in terminal.into_iter().take(remove_count) {
            jobs.remove(&id);
        }
    }
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn truncate_output(mut value: String) -> String {
    const MAX: usize = 64 * 1024;
    if value.len() <= MAX {
        return value;
    }
    let mut boundary = MAX;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str("\n[输出已截断]");
    value
}

pub fn path_is_local_file(path: &Path) -> bool {
    path.is_file() && !path.as_os_str().to_string_lossy().starts_with(r"\\")
}
