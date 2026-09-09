/** Real setup state transitions; the UI and tests supply the command transport. */
export interface ToolInfo { path: string; binDir: string; version: string }
export type SetupMode = "current" | "local" | "managed";
export interface SetupSnapshot {
  phase: "idle" | "checking" | "downloading" | "installing" | "verifying" | "ready" | "cancelled" | "error";
  ready: boolean;
  source: "local" | "managed" | null;
  tools: { node: ToolInfo | null; npm: ToolInfo | null; pnpm: ToolInfo | null; dsh: ToolInfo | null; detail: string | null };
  detail: string | null;
  saved: boolean;
}
export type StartupScreen = "checking" | "welcome" | "local" | "preparing" | "ready" | "launching" | "error";
export interface StartupState {
  screen: StartupScreen;
  busy: boolean;
  cancelling: boolean;
  mode: SetupMode;
  path: string | null;
  snapshot: SetupSnapshot | null;
  detail: string;
  error: string;
}
export interface StartupTransport {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  open(url: string): void;
}

export class StartupController {
  state: StartupState = { screen: "checking", busy: false, cancelling: false, mode: "current", path: null, snapshot: null, detail: "正在检查运行环境…", error: "" };
  private retryAction: () => Promise<void> = () => this.inspect("current", null, true);
  private transport: StartupTransport;
  private changed: (state: StartupState) => void;
  readonly settings: boolean;

  constructor(transport: StartupTransport, changed: (state: StartupState) => void, settings: boolean) {
    this.transport = transport; this.changed = changed; this.settings = settings;
  }

  private patch(change: Partial<StartupState>): void {
    this.state = { ...this.state, ...change };
    this.changed(this.state);
  }

  async inspect(mode: SetupMode, path: string | null = null, autoStart = false): Promise<void> {
    if (this.state.busy) return;
    this.retryAction = () => this.inspect(mode, path, autoStart);
    this.patch({ screen: mode === "local" ? "local" : "checking", mode, path, busy: true, error: "", snapshot: null, detail: "正在查找本机安装…" });
    try {
      const snapshot = await this.transport.invoke<SetupSnapshot>("inspect_runtime", { mode, path });
      this.patch({ snapshot, busy: false, detail: snapshot.detail ?? "", screen: mode === "local" ? "local" : snapshot.ready ? "ready" : "welcome" });
      if (snapshot.ready && autoStart && !this.settings) await this.start();
    } catch (error) {
      this.patch({ busy: false, screen: mode === "local" ? "local" : "error", error: message(error), detail: "" });
    }
  }

  async prepare(mode: SetupMode = this.state.mode, path: string | null = this.state.path): Promise<void> {
    if (this.state.busy) return;
    this.retryAction = () => this.prepare(mode, path);
    this.patch({ screen: "preparing", mode, path, busy: true, cancelling: false, error: "", detail: "正在确认需要准备的组件…" });
    let polling = false;
    const timer = setInterval(() => {
      if (polling || this.state.screen !== "preparing" || this.state.cancelling) return;
      polling = true;
      void this.transport.invoke<SetupSnapshot>("get_setup_status").then(snapshot => {
        if (this.state.screen === "preparing" && !this.state.cancelling) this.patch({ snapshot, detail: snapshot.detail ?? "正在准备运行环境…" });
      }).catch(() => { /* The preparation command remains authoritative if a status poll fails. */ }).finally(() => { polling = false; });
    }, 300);
    try {
      const snapshot = await this.transport.invoke<SetupSnapshot>("prepare_runtime", { mode, path });
      if (this.state.cancelling) {
        this.patch({ screen: mode === "local" ? "local" : "welcome", busy: false, cancelling: false, detail: "准备已停止，原来的安装没有改变。" });
      } else {
        this.patch({ screen: "ready", busy: false, snapshot, detail: "" });
      }
    } catch (error) {
      if (this.state.cancelling) this.patch({ screen: mode === "local" ? "local" : "welcome", busy: false, cancelling: false, detail: "已取消准备，原来的安装没有改变。", error: "" });
      else this.patch({ screen: "error", busy: false, error: message(error) });
    } finally { clearInterval(timer); }
  }

  async cancel(): Promise<void> {
    if (this.state.screen !== "preparing" || this.state.cancelling) return;
    this.patch({ cancelling: true, detail: "正在取消准备…" });
    try { await this.transport.invoke("cancel_setup"); }
    catch (error) { this.patch({ cancelling: false, error: `取消失败：${message(error)}` }); }
  }

  async start(): Promise<void> {
    if (this.state.busy || !this.state.snapshot?.ready) return;
    this.retryAction = () => this.start();
    this.patch({ screen: "launching", busy: true, error: "", detail: this.settings ? "正在保存选择并重新启动…" : "正在打开 DSH…" });
    try {
      if (this.settings) await this.transport.invoke("apply_runtime");
      else {
        const info = await this.transport.invoke<{ url: string; dshVersion: string }>("bootstrap");
        this.transport.open(info.url);
      }
    } catch (error) { this.patch({ screen: "error", busy: false, error: message(error) }); }
  }

  welcome(): void {
    if (this.state.busy) return;
    this.patch({ screen: "welcome", mode: "current", path: null, snapshot: null, error: "", detail: "" });
  }

  retry(): Promise<void> { return this.retryAction(); }
}

/** Provides one action for a detected installation, including any missing components. */
export function localAction(state: StartupState): "start" | "prepare" | "detect" {
  if (state.snapshot?.ready) return "start";
  if (state.snapshot?.tools.dsh || (state.path && state.snapshot && !state.snapshot.tools.node)) return "prepare";
  return "detect";
}

function message(error: unknown): string { return error instanceof Error ? error.message : String(error); }
