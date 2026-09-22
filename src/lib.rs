pub mod bundled;
pub mod civitai;
pub mod comfyui_url;
pub mod config;
pub mod discovery;
pub mod logging;
pub mod mcp;
pub mod path_utils;
pub mod state;
pub mod tray;
pub mod web;

pub const APP_NAME: &str = "ComfyUI MCP Bridge";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
