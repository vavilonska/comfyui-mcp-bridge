use std::{
    env,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process::Command,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, patch, post},
};
use local_ip_address::list_afinet_netifas;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use tower_http::limit::RequestBodyLimitLayer;
use uuid::Uuid;

use crate::{
    APP_VERSION, bundled::bundled_civitai_license, config::AppConfig, mcp::McpBridge,
    path_utils::strip_windows_verbatim_prefix, state::AppState,
};

const INDEX_HTML: &str = include_str!("../ui/index.html");

#[derive(Clone)]
pub struct WebState {
    pub app: Arc<AppState>,
    runtime_bind_address: String,
    runtime_port: u16,
    request_slots: Arc<Semaphore>,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    app_version: &'static str,
    mcp_enabled: bool,
    endpoint: String,
    lan_urls: Vec<String>,
    access_token: String,
    config: AppConfig,
    discovery: crate::discovery::DiscoveryReport,
    url_discovery: crate::comfyui_url::ComfyUrlStatus,
    cli: crate::state::CliStatus,
    jobs: Vec<crate::state::DownloadJob>,
    data_dir: String,
    log_path: String,
    restart_required: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigPatch {
    bind_address: Option<String>,
    port: Option<u16>,
    comfyui_path: Option<Option<PathBuf>>,
    comfyui_url: Option<String>,
    civitai_cli_path: Option<Option<PathBuf>>,
    allow_custom_download_dirs: Option<bool>,
    allowed_download_roots: Option<Vec<PathBuf>>,
    auto_open_panel: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ToggleRequest {
    enabled: bool,
}

#[derive(Debug, Default, Deserialize)]
struct LogQuery {
    after: Option<u64>,
    limit: Option<usize>,
}

pub async fn serve(app: Arc<AppState>) -> Result<()> {
    let config = app.config();
    let bind_ip = config
        .bind_address
        .parse::<IpAddr>()
        .with_context(|| format!("监听地址无效：{}", config.bind_address))?;
    let bind = SocketAddr::new(bind_ip, config.port);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("无法监听 {bind}；请检查端口是否被占用"))?;

    let web_state = WebState {
        app: Arc::clone(&app),
        runtime_bind_address: config.bind_address.clone(),
        runtime_port: config.port,
        request_slots: Arc::new(Semaphore::new(64)),
    };

    let factory_state = Arc::clone(&app);
    let mcp_service = StreamableHttpService::new(
        move || Ok(McpBridge::new(Arc::clone(&factory_state))),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_json_response(true)
            .with_allowed_hosts(allowed_hosts(&config))
            .with_allowed_origins(allowed_origins(&config))
            .with_cancellation_token(app.shutdown.child_token()),
    );

    let router = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/licenses/civitai", get(civitai_license))
        .route("/api/status", get(api_status))
        .route("/api/config", patch(api_patch_config))
        .route("/api/mcp/toggle", post(api_toggle_mcp))
        .route("/api/token/regenerate", post(api_regenerate_token))
        .route("/api/discover", post(api_discover))
        .route("/api/cli/probe", post(api_probe_cli))
        .route("/api/logs", get(api_logs))
        .route("/api/jobs", get(api_jobs))
        .route("/api/jobs/{id}/cancel", post(api_cancel_job))
        .route("/api/open/data", post(api_open_data))
        .route("/api/open/log", post(api_open_log))
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            web_state.clone(),
            request_gate,
        ))
        .layer(RequestBodyLimitLayer::new(1024 * 1024))
        .with_state(web_state);

    app.logger.info(format!(
        "管理面板与 MCP 服务开始监听 http://{bind}（MCP 路径 /mcp）"
    ));

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(app.shutdown.clone().cancelled_owned())
    .await
    .context("HTTP 服务异常退出")
}

pub fn panel_url(bind_address: &str, port: u16, admin_token: &str) -> String {
    format!(
        "{}/?token={admin_token}",
        loopback_base_url(bind_address, port)
    )
}

async fn index() -> impl IntoResponse {
    let mut response = Html(INDEX_HTML).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn health() -> impl IntoResponse {
    Json(json!({"ok": true, "name": "comfyui-mcp-bridge", "version": APP_VERSION}))
}

async fn civitai_license() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        bundled_civitai_license(),
    )
}

