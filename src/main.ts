import { invoke, isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { desktopEntryUrl } from "./desktop-auth.ts";
import { StartupController, localAction, type StartupState } from "./startup-controller.ts";

function element<T extends HTMLElement>(id: string): T {
  const value = document.getElementById(id);
  if (!value) throw new Error(`Missing startup element: ${id}`);
  return value as T;
}

const setup = element<HTMLElement>("setup");
const heading = element<HTMLHeadingElement>("heading");
const primary = element<HTMLButtonElement>("primary");
const secondary = element<HTMLButtonElement>("secondary");
const pathInput = element<HTMLInputElement>("dsh-path");
const form = element<HTMLFormElement>("path-form");
const details = element<HTMLDetailsElement>("details");
const settings = new URLSearchParams(window.location.search).get("environment") === "1";
let manual = false;
let showDetails = false;
let lastScreen = "";
let detailsKey = "";

function setText(id: string, text: string, visible = true): void {
  const target = element<HTMLElement>(id);
  target.textContent = text;
  target.hidden = !visible;
}

function render(state: StartupState): void {
  const { screen, snapshot, busy } = state;
  const local = screen === "local";
  const ready = screen === "ready";
  const preparing = screen === "preparing";
  const welcome = screen === "welcome";
  setup.dataset.screen = screen;
  setup.dataset.settings = String(settings);
  const titles = { checking: settings ? "运行环境" : "正在打开 DSH…", welcome: "开始使用 DSH", local: "使用已有安装", preparing: "很快就准备好了", ready: settings ? "运行环境" : "一切就绪", launching: settings ? "正在切换运行环境…" : "正在打开 DSH…", error: "暂时还没准备好" };
  heading.textContent = titles[screen];
  if (lastScreen !== screen) {
    setup.scrollTop = 0;
    showDetails = false;
    details.open = false;
    heading.focus({ preventScroll: true });
    lastScreen = screen;
  }
  element("welcome-copy").hidden = !welcome;
  setText("welcome-hint", state.detail, !!state.detail);
  element("flow").hidden = welcome;
  const readyDescription = settings && snapshot?.saved
    ? snapshot.source === "local" ? "沿用本机的安装和更新方式。" : "由应用维护和更新。"
    : snapshot?.source === "local" ? "已连接本机安装，可以开始使用了。" : "DSH 已准备好，可以开始使用了。";
  element("runtime-summary").hidden = !ready;
  setText("runtime-label", settings ? snapshot?.saved ? "当前安装" : "待切换的安装" : "已准备好");
  setText("runtime-version", `DSH ${snapshot?.tools.dsh?.version ?? ""}`.trim());
  setText("runtime-source", snapshot?.source === "local" ? "本机安装" : "应用管理");
  setText("status", local ? "继续使用你已经安装的 DSH。" : ready ? readyDescription : screen === "error" ? "原来的安装没有改变，可以重试或选择其他安装。" : state.detail);
  const missing = local && snapshot?.tools.dsh && !snapshot.ready ? "还需准备插件安装所需的组件。" : "";
  setText("hint", local ? missing || "原来的安装位置和更新方式保持不变。" : ready ? settings ? snapshot?.saved ? "更换安装后会重新启动 DSH。" : "保存后会重新启动 DSH。" : "之后打开应用，就会直接进入。" : "", local || ready);
  element("progress").hidden = !preparing && screen !== "checking" && screen !== "launching";
  element("local-result").hidden = !local;
  setText("local-status", busy ? "正在查找本机安装…" : snapshot?.tools.dsh ? "已找到 DSH" : "还没有找到可用的 DSH");
  element("local-status").classList.toggle("found", !!snapshot?.tools.dsh);
  setText("local-path", snapshot?.tools.dsh?.path ?? snapshot?.detail ?? "如果你刚刚装好，可以重新检测。", local);
  element("location-actions").hidden = !local;
  form.hidden = !local || !manual;
  pathInput.disabled = busy;
  pathInput.setAttribute("aria-invalid", String(local && !!state.error));
  if (local && state.error) pathInput.setAttribute("aria-describedby", "error"); else pathInput.removeAttribute("aria-describedby");
  form.querySelector<HTMLButtonElement>("button")!.disabled = busy;
  element<HTMLButtonElement>("manual").disabled = busy;
  element<HTMLButtonElement>("browse").disabled = busy;
  setText("error", state.error, !!state.error);
  details.hidden = !(preparing || (local && !!snapshot) || (ready && (showDetails || settings)));
  setText("details-label", ready ? "组件与使用位置" : local ? "检测详情" : "查看详情");
  if (snapshot && detailsKey !== JSON.stringify(snapshot.tools)) {
    detailsKey = JSON.stringify(snapshot.tools);
    element("tool-details").replaceChildren(...([['node', 'Node.js'], ['npm', 'npm'], ['pnpm', 'pnpm'], ['dsh', 'DSH']] as const).map(([key, name]) => {
      const tool = snapshot.tools[key];
      const row = document.createElement("p"); row.className = "tool";
      const label = document.createElement("strong"); label.textContent = `${name} · ${tool?.version ?? "未找到"}`;
      row.append(label);
      if (tool) { const path = document.createElement("span"); path.className = "install-path"; path.textContent = tool.path; row.append(path); }
      return row;
    }));
  }
  element("managed").hidden = busy || (ready && snapshot?.source === "managed") || !(settings || (welcome && snapshot?.saved) || screen === "error");
  element("managed").textContent = snapshot?.source === "managed" ? "重新准备组件" : "改由应用下载";
  element("logs").hidden = screen !== "error";
  primary.disabled = busy;
  primary.hidden = preparing || screen === "checking" || screen === "launching";
  secondary.hidden = screen === "checking" || screen === "launching";
  secondary.disabled = busy && !preparing;
  if (welcome) { primary.textContent = "下载并开始"; secondary.textContent = "使用已有安装…"; }
  else if (local) {
    const action = localAction(state);
    primary.textContent = busy ? "正在检测…" : action === "start" ? settings ? "保存并重新启动" : "使用并继续" : action === "prepare" ? "补齐组件并继续" : "重新检测";
    secondary.textContent = "返回";
  } else if (preparing) {
    primary.textContent = "正在准备…"; secondary.textContent = state.cancelling ? "正在取消…" : "取消"; secondary.disabled = state.cancelling;
  } else if (ready) {
    primary.textContent = settings ? snapshot?.saved ? "完成" : "保存并重新启动" : "打开 DSH";
    secondary.textContent = settings ? "使用其他安装…" : "查看使用位置";
  } else if (screen === "error") { primary.textContent = "重试"; secondary.textContent = "使用已有安装…"; }
  else { primary.textContent = "正在打开…"; secondary.textContent = "正在检查运行环境"; }
}

const controller = new StartupController({ invoke, open: url => window.location.replace(desktopEntryUrl(url)) }, render, settings);

primary.addEventListener("click", () => {
  const state = controller.state;
  if (state.screen === "welcome") void controller.prepare();
  else if (state.screen === "local") {
    const action = localAction(state);
    if (action === "start") void controller.start();
    else if (action === "prepare") void controller.prepare("local", state.path);
    else void controller.inspect("local", manual ? pathInput.value.trim() || null : state.path);
  } else if (state.screen === "ready") {
    if (settings && state.snapshot?.saved) void getCurrentWindow().close();
    else void controller.start();
  } else if (state.screen === "error") void controller.retry();
});
secondary.addEventListener("click", () => {
  const screen = controller.state.screen;
  if (screen === "preparing") void controller.cancel();
  else if (screen === "local") {
    manual = false;
    if (settings) void controller.inspect("current");
    else controller.welcome();
  }
  else if (screen === "ready" && !settings) { showDetails = !showDetails; details.hidden = !showDetails; details.open = showDetails; }
  else { manual = false; void controller.inspect("local"); }
});
element("manual").addEventListener("click", () => { manual = true; form.hidden = false; pathInput.focus(); });
element("browse").addEventListener("click", () => {
  void invoke<string | null>("pick_runtime_path").then(path => {
    if (path) { manual = true; pathInput.value = path; void controller.inspect("local", path); }
  }).catch(error => setText("error", String(error)));
});
form.addEventListener("submit", event => { event.preventDefault(); void controller.inspect("local", pathInput.value); });
element("managed").addEventListener("click", () => { void controller.prepare("managed", null); });
element("logs").addEventListener("click", () => { void invoke("open_logs").catch(error => setText("error", String(error))); });

if (isTauri()) void controller.inspect("current", null, true);
else {
  render({ ...controller.state, screen: "error", error: "请在 DSH Desktop 中打开此页面。浏览器预览不会安装软件或模拟成功。", detail: "" });
  for (const button of document.querySelectorAll<HTMLButtonElement>("button")) button.disabled = true;
}
