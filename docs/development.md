# 开发与打包

[返回首页](../README.md)

## 环境

发布流程会按目标平台下载对应的 WARP 内核：Windows x64、macOS Intel 和 macOS Apple Silicon 分别使用对应二进制文件，不跨平台复用。

准备 Node.js、Rust stable、C++ 构建工具和 WebView2 等 [Tauri 开发依赖](https://v2.tauri.app/start/prerequisites/)。pnpm 版本以根目录 `package.json` 的 `packageManager` 为准；以下命令使用 Corepack 调用。

```sh
git clone git@github.com:DouDOU-start/codex-state-kit.git
cd codex-state-kit
corepack pnpm install --frozen-lockfile
corepack pnpm dev
```

若环境没有 Corepack，先准备 Corepack 或安装与项目声明一致的 pnpm。已安装对应 pnpm 时，可将命令中的 `corepack pnpm` 替换为 `pnpm`。

## 常用命令

均在仓库根目录执行：

| 命令 | 用途 |
| --- | --- |
| `corepack pnpm dev` | 启动桌面开发应用与前端服务 |
| `corepack pnpm dev:renderer` | 仅启动浏览器预览，默认端口 1420 |
| `corepack pnpm typecheck` | TypeScript 类型检查 |
| `corepack pnpm build:renderer` | 类型检查并构建前端到 `dist/` |
| `cargo check --workspace` | 检查 Rust 工作区编译 |
| `cargo test -p codex-state-kit login` | 登录及相关接入测试 |
| `corepack pnpm icons` | 从统一 SVG 生成应用图标 |
| `corepack pnpm build` | 在当前系统构建桌面程序及安装包 |

纯浏览器预览使用模拟接口，不会修改真实 Codex 配置或建立 WARP 隧道。桌面开发版会执行真实操作，建议使用专门的 Codex 配置目录。

当前部分 Token 缓存测试会读写用户目录，且测试之间存在共享文件影响。完整测试应在隔离、可丢弃的用户环境执行；不要直接在日常账号目录运行全量测试。登录测试使用临时目录和本机模拟服务。

## GitHub Actions 发布

推送形如 `v0.0.1` 的标签会触发 `.github/workflows/release.yml`，并行构建 Windows x64、macOS Intel 和 macOS Apple Silicon，随后将 `.exe`、`.msi` 和 `.dmg` 上传到同一个 GitHub Release。也可以在 Actions 页面手动运行工作流。

发布说明来自**附注标签**的正文，不要用轻量标签。PowerShell 示例：

```powershell
git tag -a v0.0.4 -m @"
本次更新：
- 说明一
- 说明二

Mac 版本当前为未公证构建；首次打开时请按系统提示允许应用运行。
"@
git push origin v0.0.4
```

在 Actions 里手动跑工作流时，也可以在 `notes` 输入框填写说明。已经生成的 Release 仍可在 GitHub 上点 Edit 改说明。未写附注时才回落到默认文案。

当前工作流生成未签名、未公证的 Mac 包。正式分发前，在仓库 Secrets 配置 Apple Developer 证书和公证凭据，并在工作流中接入 Tauri 的签名环境变量；否则 macOS 可能显示安全提示。

## 版本与产物

首个版本为 `0.0.1`。发布时修改根目录 `package.json` 的 `version`，不带 `v` 前缀；标题栏与 Tauri 安装包读取同一值。同步维护两个 `Cargo.toml` 中的包版本，并更新 `Cargo.lock`。

构建成功后查看：

```text
target/release/codex-state-kit-desktop.exe
target/release/bundle/nsis/
target/release/bundle/msi/
```

构建前自动重新生成图标与前端资源。优先分发安装包；单独分发主程序时必须附带 `warp/` 资源目录。WARP 内核更新步骤见[来源记录](../src-tauri/resources/warp/PROVENANCE.md)。

## 代码结构

| 路径 | 职责 |
| --- | --- |
| `frontend/src/` | React 界面、样式与模拟接口 |
| `src-tauri/src/` | 桌面生命周期、IPC 命令与登录会话 |
| `src/proxy.rs` | HTTP/WebSocket 转发、获取调度与自动接入管理 |
| `src/attach.rs` | Codex 配置修改、备份与恢复 |
| `src/login.rs`、`src/browser_login.rs` | 授权码登录、回调登录与凭据保存 |
| `src/fetch.rs`、`src/turn_state.rs` | Turn-State 获取、缓存与替换 |
| `src/warp.rs` | 内置内核生命周期与健康检查 |
| `tools/` | 内核下载和依赖声明维护 |

发布前执行适用检查，再按[维护与验收](turn-state-sop.md)验证桌面行为。协议、默认值或文件路径变化时同步更新文档；未接入运行流程的辅助函数不作为已支持功能。