async fn api_status(State(state): State<WebState>) -> Json<StatusResponse> {
    let config = state.app.config();
    let lan_urls = lan_base_urls(&state.runtime_bind_address, state.runtime_port);
    let endpoint = lan_urls
        .first()
        .map(|url| format!("{url}/mcp"))
        .unwrap_or_else(|| {
            format!(
                "{}/mcp",
                loopback_base_url(&state.runtime_bind_address, state.runtime_port)
            )
        });
    Json(StatusResponse {
        app_version: APP_VERSION,
        mcp_enabled: config.mcp_enabled,
        endpoint,
        lan_urls,
        access_token: config.access_token.clone(),
        config: config.clone(),
        discovery: state.app.discovery(),
        url_discovery: state.app.comfyui_url_status(),
        cli: state.app.cli_status(),
        jobs: state.app.list_jobs(),
        data_dir: state.app.data_dir.display().to_string(),
        log_path: state.app.logger.file_path().display().to_string(),
        restart_required: config.bind_address != state.runtime_bind_address
            || config.port != state.runtime_port,
    })
}

async fn api_patch_config(
    State(state): State<WebState>,
    Json(patch): Json<ConfigPatch>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut config = state.app.config();
    let previous = config.clone();

    if let Some(value) = patch.bind_address {
        value
            .parse::<IpAddr>()
            .map_err(|_| ApiError::bad_request("监听地址必须是有效 IPv4 或 IPv6 地址"))?;
        config.bind_address = value;
    }
    if let Some(value) = patch.port {
        if value == 0 {
            return Err(ApiError::bad_request("端口必须在 1 到 65535 之间"));
        }
        config.port = value;
    }
    if let Some(value) = patch.comfyui_path {
        config.comfyui_path = value.map(normalize_user_path).transpose()?;
    }
    if let Some(value) = patch.comfyui_url {
        let value = value.trim();
        if !value.is_empty() {
            validate_http_url(value).map_err(ApiError::bad_request)?;
        }
        config.comfyui_url = value.trim_end_matches('/').to_owned();
    }
    if let Some(value) = patch.civitai_cli_path {
        config.civitai_cli_path = value.map(normalize_user_path).transpose()?;
    }
    if let Some(value) = patch.allow_custom_download_dirs {
        config.allow_custom_download_dirs = value;
    }
    if let Some(values) = patch.allowed_download_roots {
        let mut roots = Vec::with_capacity(values.len());
        for path in values {
            let path = normalize_user_path(path)?;
            if !path.is_absolute() {
                return Err(ApiError::bad_request("批准的下载根目录必须是绝对路径"));
            }
            if path.as_os_str().to_string_lossy().starts_with(r"\\") {
                return Err(ApiError::bad_request("不允许 UNC 网络路径"));
            }
            roots.push(path);
        }
        config.allowed_download_roots = roots;
    }
    if let Some(value) = patch.auto_open_panel {
        config.auto_open_panel = value;
    }

    state
        .app
        .replace_config(config.clone())
        .map_err(ApiError::internal)?;
    let discovery_changed = previous.comfyui_path != config.comfyui_path;
    let url_changed = previous.comfyui_url != config.comfyui_url;
    let cli_changed = previous.civitai_cli_path != config.civitai_cli_path;
    if discovery_changed {
        state.app.refresh_discovery();
    }
    if cli_changed {
        state.app.refresh_cli().await;
    }
    if url_changed {
        state.app.refresh_comfyui_url().await;
    }
    state.app.logger.info("面板配置已保存");
    Ok(Json(json!({
        "ok": true,
        "restart_required": config.bind_address != state.runtime_bind_address || config.port != state.runtime_port
    })))
}

async fn api_toggle_mcp(
    State(state): State<WebState>,
    Json(request): Json<ToggleRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    state
        .app
        .set_mcp_enabled(request.enabled)
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "enabled": request.enabled})))
}

async fn api_regenerate_token(State(state): State<WebState>) -> ApiResult<Json<serde_json::Value>> {
    let token = state
        .app
        .regenerate_access_token()
        .map_err(ApiError::internal)?;
    Ok(Json(json!({"ok": true, "access_token": token})))
}

async fn api_discover(State(state): State<WebState>) -> Json<serde_json::Value> {
    let paths = state.app.refresh_discovery();
    let url = state.app.refresh_comfyui_url().await;
    Json(json!({"paths": paths, "url": url}))
}

