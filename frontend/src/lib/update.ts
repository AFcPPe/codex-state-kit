import { invoke } from "@tauri-apps/api/core";
import { isTauri, GITHUB_REPO_URL } from "./api";

export const RELEASES_URL = `${GITHUB_REPO_URL}/releases/latest`;

export interface UpdateInfo {
  currentVersion: string;
  latestVersion: string | null;
  tag: string | null;
  available: boolean;
  releaseUrl: string;
}

let pending: Promise<UpdateInfo> | null = null;
let cached: { time: number; value: UpdateInfo } | null = null;

export function checkUpdate(): Promise<UpdateInfo> {
  if (pending) return pending;
  if (cached && Date.now() - cached.time < 60_000) return Promise.resolve(cached.value);
  pending = invoke<UpdateInfo>("check_update")
    .then((value) => { cached = { time: Date.now(), value }; return value; })
    .finally(() => { pending = null; });
  return pending;
}

export async function openReleasePage(tag?: string | null): Promise<void> {
  if (isTauri) await invoke("open_release_page", { tag: tag ?? null });
  else window.open(RELEASES_URL, "_blank", "noopener,noreferrer");
}
