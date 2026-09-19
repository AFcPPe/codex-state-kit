# Codex State Kit

面向 Codex 的本地桌面助手，通过管理连接、出站代理与 Turn-State，辅助排查和缓解负载、响应质量波动问题。

基于 Tauri、Rust 和 React。实际效果受账号、网络和上游服务影响，不保证消除过载或提升模型能力。

## 使用说明

启动 Kit，用 ChatGPT 登录完成接入，再为 Turn-State（界面中的 Token）配置出站代理。路由只在 Kit 运行期间生效；正常退出后会还原本地原有配置。

![Codex State Kit 主界面（代理地址、账号与工作目录已打码）](docs/images/kit-overview.png)

1. 安装并启动 Codex State Kit，确认「Codex 工作目录」与 Codex 客户端使用的配置目录一致，一般为用户目录下的 `.codex`。
2. 尚未登录时，在「Codex 接入」中用浏览器回调或授权码完成 ChatGPT 登录。
3. 在「出站代理」中选择出口。内置 WARP 可直接试用；若要稳定获取完整能力的 Token，建议改用手动代理，见下方说明。
4. 界面显示「已接入」且 Token 可用后，重启 Codex 客户端以加载本机路由。
5. 使用期间保持 Kit 运行。正常退出后，再重启 Codex，即可回到退出前的官方账号与本地配置。

### 出站代理与 Token

出站代理只用于获取 Turn-State，不会套用到 Codex 的业务转发。Token 是否完整，直接影响请求能否按未降级能力执行。

内置 WARP 出口相对固定。就目前观察，该模式多数情况下只能拿到降级票据，较难稳定获得完整能力的 Token。若目标是不降智票据，请切换到「手动代理」，并使用会轮换出口的动态代理。

出口不要锁在单一国家。将节点限制在美国时，完整票据的获取成功率通常更低；不限制地区、由服务商随机分配出口，整体更稳妥。若需要指定地区，瑞士、瑞典、英国等欧洲节点相对更容易打出完整票据。以上为使用经验，受账号、上游策略和代理质量影响，不保证结果。

手动代理填写完整 URL，失焦后自动保存，例如 `socks5h://user:password@proxy.example.com:1080`。详细格式见[出站代理与 WARP](docs/warp.md)。

## 主要功能

- **自动接入**：启动后配置 Codex 本地路由，正常退出时恢复。
- **ChatGPT 登录**：默认使用浏览器回调，也支持授权码登录。
- **出站代理**：默认内置 WARP，无需安装额外客户端；支持手动代理。
- **Turn-State 管理**：后台获取、缓存并复用，展示状态与最近请求。
- **目录切换**：修改 Codex 工作目录后自动保存、恢复旧路由并加载新目录。

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

发布版本时推送 `v` 开头的标签，例如 `v0.0.1`，GitHub Actions 会自动构建并发布 Windows 与 macOS 安装包。

## 第三方组件

内置 WARP 使用第三方开源内核 usque，来源和许可证见[内核来源记录](src-tauri/resources/warp/PROVENANCE.md)。上游许可证与依赖声明保留原文。