async fn api_probe_cli(State(state): State<WebState>) -> Json<crate::state::CliStatus> {
    Json(state.app.refresh_cli().await)
}

async fn api_logs(
    State(state): State<WebState>,
    Query(query): Query<LogQuery>,
) -> Json<serde_json::Value> {
    Json(json!({
        "entries": state.app.logger.tail(query.after, query.limit.unwrap_or(250))
    }))
}

async fn api_jobs(State(state): State<WebState>) -> Json<Vec<crate::state::DownloadJob>> {
    Json(state.app.list_jobs())
}

async fn api_cancel_job(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::state::DownloadJob>> {
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::bad_request("任务 ID 不是有效 UUID"))?;
    let job = state.app.cancel_job(id).map_err(ApiError::bad_request)?;
    Ok(Json(job))
}

async fn api_open_data(State(state): State<WebState>) -> ApiResult<StatusCode> {
    open_path(&state.app.data_dir).map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_open_log(State(state): State<WebState>) -> ApiResult<StatusCode> {
    let log_path = state.app.logger.file_path();
    if !log_path.exists() {
        return Err(ApiError::bad_request("日志文件尚未生成"));
    }
    open_path(log_path).map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn request_gate(State(state): State<WebState>, request: Request, next: Next) -> Response {
    // Streamable HTTP GET requests may be long-lived SSE connections. They
    // must not consume the short-request pool or a client could starve every
    // POST and the local panel simply by keeping SSE connections open.
    let _request_permit = if request.method() == axum::http::Method::GET {
        None
    } else {
        match Arc::clone(&state.request_slots).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return error_response(StatusCode::SERVICE_UNAVAILABLE, "并发请求过多，请稍后重试");
            }
        }
    };
    let path = request.uri().path();
    let protected_mcp = path == "/mcp" || path.starts_with("/mcp/");
    let protected_admin = path.starts_with("/api/");

    if protected_admin {
        let peer = request.extensions().get::<ConnectInfo<SocketAddr>>();
        if !peer.is_some_and(|ConnectInfo(address)| is_local_host_ip(address.ip())) {
            return error_response(StatusCode::FORBIDDEN, "管理 API 仅允许本机访问");
        }
    }

    if protected_mcp && !state.app.config().mcp_enabled {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "MCP 服务当前已在面板中关闭",
        );
    }

    if protected_admin || protected_mcp {
        let supplied = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_bearer);
        let authorized = if protected_admin {
            supplied.is_some_and(|value| constant_time_equal(value, state.app.admin_token()))
        } else {
            let expected = state.app.config().access_token;
            supplied.is_some_and(|value| constant_time_equal(value, &expected))
        };
        if !authorized {
            state.app.log_auth_rejection(format!(
                "拒绝未授权请求：{} {}",
                request.method(),
                request.uri().path()
            ));
            let mut response = error_response(StatusCode::UNAUTHORIZED, "Bearer Token 无效或缺失");
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"comfyui-mcp-bridge\""),
            );
            return response;
        }
    }

    next.run(request).await
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    left.len() == right.len() && bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

fn is_local_host_ip(address: IpAddr) -> bool {
    address.is_loopback()
        || list_afinet_netifas()
            .unwrap_or_default()
            .into_iter()
            .any(|(_, local)| local == address)
}

fn parse_bearer(value: &str) -> Option<&str> {
    let mut parts = value.split_ascii_whitespace();
    let scheme = parts.next()?;
    let credentials = parts.next()?;
    if parts.next().is_some() || !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    Some(credentials)
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

fn normalize_user_path(path: PathBuf) -> ApiResult<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(ApiError::bad_request("路径不能为空"));
    }
    Ok(std::fs::canonicalize(&path)
        .map(strip_windows_verbatim_prefix)
        .unwrap_or(path))
}

fn validate_http_url(value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        bail!("ComfyUI 地址不能为空");
    }
    let parsed = reqwest::Url::parse(value).context("ComfyUI 地址格式无效")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("ComfyUI 地址必须以 http:// 或 https:// 开头");
    }
    if parsed.host_str().is_none() {
        bail!("ComfyUI 地址必须包含主机名或 IP");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("ComfyUI 地址不能包含用户名或密码");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("ComfyUI 地址不能包含查询参数或片段");
    }
    if value.chars().any(char::is_control) {
        bail!("ComfyUI 地址不能包含控制字符");
    }
    Ok(())
}

