# Codex State Kit

面向 Codex 的本地桌面助手，通过管理连接、出站代理与 Turn-State，辅助排查和缓解负载、响应质量波动问题。

基于 Tauri、Rust 和 React。实际效果受账号、网络和上游服务影响，不保证消除过载或提升模型能力。

## 主要功能

- **自动接入**：启动后配置 Codex 本地路由，正常退出时恢复。
- **ChatGPT 登录**：默认使用浏览器回调，也支持授权码登录。
- **出站代理**：默认内置 WARP，无需安装额外客户端；支持手动代理。
- **Turn-State 管理**：后台获取、缓存并复用，展示状态与最近请求。
- **目录切换**：修改 Codex 工作目录后自动保存、恢复旧路由并加载新目录。

## 快速开始

1. 安装并启动 Codex State Kit。
2. 确认「Codex 工作目录」指向客户端使用的配置目录，通常为用户目录下的 `.codex`。
3. 尚未登录时，点击「登录 ChatGPT」，在浏览器完成授权。
4. 等待内置 WARP 就绪，或切换到「手动代理」填写代理地址。
5. 确认界面显示「已接入」，重新启动 Codex 客户端以加载路由。

使用期间保持应用运行；正常退出后，重新启动 Codex 客户端以使用恢复后的路由。

## 网络路径

```text
Codex 客户端 ── 本机代理 ── Codex 上游
Turn-State 获取 ── 内置 WARP / 手动代理 ── Codex 上游
```

界面中的出站代理用于获取 Turn-State，业务转发不会主动套用该配置。正式版本机代理默认监听 `127.0.0.1:8787`。

## 文档导航

| 文档 | 内容 |
| --- | --- |
| [配置与自动接入](docs/config.md) | 工作目录、文件位置、路由恢复 |
| [ChatGPT 登录](docs/browser-login.md) | 回调与授权码登录 |
| [出站代理与 WARP](docs/warp.md) | 自动连接、手动代理、运行限制 |
| [常见问题](docs/troubleshooting.md) | 登录、网络、接入与恢复排查 |
| [Turn-State 机制](docs/turn-state-notes.md) | 当前缓存、替换规则和效果边界 |
| [维护与验收](docs/turn-state-sop.md) | 验证步骤和问题报告 |
| [开发与打包](docs/development.md) | 环境、命令、版本和安装包 |
| [界面与图标](docs/ui-theme.md) | 布局、配色、图标维护 |

## 本地开发

准备 Node.js、项目指定的 pnpm、Rust stable 和 [Tauri 开发依赖](https://v2.tauri.app/start/prerequisites/)，在仓库根目录运行：

```sh
corepack pnpm install --frozen-lockfile
corepack pnpm dev
```

构建安装包使用 `corepack pnpm build`。完整说明见[开发与打包](docs/development.md)。

## 第三方组件

内置 WARP 使用第三方开源内核 usque，来源和许可证见[内核来源记录](src-tauri/resources/warp/PROVENANCE.md)。上游许可证与依赖声明保留原文。
