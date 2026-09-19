import Minus from "lucide-react/dist/esm/icons/minus.js";
import Square from "lucide-react/dist/esm/icons/square.js";
import X from "lucide-react/dist/esm/icons/x.js";
import Github from "lucide-react/dist/esm/icons/github.js";
import ExternalLink from "lucide-react/dist/esm/icons/external-link.js";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useState, type PropsWithChildren } from "react";
import { GITHUB_REPO_URL, isTauri, openGithubRepo } from "@/lib/api";
import { version } from "../../../package.json";
import { Logo } from "./Logo";
import { useUpdateCheck } from "@/hooks/useUpdateCheck";

async function windowAction(action: "minimize" | "maximize" | "close") {
  if (!isTauri) return;
  const window = getCurrentWindow();
  if (action === "minimize") await window.minimize();
  if (action === "maximize") await window.toggleMaximize();
  if (action === "close") await window.close();
}

export function AppShell({ children }: PropsWithChildren) {
  const [repoError, setRepoError] = useState<string | null>(null);
  const updates = useUpdateCheck();
  const versionLabel = import.meta.env.DEV || !isTauri ? "dev" : `v${version}`;
  return (
    <div className="app-shell">
      <header className="titlebar" data-tauri-drag-region>
        <div className="titlebar__identity" data-tauri-drag-region>
          <Logo />
          <span className="app-version" data-tauri-drag-region>{versionLabel}</span>
        </div>
        <div className="titlebar__actions">
          <button className="update-check" type="button" disabled={updates.checking} onClick={() => void updates.check(true)}>
            {updates.phase === "installing" ? "安装中…" : updates.phase === "downloading" ? "下载中…" : updates.checking && updates.phase !== "ready" ? "检查中…" : "检查更新"}
          </button>
          <a className="repo-link" href={GITHUB_REPO_URL} target="_blank" rel="noopener noreferrer"
            aria-label="在浏览器打开 GitHub 仓库 DouDOU-start/codex-state-kit"
            title="DouDOU-start/codex-state-kit"
            onClick={(event) => {
              if (!isTauri) return;
              event.preventDefault();
              setRepoError(null);
              void openGithubRepo().catch(() => setRepoError("无法打开浏览器，请访问 github.com/DouDOU-start/codex-state-kit"));
            }}>
            <Github size={15} aria-hidden="true" /><span>GitHub</span><ExternalLink size={11} aria-hidden="true" />
          </a>
        <div className="window-controls">
          <button type="button" aria-label="最小化" onClick={() => void windowAction("minimize")}>
            <Minus size={17} />
          </button>
          <button type="button" aria-label="最大化" onClick={() => void windowAction("maximize")}>
            <Square size={13} />
          </button>
          <button className="window-controls__close" type="button" aria-label="关闭" disabled={updates.phase === "installing"} onClick={() => void windowAction("close")}>
            <X size={17} />
          </button>
        </div>
        </div>
      </header>
      <main className="app-content">
        {updates.update ? <div className="update-notice" role="status">
          <span>{updates.phase === "ready" ? <>v{updates.update.latestVersion} 已下载并通过签名校验。安装将关闭应用；请先结束当前会话。</> : updates.phase === "installing" ? "正在准备安装：恢复路由、停止 WARP，并等待在途请求结束，请勿关闭应用…" : updates.phase === "downloading" ? `正在下载 v${updates.update.latestVersion} ${updates.progress === null ? "" : `${updates.progress}%`}，下载期间可继续使用。` : <>发现新版本 <strong>v{updates.update.latestVersion}</strong>（当前 v{updates.update.currentVersion}）。</>}</span>
          <div className="update-notice__actions">
            {updates.phase === "idle" ? <button type="button" onClick={() => void updates.download()}>下载更新</button> : null}
            {updates.phase === "ready" ? <button type="button" onClick={() => void updates.install()}>确认安装并重启</button> : null}
            <button type="button" onClick={() => void updates.open(updates.update?.tag)}>前往下载 <ExternalLink size={12} /></button>
            {updates.phase === "idle" ? <button type="button" onClick={updates.dismiss}>稍后</button> : null}
          </div>
        </div> : null}
        {updates.message ? <div className="update-notice" role="status">
          <span>{updates.message}</span>
          <div className="update-notice__actions">
            <button type="button" onClick={() => void updates.open()}>发布页面</button>
            <button type="button" onClick={updates.dismiss}>关闭</button>
          </div>
        </div> : null}
        {repoError ? <div className="banner banner--error repo-error" role="alert"><span>{repoError}</span><button type="button" onClick={() => setRepoError(null)}>关闭</button></div> : null}
        <div style={{ display: "contents" }} ref={(node) => { if (node) node.inert = updates.phase === "installing"; }}>
          {children}
        </div>
      </main>
    </div>
  );
}