fn allowed_hosts(config: &AppConfig) -> Vec<String> {
    let mut hosts = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
        config.bind_address.clone(),
    ];
    if let Some(name) = env::var_os("COMPUTERNAME") {
        hosts.push(name.to_string_lossy().into_owned());
    }
    for (_, address) in list_afinet_netifas().unwrap_or_default() {
        hosts.push(address.to_string());
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

fn allowed_origins(config: &AppConfig) -> Vec<String> {
    let mut origins = vec![
        format!("http://127.0.0.1:{}", config.port),
        format!("http://localhost:{}", config.port),
    ];
    for (_, address) in list_afinet_netifas().unwrap_or_default() {
        origins.push(http_base_url(address, config.port));
    }
    origins
}

fn lan_base_urls(bind_address: &str, port: u16) -> Vec<String> {
    let bind_ip = bind_address.parse::<IpAddr>().ok();
    if let Some(address) = bind_ip.filter(|address| !address.is_unspecified()) {
        return vec![http_base_url(address, port)];
    }

    let all_addresses: Vec<IpAddr> = list_afinet_netifas()
        .unwrap_or_default()
        .into_iter()
        .map(|(_, address)| address)
        .filter(|address| !address.is_loopback())
        .filter(|address| match bind_ip {
            Some(IpAddr::V4(_)) => address.is_ipv4(),
            Some(IpAddr::V6(_)) => address.is_ipv6(),
            None => true,
        })
        .collect();
    let mut addresses: Vec<IpAddr> = all_addresses
        .iter()
        .copied()
        .filter(|address| match address {
            IpAddr::V4(address) => address.is_private(),
            IpAddr::V6(address) => address.is_unique_local(),
        })
        .collect();
    if addresses.is_empty() {
        addresses.extend(all_addresses.into_iter().filter(|address| match address {
            IpAddr::V4(address) => address.is_link_local(),
            IpAddr::V6(_) => false,
        }));
    }
    addresses.sort_by_key(|address| (!address.is_ipv4(), address.to_string()));
    addresses.dedup();
    addresses
        .into_iter()
        .map(|address| http_base_url(address, port))
        .collect()
}

fn loopback_base_url(bind_address: &str, port: u16) -> String {
    let address = match bind_address.parse::<IpAddr>() {
        Ok(IpAddr::V6(address)) if address.is_unspecified() || address.is_loopback() => {
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
        Ok(address) if !address.is_unspecified() && !address.is_loopback() => address,
        _ => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    };
    http_base_url(address, port)
}

fn http_base_url(address: IpAddr, port: u16) -> String {
    match address {
        IpAddr::V4(address) => format!("http://{address}:{port}"),
        IpAddr::V6(address) => format!("http://[{address}]:{port}"),
    }
}

fn open_path(path: &std::path::Path) -> Result<()> {
    #[cfg(windows)]
    {
        Command::new("explorer.exe")
            .arg(path)
            .spawn()
            .with_context(|| format!("无法打开 {}", path.display()))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        webbrowser::open(path.to_string_lossy().as_ref())
            .map(|_| ())
            .map_err(|error| anyhow!(error.to_string()))
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(error: impl ToString) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
        }
    }

    fn internal(error: impl ToString) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_comparison_requires_equal_length_and_value() {
        assert!(constant_time_equal("abc", "abc"));
        assert!(!constant_time_equal("abc", "abd"));
        assert!(!constant_time_equal("abc", "abc0"));
        assert_eq!(parse_bearer("bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("BEARER abc"), Some("abc"));
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer("Bearer abc extra"), None);
    }

    #[test]
    fn panel_url_uses_loopback_and_token() {
        let url = panel_url("0.0.0.0", 8765, "admin-secret");
        assert!(url.starts_with("http://127.0.0.1:8765/?token="));
        assert!(url.ends_with("admin-secret"));
        assert_eq!(
            panel_url("::", 8765, "admin-secret"),
            "http://[::1]:8765/?token=admin-secret"
        );
    }

    #[test]
    fn advertised_urls_follow_the_actual_bind_address() {
        assert_eq!(
            lan_base_urls("127.0.0.1", 8765),
            vec!["http://127.0.0.1:8765"]
        );
        assert_eq!(
            lan_base_urls("192.0.2.10", 9000),
            vec!["http://192.0.2.10:9000"]
        );
    }
}
