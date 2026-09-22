#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{
    process::Command,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use comfyui_mcp_bridge::{
    APP_NAME,
    config::default_data_dir,
    state::AppState,
    tray::{TrayEvent, run_tray},
    web::{self, panel_url},
};

fn main() {
    if let Err(error) = run() {
        show_fatal_error(&format!("{error:#}"));
    }
}

fn run() -> Result<()> {
    let data_dir = default_data_dir()?;
    let state = AppState::load(data_dir)?;
    state.logger.info(format!(
        "{APP_NAME} {} 启动，数据目录：{}",
        comfyui_mcp_bridge::APP_VERSION,
        state.data_dir.display()
    ));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("comfyui-mcp")
        .build()
        .context("无法创建异步运行时")?;

    let cli_state = Arc::clone(&state);
    runtime.spawn(async move {
        cli_state.refresh_cli().await;
    });
    let url_state = Arc::clone(&state);
    runtime.spawn(async move {
        url_state.refresh_comfyui_url().await;
    });

    let server_state = Arc::clone(&state);
    let server_task = runtime.spawn(async move { web::serve(server_state).await });
    thread::sleep(Duration::from_millis(350));
    if server_task.is_finished() {
        return runtime
            .block_on(server_task)
            .map_err(|error| anyhow!("HTTP 服务任务异常：{error}"))?;
    }

    let config = state.config();
    if config.auto_open_panel {
        open_panel(&state);
    }

    let (event_tx, event_rx) = mpsc::channel();
    let event_state = Arc::clone(&state);
    let event_runtime = runtime.handle().clone();
    let event_thread = thread::Builder::new()
        .name("comfyui-mcp-tray-events".to_owned())
        .spawn(move || {
            while let Ok(event) = event_rx.recv() {
                match event {
                    TrayEvent::OpenPanel => open_panel(&event_state),
                    TrayEvent::ToggleMcp => {
                        let next = !event_state.config().mcp_enabled;
                        if let Err(error) = event_state.set_mcp_enabled(next) {
                            event_state
                                .logger
                                .error(format!("切换 MCP 服务失败：{error:#}"));
                        }
                    }
                    TrayEvent::Rediscover => {
                        event_state.refresh_discovery();
                        let url_state = Arc::clone(&event_state);
                        event_runtime.spawn(async move {
                            url_state.refresh_comfyui_url().await;
                        });
                    }
                    TrayEvent::OpenLogs => open_logs(&event_state),
                    TrayEvent::Exit => {
                        event_state.logger.info("收到托盘退出命令");
                        event_state.shutdown.cancel();
                        break;
                    }
                }
            }
        })?;

    let tray_result = run_tray(event_tx, config.mcp_enabled);
    state.shutdown.cancel();
    let _ = event_thread.join();
    let server_result = runtime
        .block_on(server_task)
        .map_err(|error| anyhow!("HTTP 服务任务异常：{error}"))?;
    server_result?;
    tray_result
}

fn open_panel(state: &AppState) {
    let config = state.config();
    let _ = webbrowser::open(&panel_url(
        &config.bind_address,
        config.port,
        state.admin_token(),
    ));
}

fn open_logs(state: &AppState) {
    let path = state.logger.file_path();
    #[cfg(windows)]
    {
        let _ = Command::new("explorer.exe").arg(path).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = webbrowser::open(path.to_string_lossy().as_ref());
    }
}

#[cfg(windows)]
fn show_fatal_error(message: &str) {
    use windows::{
        Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
        core::PCWSTR,
    };

    let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = APP_NAME.encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show_fatal_error(message: &str) {
    eprintln!("{APP_NAME}: {message}");
}
