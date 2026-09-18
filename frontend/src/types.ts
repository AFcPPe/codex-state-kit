export interface LogEntry {
  ts: string;
  method: string;
  path: string;
  status: number;
  ms: number;
}

export interface TurnStateView {
  status: "empty" | "active" | string;
  ageSecs?: number | null;
  len?: number | null;
  source?: string | null;
  capturedAt?: string | null;
}

export interface Status {
  proxyListen: string;
  upstream: string;
  codexHome: string;
  proxyOk: boolean;
  attached: boolean;
  proxyError?: string | null;
  attachError?: string | null;
  outboundProxy: string;
  outboundMode: OutboundMode;
  warpHttp2: boolean;
  warp: WarpStatus;
  fetchError?: string | null;
  fetchOkAt?: string | null;
  turnState: TurnStateView;
  degraded: boolean;
  degradedAt?: string | null;
  logs: LogEntry[];
}

export interface SettingsPatch {
  proxyListen: string;
  upstream: string;
  codexHome: string;
  outboundProxy: string;
  outboundMode: OutboundMode;
  warpHttp2: boolean;
}

export type OutboundMode = "manual" | "warp";

export interface WarpStatus {
  available: boolean;
  registered: boolean;
  phase: "stopped" | "starting" | "registering" | "connecting" | "connected" | "reconnecting" | "error";
  proxyUrl: string | null;
  exitIp: string | null;
  country: string | null;
  error: string | null;
}

export interface ProviderView {
  name: string;
  baseUrl?: string | null;
}

export interface CodexConfigView {
  codexHome: string;
  modelProvider?: string | null;
  openaiBaseUrl?: string | null;
  providers: ProviderView[];
  suggestedBaseUrl: string;
  attached: boolean;
  error?: string | null;
}

export interface LoginStatus {
  loggedIn: boolean;
  authMode?: string | null;
  email?: string | null;
  accountId?: string | null;
}

export type LoginMethod = "device" | "browser";

export interface LoginStart {
  method: LoginMethod;
  userCode: string;
  verificationUri: string;
  expiresIn: number;
  interval: number;
}

export interface LoginPoll {
  status: "pending" | "ok" | "denied" | "expired" | "error" | string;
  message?: string | null;
  login?: LoginStatus | null;
}

export interface ActionResult {
  ok: boolean;
  message: string;
}

export interface Banner {
  kind: "ok" | "error";
  text: string;
}
