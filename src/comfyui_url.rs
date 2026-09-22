use std::{collections::HashSet, time::Duration};

use chrono::Utc;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sysinfo::System;

const COMMON_PORTS: [u16; 3] = [8188, 8189, 8190];
const MAX_PROCESS_CANDIDATES: usize = 16;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_millis(650);

/// The effective ComfyUI HTTP endpoint and how it was selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComfyUrlStatus {
    pub url: Option<String>,
    pub source: String,
    pub verified: bool,
    pub message: Option<String>,
    pub checked_at: String,
}

impl ComfyUrlStatus {
    pub fn pending(manual_url: Option<&str>) -> Self {
        let manual_url = manual_url.map(str::trim).filter(|value| !value.is_empty());
        Self {
            url: manual_url.map(normalize_url),
            source: if manual_url.is_some() {
                "manual".to_owned()
            } else {
                "pending".to_owned()
            },
            verified: false,
            message: Some("等待 HTTP 探测".to_owned()),
            checked_at: Utc::now().to_rfc3339(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlCandidate {
    url: String,
    source: &'static str,
}

/// Discover a live ComfyUI HTTP endpoint with bounded, concurrent local probes.
/// A non-empty manual URL always wins, even if its verification probe fails.
pub async fn discover_comfyui_url(manual_url: Option<&str>) -> ComfyUrlStatus {
    if let Some(url) = manual_url.map(str::trim).filter(|value| !value.is_empty()) {
        let url = normalize_url(url);
        return status_from_probe(url, "manual", true).await;
    }

    let process_candidates = running_process_candidates();
    if let Some(status) = probe_candidates(process_candidates).await {
        return status;
    }

    let common_candidates = COMMON_PORTS
        .into_iter()
        .map(|port| UrlCandidate {
            url: format!("http://127.0.0.1:{port}"),
            source: "common-port",
        })
        .collect();
    if let Some(status) = probe_candidates(common_candidates).await {
        return status;
    }

    ComfyUrlStatus {
        url: None,
        source: "not-found".to_owned(),
        verified: false,
        message: Some("未在运行进程或本机常见端口发现 ComfyUI".to_owned()),
        checked_at: Utc::now().to_rfc3339(),
    }
}

async fn probe_candidates(candidates: Vec<UrlCandidate>) -> Option<ComfyUrlStatus> {
    let probes = candidates.into_iter().map(|candidate| async move {
        let verified = probe_comfyui_url(&candidate.url).await;
        (candidate, verified)
    });
    futures::future::join_all(probes)
        .await
        .into_iter()
        .find_map(|(candidate, verified)| {
            verified.then(|| ComfyUrlStatus {
                url: Some(candidate.url),
                source: candidate.source.to_owned(),
                verified: true,
                message: Some("/system_stats 已验证为 ComfyUI".to_owned()),
                checked_at: Utc::now().to_rfc3339(),
            })
        })
}

async fn status_from_probe(url: String, source: &str, preserve: bool) -> ComfyUrlStatus {
    let verified = probe_comfyui_url(&url).await;
    ComfyUrlStatus {
        url: (verified || preserve).then_some(url),
        source: source.to_owned(),
        verified,
        message: Some(if verified {
            "/system_stats 已验证为 ComfyUI".to_owned()
        } else {
            "手动地址已保留，但当前无法验证为 ComfyUI".to_owned()
        }),
        checked_at: Utc::now().to_rfc3339(),
    }
}

async fn probe_comfyui_url(base_url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_millis(300))
        .timeout(PROBE_TIMEOUT)
        .build()
    else {
        return false;
    };
    let url = format!("{}/system_stats", base_url.trim_end_matches('/'));
    let Ok(response) = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
    else {
        return false;
    };
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return false;
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return false;
        };
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return false;
        }
        body.extend_from_slice(&chunk);
    }

    serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|object| object.contains_key("system") && object.contains_key("devices"))
}

fn running_process_candidates() -> Vec<UrlCandidate> {
    let system = System::new_all();
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    for process in system.processes().values() {
        let name = process.name().to_string_lossy().to_lowercase();
        let args: Vec<String> = process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        let command = args.join(" ").to_lowercase();
        let is_bridge = name.contains("comfyui-mcp") || command.contains("comfyui-mcp-bridge");
        let looks_like_comfyui =
            command.contains("main.py") || (name.contains("comfyui") && !name.contains("mcp"));
        if is_bridge || !looks_like_comfyui {
            continue;
        }

        let url = endpoint_from_process_args(&args);
        if seen.insert(url.clone()) {
            candidates.push(UrlCandidate {
                url,
                source: "running-process",
            });
        }
        if candidates.len() >= MAX_PROCESS_CANDIDATES {
            break;
        }
    }
    candidates
}

fn endpoint_from_process_args(args: &[String]) -> String {
    let mut listen = None;
    let mut port = None;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--listen" {
            listen = args
                .get(index + 1)
                .filter(|next| !next.starts_with('-'))
                .cloned()
                .or_else(|| Some("0.0.0.0".to_owned()));
        } else if let Some(value) = argument.strip_prefix("--listen=") {
            listen = Some(value.to_owned());
        } else if argument == "--port" {
            port = args
                .get(index + 1)
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|value| *value != 0);
        } else if let Some(value) = argument.strip_prefix("--port=") {
            port = value.parse::<u16>().ok().filter(|value| *value != 0);
        }
        index += 1;
    }

    let listen = listen.unwrap_or_else(|| "127.0.0.1".to_owned());
    let host = match listen.trim().trim_matches(['[', ']']) {
        "" | "0.0.0.0" | "::" | "*" => "127.0.0.1".to_owned(),
        host if host.contains(':') => format!("[{host}]"),
        host => host.to_owned(),
    };
    format!("http://{}:{}", host, port.unwrap_or(8188))
}

fn normalize_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn process_arguments_support_comfyui_listen_and_port_forms() {
        let args =
            ["python", "main.py", "--listen", "0.0.0.0", "--port", "8189"].map(str::to_owned);
        assert_eq!(endpoint_from_process_args(&args), "http://127.0.0.1:8189");

        let args = ["python", "main.py", "--listen=192.168.1.20", "--port=8190"].map(str::to_owned);
        assert_eq!(
            endpoint_from_process_args(&args),
            "http://192.168.1.20:8190"
        );

        let args = ["python", "main.py", "--listen", "--port", "8188"].map(str::to_owned);
        assert_eq!(endpoint_from_process_args(&args), "http://127.0.0.1:8188");
    }

    #[tokio::test]
    async fn system_stats_probe_requires_comfyui_response_shape() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let read = socket.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..read]).contains("GET /system_stats"));
            let body = r#"{"system":{"os":"test"},"devices":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        assert!(probe_comfyui_url(&format!("http://{address}")).await);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn manual_url_is_preserved_when_offline() {
        let status = discover_comfyui_url(Some("http://127.0.0.1:1/")).await;
        assert_eq!(status.url.as_deref(), Some("http://127.0.0.1:1"));
        assert_eq!(status.source, "manual");
        assert!(!status.verified);
    }
}
