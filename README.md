# ComfyUI MCP Bridge

**A portable Windows MCP server for discovering ComfyUI and searching, downloading, and managing Civitai models.**

通过 MCP 让 Agent 发现 ComfyUI、搜索 Civitai，并管理模型下载。

[快速安装](#下载与使用) · [English setup](#download-and-use) · [Download / 下载](https://github.com/vavilonska/comfyui-mcp-bridge/releases/latest) · [MIT](LICENSE)

![ComfyUI MCP Bridge — conceptual workflow / 功能流程示意](docs/assets/overview.svg)

> Trusted-network model management; not a workflow executor. / 适用于可信网络，不执行 ComfyUI 工作流。

[中文](#中文) · [English](#english)

## 中文

Windows 便携单文件工具，通过 MCP Streamable HTTP 为可信局域网中的 Agent 提供本机 ComfyUI 发现、Civitai 模型搜索与下载管理。无需安装 Python、Node 或 Rust；它不是工作流执行器。

### 下载与使用

1. 从 [Releases](https://github.com/vavilonska/comfyui-mcp-bridge/releases) 下载 Windows x64 ZIP，解压运行 `ComfyUI-MCP-Bridge.exe`。
2. 程序在同目录创建 `data`，显示托盘和本机管理面板。检查 ComfyUI 路径及 HTTP 地址。
3. 在面板复制 Agent 配置，并连接 `/mcp`。默认监听 `0.0.0.0:8765`，请求必须携带面板中的 Bearer Token。客户端配置字段可能不同。

```json
{
  "mcpServers": {
    "comfyui": {
      "type": "streamable-http",
      "url": "http://127.0.0.1:8765/mcp",
      "headers": { "Authorization": "Bearer YOUR_MCP_TOKEN" }
    }
  }
}
```

远程 Agent 将 `127.0.0.1` 替换为本机局域网地址。HTTP 明文仅适合可信网络；跨网络连接应自行配置加密隧道或 TLS。防火墙只对所需专用网络放行。

### 功能

- 自动发现 ComfyUI 安装路径和服务地址，也可手动指定；验证常见本机端口。
- 内置 Civitai CLI 0.1.102，优先使用显式配置或 PATH 中的 CLI。
- 单任务后台下载队列、状态查询、取消和退出码；默认按 ComfyUI 模型类型分目录。
- 托盘和面板可控制 MCP 开关、Token、路径、任务和日志。
- 管理接口只接受本机连接并使用独立密钥；下载目录限制在 ComfyUI 或明确批准的根目录。拒绝 UNC、设备路径、系统目录及盘符根目录。

搜索公开信息无需登录；多数下载需要 Civitai Token，可自行运行 `.\data\bin\civitai-0.1.102.exe login`，或在启动前设置 `CIVITAI_TOKEN`。Civitai 凭据不会作为工具结果返回。

### MCP 工具 / MCP tools

| 工具 / Tool | 用途 / Purpose |
|---|---|
| `server_status` | 状态 / Status |
| `comfyui_discover` | 重新发现 / Rediscover ComfyUI |
| `civitai_search_models` | 搜索模型 / Search models |
| `civitai_get_model` | 模型详情 / Model details |
| `civitai_get_version` | 版本、文件、哈希 / Version, files and hashes |
| `civitai_plan_download` | 下载预览 / Download plan |
| `civitai_download` | 启动下载 / Start download |
| `civitai_download_status` | 任务状态 / Job status |
| `civitai_downloads_list` | 最近任务 / Recent jobs |
| `civitai_download_cancel` | 取消任务 / Cancel job |

下载输入必须在 `model_id` 和 `version_id` 中二选一；`target_dir` 是运行 Bridge 的 Windows 主机路径，不是 Agent 机器路径。支持的 MCP 协议协商范围为 `2024-11-05` 至 `2026-07-28`。

### 构建与许可

使用 Windows 稳定版 Rust 和对应 C/C++ 链接工具，运行 `cargo test --locked` 或 `.\build-portable.ps1`。构建依赖随源码保存的 `vendor/civitai/civitai.exe`；输出为 `dist/ComfyUI-MCP-Bridge.exe`。发布时 26 项 Rust 测试和便携构建通过，未重新完成所有客户端的端到端验收。程序未做 Authenticode 签名。

代码 [MIT](LICENSE)，内置 Civitai CLI 为 Apache-2.0，见 [第三方声明](THIRD_PARTY_NOTICES.md) 及 `vendor/civitai/LICENSE`。本项目非 ComfyUI/Civitai 官方产品。

## English

A portable, single-file Windows tool exposing local ComfyUI discovery, Civitai model search and download management to trusted LAN agents over MCP Streamable HTTP. No Python, Node or Rust installation is needed to run it. It is not a workflow execution server.

### Download and use

1. Download the Windows x64 ZIP from [Releases](https://github.com/vavilonska/comfyui-mcp-bridge/releases), extract it and run `ComfyUI-MCP-Bridge.exe`.
2. The application creates `data` beside the executable and opens its tray and local management panel. Verify the ComfyUI path and HTTP address.
3. Copy the Agent configuration from the panel and connect to `/mcp`. The default listener is `0.0.0.0:8765`; requests require the panel's Bearer Token. Client configuration formats may differ; a generic example is shown above.

For a remote agent, replace `127.0.0.1` with the Windows host's LAN address. Plain HTTP is for trusted networks only; use an encrypted tunnel or TLS across networks. Allow firewall access only on the required private network.

### Features

- Automatic ComfyUI installation/service discovery with manual overrides and common local-port checks.
- Bundled Civitai CLI 0.1.102; an explicit CLI path or a PATH installation takes precedence.
- One active background download, status/cancellation/exit codes and automatic ComfyUI model-folder routing.
- Tray and panel controls for MCP, tokens, paths, jobs and logs.
- Local-only management API with a separate secret. Download paths are restricted to ComfyUI or explicitly approved roots; UNC/device paths, system directories and drive roots are rejected.

Public searches need no login; most downloads require a Civitai token. Run `.\data\bin\civitai-0.1.102.exe login` yourself, or set `CIVITAI_TOKEN` before launch. Civitai credentials are not returned in tool results.

The bilingual tool table above lists every MCP tool. Download requests require exactly one of `model_id` and `version_id`. `target_dir` refers to the Windows host running the bridge, not the agent machine. MCP version negotiation supports `2024-11-05` through `2026-07-28`.

### Build and license

Use stable Rust for Windows with the matching C/C++ linker toolchain. Run `cargo test --locked` or `.\build-portable.ps1`. Builds require the included `vendor/civitai/civitai.exe`; output is `dist/ComfyUI-MCP-Bridge.exe`. The release passed 26 Rust tests and a portable build; end-to-end behavior across all clients was not revalidated. Binaries are not Authenticode-signed.

Project code is [MIT](LICENSE); the bundled Civitai CLI is Apache-2.0. See [third-party notices](THIRD_PARTY_NOTICES.md) and `vendor/civitai/LICENSE`. This is not an official ComfyUI or Civitai product.

## Related projects / 相关项目

[Black Screen Tray](https://github.com/vavilonska/black-screen-tray) · [OMPmail](https://github.com/vavilonska/OMPmail) · [All projects / 全部项目](https://github.com/vavilonska#projects--项目)
