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

async function windowAction(action: "minimize" | "maximize" | "close") {
  if (!isTauri) return;
  const window = getCurrentWindow();
  if (action === "minimize") await window.minimize();
  if (action === "maximize") await window.toggleMaximize();
  if (action === "close") await window.close();
}

export function AppShell({ children }: PropsWithChildren) {
  const [repoError, setRepoError] = useState<string | null>(null);
  const versionLabel = import.meta.env.DEV || !isTauri ? "dev" : `v${version}`;
  return (
    <div className="app-shell">
      <header className="titlebar" data-tauri-drag-region>
        <div className="titlebar__identity" data-tauri-drag-region>
          <Logo />
          <span className="app-version" data-tauri-drag-region>{versionLabel}</span>
        </div>
        <div className="titlebar__actions">
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
          <button className="window-controls__close" type="button" aria-label="关闭" onClick={() => void windowAction("close")}>
            <X size={17} />
          </button>
        </div>
        </div>
      </header>
      <main className="app-content">
        {repoError ? <div className="banner banner--error repo-error" role="alert"><span>{repoError}</span><button type="button" onClick={() => setRepoError(null)}>关闭</button></div> : null}
        {children}
      </main>
    </div>
  );
}
