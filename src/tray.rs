use std::sync::mpsc::Sender;

/// Actions initiated from the notification-area icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    OpenPanel,
    ToggleMcp,
    Rediscover,
    OpenLogs,
    Exit,
}

const ICON_SIZE: u32 = 32;

/// Builds a small blue "connected nodes" icon without any external assets.
fn icon_rgba() -> Vec<u8> {
    let mut rgba = vec![0; (ICON_SIZE * ICON_SIZE * 4) as usize];
    let center = (ICON_SIZE as f32 - 1.0) / 2.0;
    let nodes = [(9.0_f32, 9.0_f32), (22.0, 16.0), (9.0, 23.0)];

    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let pixel = ((y * ICON_SIZE + x) * 4) as usize;
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let dx = px - center;
            let dy = py - center;

            if dx * dx + dy * dy > 15.0 * 15.0 {
                continue;
            }

            rgba[pixel..pixel + 4].copy_from_slice(&[30, 112, 224, 255]);

            let on_link = distance_to_segment((px, py), nodes[0], nodes[1]) <= 1.25
                || distance_to_segment((px, py), nodes[1], nodes[2]) <= 1.25;
            let on_node = nodes.iter().any(|(nx, ny)| {
                let node_dx = px - nx;
                let node_dy = py - ny;
                node_dx * node_dx + node_dy * node_dy <= 2.75 * 2.75
            });

            if on_link || on_node {
                rgba[pixel..pixel + 4].copy_from_slice(&[245, 249, 255, 255]);
            }
        }
    }

    rgba
}

fn distance_to_segment(point: (f32, f32), start: (f32, f32), end: (f32, f32)) -> f32 {
    let segment = (end.0 - start.0, end.1 - start.1);
    let to_point = (point.0 - start.0, point.1 - start.1);
    let length_squared = segment.0 * segment.0 + segment.1 * segment.1;
    let projection =
        ((to_point.0 * segment.0 + to_point.1 * segment.1) / length_squared).clamp(0.0, 1.0);
    let closest = (
        start.0 + projection * segment.0,
        start.1 + projection * segment.1,
    );
    let dx = point.0 - closest.0;
    let dy = point.1 - closest.1;
    (dx * dx + dy * dy).sqrt()
}

#[cfg(windows)]
pub fn run_tray(event_tx: Sender<TrayEvent>, initial_enabled: bool) -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent as NativeTrayEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MSG, TranslateMessage,
    };

    let menu = Menu::new();
    let open_panel = MenuItem::new("打开面板", true, None);
    let toggle_mcp = MenuItem::new(toggle_label(initial_enabled), true, None);
    let rediscover = MenuItem::new("重新发现 ComfyUI", true, None);
    let open_logs = MenuItem::new("打开日志", true, None);
    let separator = PredefinedMenuItem::separator();
    let exit = MenuItem::new("退出", true, None);
    menu.append_items(&[
        &open_panel,
        &toggle_mcp,
        &rediscover,
        &open_logs,
        &separator,
        &exit,
    ])?;

    let icon = Icon::from_rgba(icon_rgba(), ICON_SIZE, ICON_SIZE)?;
    let _tray_icon = TrayIconBuilder::new()
        .with_tooltip("ComfyUI MCP Bridge")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_menu_on_right_click(true)
        .build()?;

    let mut enabled = initial_enabled;
    let mut last_open = None::<Instant>;
    let mut message = MSG::default();

    loop {
        // GetMessageW must run on the same thread that created the tray icon.
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if status.0 == -1 {
            return Err(anyhow::anyhow!(
                "Win32 GetMessageW failed: {}",
                windows::core::Error::from_thread()
            ));
        }
        if status.0 == 0 {
            return Ok(());
        }

        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let outgoing = if event.id() == open_panel.id() {
                Some(TrayEvent::OpenPanel)
            } else if event.id() == toggle_mcp.id() {
                enabled = !enabled;
                toggle_mcp.set_text(toggle_label(enabled));
                Some(TrayEvent::ToggleMcp)
            } else if event.id() == rediscover.id() {
                Some(TrayEvent::Rediscover)
            } else if event.id() == open_logs.id() {
                Some(TrayEvent::OpenLogs)
            } else if event.id() == exit.id() {
                Some(TrayEvent::Exit)
            } else {
                None
            };

            if let Some(outgoing) = outgoing
                && (event_tx.send(outgoing).is_err() || outgoing == TrayEvent::Exit)
            {
                return Ok(());
            }
        }

        while let Ok(event) = NativeTrayEvent::receiver().try_recv() {
            let opens_panel = matches!(
                event,
                NativeTrayEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | NativeTrayEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            );

            if opens_panel {
                let now = Instant::now();
                let duplicate = last_open.is_some_and(|previous| {
                    now.duration_since(previous) < Duration::from_millis(600)
                });
                if !duplicate {
                    last_open = Some(now);
                    if event_tx.send(TrayEvent::OpenPanel).is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn toggle_label(enabled: bool) -> &'static str {
    if enabled { "停用 MCP" } else { "启用 MCP" }
}

#[cfg(not(windows))]
pub fn run_tray(_event_tx: Sender<TrayEvent>, _initial_enabled: bool) -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "the system tray is only available on Windows"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_icon_is_valid_rgba() {
        let rgba = icon_rgba();
        assert_eq!(rgba.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        assert!(rgba.chunks_exact(4).any(|pixel| pixel[3] == 0));
        assert!(rgba.chunks_exact(4).any(|pixel| pixel[3] == 255));
        assert!(
            rgba.chunks_exact(4)
                .all(|pixel| pixel[3] == 0 || pixel[3] == 255)
        );
    }
}
