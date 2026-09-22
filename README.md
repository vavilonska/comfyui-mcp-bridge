# ComfyUI MCP Bridge

一个 Windows 便携式单文件应用，把本机的 ComfyUI 模型目录和 Civitai CLI
通过 MCP Streamable HTTP 暴露给可信局域网内的 agent。

## 主要能力

- 自动发现 ComfyUI：安装路径支持手动配置、环境变量、运行进程、常用目录和有限深度盘符扫描；HTTP 地址优先解析运行进程的 `--listen/--port`，并验证本机 `8188`、`8189`、`8190`。
- 标准 MCP Streamable HTTP：支持 MCP `2024-11-05` 至 `2026-07-28`
  的协议协商。
- 内置官方 Civitai Windows CLI；系统里有 CLI 时优先使用系统版本。
- 自动模型路由：未指定目录时调用 `--layout comfyui --root <ComfyUI>`。
- 显式模型目录：agent 可以传本机绝对路径，但默认只能写入 ComfyUI 根目录
  或面板批准的根目录。
- 后台下载任务：单任务队列、状态查询、取消、输出和退出码。
- 本地管理面板：MCP 开关、地址、Token、ComfyUI/CLI 路径、下载任务和日志。
- Windows 系统托盘：打开面板、开关 MCP、重新发现、打开日志和退出。
- 单个自包含 EXE：不需要安装 Python、Node、.NET 或 Rust。

## 下载

从 [GitHub Releases](https://github.com/vavilonska/comfyui-mcp-bridge/releases) 下载 Windows x64 便携包并解压。发布包不包含任何本机配置、Token 或模型文件。

## 直接使用

1. 双击 `ComfyUI-MCP-Bridge.exe`。
2. 程序会在 EXE 同目录创建 `data`，启动托盘并打开面板。
3. Windows 防火墙首次询问时，只对“专用网络”放行。
4. 在面板检查 ComfyUI 路径和已验证的 HTTP 地址；自动识别错误时可手动填写并保存。
5. 点击“复制 Agent 配置”，粘贴到支持 Streamable HTTP 的 MCP 客户端。

默认监听 `0.0.0.0:8765`，MCP endpoint 是：

```text
http://<本机局域网 IP>:8765/mcp
```

每次请求必须带：

```http
Authorization: Bearer <面板中显示的 Token>
```

通用客户端配置示例：

```json
{
  "mcpServers": {
    "comfyui": {
      "type": "streamable-http",
      "url": "http://192.168.1.10:8765/mcp",
      "headers": {
        "Authorization": "Bearer 替换成面板中的Token"
      }
    }
  }
}
```

字段名会因 agent 客户端而异；面板复制的是上面的通用格式。

## Civitai 登录

模型搜索和读取公开信息不要求登录；多数模型文件下载需要 Civitai Token。
应用不会向远程 agent 暴露或记录这个 Token。

内置 CLI 首次运行后位于：

```powershell
.\data\bin\civitai-0.1.102.exe login
```

也可以在启动应用前设置 `CIVITAI_TOKEN`。如果面板手动指定了 CLI 路径，
该路径优先；否则依次使用 PATH 中的 CLI 和内置 CLI。

## MCP 工具

| 工具 | 用途 |
|---|---|
| `server_status` | 查看 ComfyUI、CLI 和活动下载状态，不返回密钥 |
| `comfyui_discover` | 重新扫描并返回排序后的 ComfyUI 候选 |
| `civitai_search_models` | 搜索 Civitai 公共模型目录 |
| `civitai_get_model` | 获取模型及其版本列表 |
| `civitai_get_version` | 获取精确版本、文件、大小和哈希 |
| `civitai_plan_download` | dry-run，返回目标、大小、SHA256 和认证要求 |
| `civitai_download` | 启动异步下载并立即返回 job UUID |
| `civitai_download_status` | 查询一个下载任务 |
| `civitai_downloads_list` | 列出最近任务 |
| `civitai_download_cancel` | 取消活动任务 |

下载输入示例：

```json
{
  "version_id": 691639,
  "model_id": null,
  "file": null,
  "all": false,
  "target_dir": null,
  "force": false,
  "dry_run": false,
  "for_base": null
}
```

`model_id` 和 `version_id` 必须二选一。`target_dir` 是运行本程序的 Windows
主机路径，不是调用 agent 所在机器的路径。省略时由 Civitai CLI 自动按
ComfyUI 模型类型放入 `models` 子目录。

## 安全默认值

- 管理 API 只接受本机连接，并使用每次启动重新生成的独立面板密钥；
  局域网 MCP Token 无法调用管理 API。
- MCP Bearer Token 使用 256 位随机数，可在面板重新生成。
- MCP SDK 校验 `Host` 和浏览器 `Origin`，防止 DNS rebinding。
- 显式下载目录默认受 ComfyUI 根目录和批准根目录白名单限制。
- 禁止 UNC、设备路径、盘符根目录和 Windows 系统目录。
- 下载槽位以原子方式限制为一个，所有 Civitai CLI 子进程总并发不超过四个。
- HTTP 明文只适合可信局域网。跨网络请使用 Tailscale、WireGuard 或带 TLS
  的反向代理；反向代理只应暴露 `/mcp`，不要暴露 `/api` 或管理面板。

## 便携数据

```text
ComfyUI-MCP-Bridge.exe
data/
  config.json
  bin/civitai-0.1.102.exe
  logs/app.log
```

设置环境变量 `COMFYUI_MCP_DATA_DIR` 可以覆盖数据目录，主要用于测试。

## 从源码构建

需要 Rust 1.88+ 和 Windows GNU 或 MSVC 工具链：

```powershell
.\build-portable.ps1
```

脚本先执行全部测试，再使用静态 CRT 和 LTO 构建：

```text
dist\ComfyUI-MCP-Bridge.exe
```

手动验证：

```powershell
cargo fmt --all -- --check
cargo test --all-targets
cargo check --all-targets
```

## 许可

本项目使用 MIT License。内置 Civitai CLI 使用 Apache License 2.0；详见
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) 和
[`vendor/civitai/LICENSE`](vendor/civitai/LICENSE)。完整第三方许可也嵌入面板的
`/licenses/civitai` 页面。
