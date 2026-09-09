use std::{
    collections::{HashSet, VecDeque},
    env,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use semver::Version;
use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::runtime_environment::{self as environment, Environment, RuntimeSource};
use crate::terminal_command::{TerminalCommandInstall, TerminalCommandManager};

const PACKAGE_NAME: &str = "@deepseek-ai/dsh";
const INSTALL_SCRIPT_PACKAGES: [&str; 5] = [
    "@deepseek-ai/dsh-subprocess-local",
    "@google/genai",
    "koffi",
    "node-pty",
    "protobufjs",
];
const WEB_COMMAND_ARGS: [&str; 3] = ["web", "--port", "0"];
const NO_OPEN_ARGUMENT: &str = "--no-open";
const READY_PREFIX: &str = "dsh web: ";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const UPDATE_DELAY: Duration = Duration::from_secs(60);
const UPDATE_INTERVAL_SECS: u64 = 12 * 60 * 60;
const FAILED_RETRY_SECS: u64 = 60 * 60;
const NODE_ENTRY_ENV: &str = "DSH_DESKTOP_NODE_ENTRY";
const NODE_CWD_ENV: &str = "DSH_DESKTOP_NODE_CWD";
const ERROR_TAIL_LINES: usize = 20;
const ERROR_TAIL_LINE_CHARS: usize = 1_000;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupInfo {
    pub url: String,
    pub dsh_version: String,
}

#[derive(Clone, Debug)]
struct SelectedVersions {
    node_version: String,
    npm_version: String,
    dsh_version: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeUpdateChannel {
    #[default]
    Latest,
    Next,
}

impl RuntimeUpdateChannel {
    fn dist_tag(self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::Next => "next",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeState {
    active: Option<String>,
    previous: Option<String>,
    pending: Option<String>,
    last_update_attempt: Option<u64>,
    last_successful_check: Option<u64>,
    #[serde(default)]
    update_channel: RuntimeUpdateChannel,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeUpdatePhase {
    #[default]
    Idle,
    Checking,
    Current,
    Ahead,
    Installing,
    Verifying,
    Ready,
    Error,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeUpdateSnapshot {
    pub phase: RuntimeUpdatePhase,
    pub target_version: Option<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    pub current_version: String,
    pub pending_version: Option<String>,
    pub node_version: String,
    pub npm_version: String,
    pub update_channel: RuntimeUpdateChannel,
    pub update: RuntimeUpdateSnapshot,
    pub source: Option<RuntimeSource>,
    pub path: Option<PathBuf>,
    pub pnpm_version: String,
}

struct Paths {
    node: PathBuf,
    node_launcher: PathBuf,
    npm_cli: PathBuf,
    selected_runtime: PathBuf,
    runtimes: PathBuf,
    state: PathBuf,
    logs: PathBuf,
    npm_cache: PathBuf,
    smoke: PathBuf,
}

struct ManagedProcess {
    child: Child,
    process_id: u32,
}

impl ManagedProcess {
    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn terminate(&mut self) {
        if self.process_id != 0 {
            terminate_child(&mut self.child, self.process_id);
            self.process_id = 0;
        }
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub struct RuntimeManager {
    paths: Paths,
    tool_bin: PathBuf,
    manifest: SelectedVersions,
    terminal_command: TerminalCommandManager,
    state: Mutex<RuntimeState>,
    process: Mutex<Option<ManagedProcess>>,
    startup: Mutex<Option<StartupInfo>>,
    start_gate: Mutex<()>,
    update_status: Mutex<RuntimeUpdateSnapshot>,
    update_scheduled: AtomicBool,
    update_running: AtomicBool,
    shutting_down: AtomicBool,
    environment: Environment,
}

impl RuntimeManager {
    pub fn from_environment(
        resource_dir: PathBuf,
        app_data_dir: PathBuf,
        environment: Environment,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let resource_dir = node_compatible_path(&resource_dir);
        let app_data_dir = node_compatible_path(&app_data_dir);
        let manifest = SelectedVersions {
            node_version: environment.node.version.clone(),
            npm_version: environment
                .npm
                .as_ref()
                .map(|tool| tool.version.clone())
                .unwrap_or_else(|| "未安装".to_string()),
            dsh_version: environment.dsh.version.clone(),
        };
        Version::parse(&manifest.dsh_version)?;
        let data_root = app_data_dir.join("runtime");
        let logs = app_data_dir.join("logs");
        fs::create_dir_all(data_root.join("versions"))?;
        fs::create_dir_all(&logs)?;
        fs::create_dir_all(data_root.join("npm-cache"))?;
        fs::create_dir_all(data_root.join("smoke"))?;

        let paths = Paths {
            node: environment.node.path.clone(),
            node_launcher: resolve_resource(&resource_dir, "node-launcher.mjs"),
            npm_cli: environment
                .npm
                .as_ref()
                .map(|tool| tool.path.clone())
                .unwrap_or_default(),
            selected_runtime: if environment.source == RuntimeSource::Managed {
                environment
                    .dsh
                    .path
                    .ancestors()
                    .nth(5)
                    .ok_or("managed DSH entry has no installation prefix")?
                    .to_path_buf()
            } else {
                app_data_dir.join("runtime/unused-local-fallback")
            },
            runtimes: data_root.join("versions"),
            state: data_root.join("state.json"),
            logs,
            npm_cache: data_root.join("npm-cache"),
            smoke: data_root.join("smoke"),
        };
        if environment.source == RuntimeSource::Managed
            && !paths.selected_runtime.starts_with(&paths.runtimes)
        {
            return Err("managed DSH must stay inside the application's versions directory".into());
        }
        for required in [&paths.node, &paths.node_launcher] {
            if !required.exists() {
                return Err(format!("runtime resource is missing: {}", required.display()).into());
            }
        }

        let tool_bin = environment.create_tool_aliases(&app_data_dir)?;
        let state = read_state(&paths.state).unwrap_or_default();
        let terminal_command = TerminalCommandManager::new(
            &app_data_dir,
            paths.node.clone(),
            paths.selected_runtime.clone(),
            paths.runtimes.clone(),
            paths.state.clone(),
        )
        .with_path_entries(
            std::iter::once(tool_bin.clone())
                .chain(env::split_paths(&environment.path(None)?))
                .collect(),
        );
        let update_status = state
            .pending
            .as_ref()
            .map(|version| RuntimeUpdateSnapshot {
                phase: RuntimeUpdatePhase::Ready,
                target_version: Some(version.clone()),
                detail: Some("更新已就绪，重启后生效".to_string()),
            })
            .unwrap_or_default();
        append_log(
            &paths.logs.join("desktop.log"),
            &format!(
                "DSH Desktop initialized (Node {}, npm {}, selected dsh {})",
                manifest.node_version, manifest.npm_version, manifest.dsh_version
            ),
        );
        Ok(Self {
            paths,
            tool_bin,
            manifest,
            terminal_command,
            state: Mutex::new(state),
            process: Mutex::new(None),
            startup: Mutex::new(None),
            start_gate: Mutex::new(()),
            update_status: Mutex::new(update_status),
            update_scheduled: AtomicBool::new(false),
            update_running: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            environment,
        })
    }

    pub fn start(&self, app: &tauri::AppHandle) -> Result<StartupInfo, String> {
        let _gate = self
            .start_gate
            .lock()
            .map_err(|_| "启动锁已损坏".to_string())?;
        environment::check_cancel(&self.shutting_down)?;

        {
            let mut process = self
                .process
                .lock()
                .map_err(|_| "进程状态锁已损坏".to_string())?;
            if process.as_mut().is_some_and(ManagedProcess::is_running) {
                if let Some(info) = self
                    .startup
                    .lock()
                    .map_err(|_| "启动状态锁已损坏".to_string())?
                    .clone()
                {
                    return Ok(info);
                }
            } else {
                process.take();
            }
        }

        if self.environment.source == RuntimeSource::Local {
            let version = self.environment.dsh.version.clone();
            let (process, url) = self.launch_at(&version, Path::new(""), None, START_TIMEOUT)?;
            *self
                .process
                .lock()
                .map_err(|_| "进程状态锁已损坏".to_string())? = Some(process);
            let info = StartupInfo {
                url,
                dsh_version: version,
            };
            *self
                .startup
                .lock()
                .map_err(|_| "启动状态锁已损坏".to_string())? = Some(info.clone());
            return Ok(info);
        }

        let (candidates, pending) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "版本状态锁已损坏".to_string())?;
            self.reconcile_state(&mut state);
            self.save_state(&state)?;
            (
                launch_candidates(&state, &self.manifest.dsh_version),
                state.pending.clone(),
            )
        };

        let mut failures = Vec::new();
        for version in candidates {
            let Some(runtime_path) = self.runtime_path(&version) else {
                failures.push(format!("DSH {version}: 安装目录不存在"));
                continue;
            };
            match self.launch_at(&version, &runtime_path, None, START_TIMEOUT) {
                Ok((process, url)) => {
                    {
                        let mut state = self
                            .state
                            .lock()
                            .map_err(|_| "版本状态锁已损坏".to_string())?;
                        let old_active = state.active.clone();
                        if state.pending.as_deref() == Some(&version) {
                            state.previous = old_active.filter(|old| old != &version);
                            state.active = Some(version.clone());
                            state.pending = None;
                        } else if state.active.as_deref() != Some(&version) {
                            state.previous = old_active.filter(|old| old != &version);
                            state.active = Some(version.clone());
                        }
                        if pending.is_some() && pending.as_deref() != Some(&version) {
                            state.pending = None;
                        }
                        self.save_state(&state)?;
                    }
                    *self
                        .process
                        .lock()
                        .map_err(|_| "进程状态锁已损坏".to_string())? = Some(process);
                    let info = StartupInfo {
                        url,
                        dsh_version: version.clone(),
                    };
                    *self
                        .startup
                        .lock()
                        .map_err(|_| "启动状态锁已损坏".to_string())? = Some(info.clone());
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.set_title(&format!("DSH Desktop · DSH {version}"));
                    }
                    append_log(
                        &self.desktop_log(),
                        &format!("active runtime: dsh {version}"),
                    );
                    self.set_update_status(
                        RuntimeUpdatePhase::Idle,
                        None,
                        Some("当前版本已就绪".to_string()),
                    );
                    self.terminal_command.refresh_if_installed();
                    return Ok(info);
                }
                Err(error) => {
                    append_log(
                        &self.desktop_log(),
                        &format!("dsh {version} failed to start: {error}"),
                    );
                    failures.push(format!("DSH {version}: {error}"));
                }
            }
        }

        Err(format!("没有可启动的 DSH 版本。\n{}", failures.join("\n")))
    }

    pub fn schedule_update_check(self: &std::sync::Arc<Self>, app: tauri::AppHandle) {
        if self.environment.source == RuntimeSource::Local {
            return;
        }
        if self.update_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let manager = std::sync::Arc::clone(self);
        thread::spawn(move || {
            if !manager.wait_for_update_tick(UPDATE_DELAY) {
                return;
            }
            loop {
                manager.check_for_updates(app.clone(), false);
                if !manager.wait_for_update_tick(Duration::from_secs(FAILED_RETRY_SECS)) {
                    return;
                }
            }
        });
    }

    pub fn check_for_updates(
        self: &std::sync::Arc<Self>,
        app: tauri::AppHandle,
        force: bool,
    ) -> bool {
        if self.environment.source == RuntimeSource::Local {
            self.set_update_status(
                RuntimeUpdatePhase::Idle,
                None,
                Some("本机安装由原安装方式更新；DSH Desktop 不会替换它。".to_string()),
            );
            return false;
        }
        let channel = match self.state.lock() {
            Ok(state) => {
                if !force && !update_check_is_due(&state, unix_time(), force) {
                    return false;
                }
                state.update_channel
            }
            Err(_) => return false,
        };
        if self.update_running.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.set_update_status(
            RuntimeUpdatePhase::Checking,
            None,
            Some(format!("正在查询 npm {} 通道", channel.dist_tag())),
        );
        let manager = std::sync::Arc::clone(self);
        thread::spawn(move || {
            let result = manager.check_for_updates_inner(force, channel);
            match result {
                Ok(UpdateOutcome::Staged(version)) => {
                    append_log(
                        &manager.desktop_log(),
                        &format!("dsh {version} staged; restart to apply"),
                    );
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window
                            .set_title(&format!("DSH Desktop · DSH {version} 可在重启后使用"));
                    }
                    manager.set_update_status(
                        RuntimeUpdatePhase::Ready,
                        Some(version),
                        Some("更新已就绪，重启后生效".to_string()),
                    );
                }
                Ok(UpdateOutcome::Current) => {
                    append_log(
                        &manager.desktop_log(),
                        &format!("npm {} is already active", channel.dist_tag()),
                    );
                    manager.set_update_status(
                        RuntimeUpdatePhase::Current,
                        None,
                        Some(match channel {
                            RuntimeUpdateChannel::Latest => "已是 npm latest 当前版本".to_string(),
                            RuntimeUpdateChannel::Next => "已是 DSH Beta 当前版本".to_string(),
                        }),
                    );
                }
                Ok(UpdateOutcome::Ahead { active, target }) => {
                    append_log(
                        &manager.desktop_log(),
                        &format!(
                            "active dsh {active} is ahead of npm {} ({target}); keeping active runtime",
                            channel.dist_tag()
                        ),
                    );
                    manager.set_update_status(
                        RuntimeUpdatePhase::Ahead,
                        Some(target.clone()),
                        Some(format!(
                            "当前 DSH {active} 高于 npm {} 的 {target}，不会自动降级",
                            channel.dist_tag()
                        )),
                    );
                }
                Ok(UpdateOutcome::Skipped) => manager.restore_resting_update_status(),
                Err(error) => {
                    append_log(
                        &manager.desktop_log(),
                        &format!("update check failed: {error}"),
                    );
                    let target_version = manager.update_snapshot().target_version;
                    manager.set_update_status(
                        RuntimeUpdatePhase::Error,
                        target_version,
                        Some(error),
                    );
                }
            }
            manager.update_running.store(false, Ordering::Release);
        });
        true
    }

    pub fn set_update_channel(&self, channel: RuntimeUpdateChannel) -> Result<bool, String> {
        if self.environment.source == RuntimeSource::Local {
            return Err("本机安装请使用原来的方式选择版本和更新。".to_string());
        }
        if self.update_running.load(Ordering::Acquire) {
            return Err("DSH 更新正在进行，请完成后再切换通道。".to_string());
        }
        let changed = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "版本状态锁已损坏".to_string())?;
            let mut updated = state.clone();
            if !select_update_channel(&mut updated, channel) {
                return Ok(false);
            }
            self.save_state(&updated)?;
            *state = updated;
            true
        };
        append_log(
            &self.desktop_log(),
            &format!(
                "runtime update channel changed to npm {}",
                channel.dist_tag()
            ),
        );
        self.set_update_status(
            RuntimeUpdatePhase::Idle,
            None,
            Some(format!("已切换到 npm {} 通道", channel.dist_tag())),
        );
        Ok(changed)
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        if self.environment.source == RuntimeSource::Local {
            return RuntimeSnapshot {
                current_version: self.environment.dsh.version.clone(),
                pending_version: None,
                node_version: self.environment.node.version.clone(),
                npm_version: self.manifest.npm_version.clone(),
                update_channel: RuntimeUpdateChannel::Latest,
                update: RuntimeUpdateSnapshot {
                    phase: RuntimeUpdatePhase::Idle,
                    target_version: None,
                    detail: Some("使用本机安装，请通过原安装方式更新。".to_string()),
                },
                source: Some(RuntimeSource::Local),
                path: Some(self.environment.dsh.path.clone()),
                pnpm_version: self.environment.pnpm.version.clone(),
            };
        }
        let (current_version, pending_version, update_channel) = self
            .state
            .lock()
            .map(|state| {
                (
                    state
                        .active
                        .clone()
                        .unwrap_or_else(|| self.manifest.dsh_version.clone()),
                    state.pending.clone(),
                    state.update_channel,
                )
            })
            .unwrap_or_else(|_| {
                (
                    self.manifest.dsh_version.clone(),
                    None,
                    RuntimeUpdateChannel::default(),
                )
            });
        let mut update = self.update_snapshot();
        if let Some(pending) = pending_version.as_ref() {
            if !matches!(
                update.phase,
                RuntimeUpdatePhase::Checking
                    | RuntimeUpdatePhase::Installing
                    | RuntimeUpdatePhase::Verifying
                    | RuntimeUpdatePhase::Ahead
                    | RuntimeUpdatePhase::Error
            ) {
                update = RuntimeUpdateSnapshot {
                    phase: RuntimeUpdatePhase::Ready,
                    target_version: Some(pending.clone()),
                    detail: Some("更新已就绪，重启后生效".to_string()),
                };
            }
        }
        RuntimeSnapshot {
            current_version,
            pending_version,
            node_version: self.manifest.node_version.clone(),
            npm_version: self.manifest.npm_version.clone(),
            update_channel,
            update,
            source: Some(self.environment.source),
            path: self
                .state
                .lock()
                .ok()
                .and_then(|state| {
                    state
                        .active
                        .as_deref()
                        .and_then(|version| self.runtime_path(version))
                })
                .map(|path| runtime_cli(&path)),
            pnpm_version: self.environment.pnpm.version.clone(),
        }
    }

    pub fn has_started(&self) -> bool {
        self.startup.lock().is_ok_and(|startup| startup.is_some())
    }

    pub fn terminate(&self) {
        self.shutting_down.store(true, Ordering::Release);
        if let Ok(mut slot) = self.process.lock() {
            if let Some(mut process) = slot.take() {
                process.terminate();
            }
        }
        if let Ok(mut startup) = self.startup.lock() {
            startup.take();
        }
    }

    pub fn install_terminal_command(&self) -> Result<TerminalCommandInstall, String> {
        if self.environment.source == RuntimeSource::Local {
            return Err("当前正在复用本机 DSH，无需再安装桌面端终端命令。".to_string());
        }
        self.terminal_command.install()
    }

    pub fn smoke_runtime(&self) -> Result<StartupInfo, String> {
        let smoke_home =
            self.paths
                .smoke
                .join(format!("packaged-{}-{}", std::process::id(), unix_time()));
        fs::create_dir_all(&smoke_home)
            .map_err(|error| format!("无法创建安装包冒烟测试目录：{error}"))?;
        let result = self.launch_at(
            &self.manifest.dsh_version,
            &self.paths.selected_runtime,
            Some(&smoke_home),
            START_TIMEOUT,
        );
        let result = match result {
            Ok((mut process, url)) => {
                process.terminate();
                Ok(StartupInfo {
                    url,
                    dsh_version: self.manifest.dsh_version.clone(),
                })
            }
            Err(error) => Err(error),
        };
        let _ = fs::remove_dir_all(smoke_home);
        result
    }

    fn wait_for_update_tick(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            if self.shutting_down.load(Ordering::Acquire) {
                return false;
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(60)),
            );
        }
        !self.shutting_down.load(Ordering::Acquire)
    }

    fn reconcile_state(&self, state: &mut RuntimeState) {
        let active_available = state
            .active
            .as_deref()
            .and_then(|version| self.runtime_path(version))
            .is_some();
        if !active_available {
            state.active = Some(self.manifest.dsh_version.clone());
            state.previous = None;
        }
        if state
            .pending
            .as_deref()
            .and_then(|version| self.runtime_path(version))
            .is_none()
        {
            state.pending = None;
        }
        if state
            .previous
            .as_deref()
            .and_then(|version| self.runtime_path(version))
            .is_none()
        {
            state.previous = None;
        }

        if state.pending.is_none()
            && state
                .active
                .as_deref()
                .is_some_and(|active| version_is_newer(&self.manifest.dsh_version, active))
        {
            state.previous = state.active.replace(self.manifest.dsh_version.clone());
        }
    }

    fn runtime_path(&self, version: &str) -> Option<PathBuf> {
        let installed = self.paths.runtimes.join(version);
        if runtime_cli(&installed).is_file() {
            Some(installed)
        } else if version == self.manifest.dsh_version
            && runtime_cli(&self.paths.selected_runtime).is_file()
        {
            Some(self.paths.selected_runtime.clone())
        } else {
            None
        }
    }

    fn launch_at(
        &self,
        version: &str,
        runtime_path: &Path,
        dsh_home: Option<&Path>,
        timeout: Duration,
    ) -> Result<(ManagedProcess, String), String> {
        append_log(&self.desktop_log(), &format!("launching dsh {version}"));
        let first_attempt = self.launch_at_once(runtime_path, dsh_home, timeout, true);
        if first_attempt
            .as_ref()
            .is_err_and(|error| no_open_is_unsupported(error))
        {
            append_log(
                &self.desktop_log(),
                &format!(
                    "dsh {version} does not support {NO_OPEN_ARGUMENT}; retrying legacy web arguments"
                ),
            );
            return self.launch_at_once(runtime_path, dsh_home, timeout, false);
        }
        first_attempt
    }

    fn launch_at_once(
        &self,
        runtime_path: &Path,
        dsh_home: Option<&Path>,
        timeout: Duration,
        suppress_browser: bool,
    ) -> Result<(ManagedProcess, String), String> {
        let local = self.environment.source == RuntimeSource::Local;
        let cli = if local {
            self.environment.dsh.path.clone()
        } else {
            runtime_cli(runtime_path)
        };
        let mut command = if local
            && !cli
                .extension()
                .is_some_and(|ext| ext == "js" || ext == "cjs" || ext == "mjs")
        {
            environment::command(
                &self.environment.dsh,
                Some(&self.environment.node),
                &self.environment.search_path,
            )?
        } else {
            self.node_entry_command(&cli, &user_home())?
        };
        let child_path = self.child_path(if local { None } else { Some(runtime_path) })?;
        command
            .args(WEB_COMMAND_ARGS)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NODE_ENV", "production")
            .env("DSH_DESKTOP", "1")
            .env("COREPACK_ENABLE_NETWORK", "0")
            .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
            .env("PATH", child_path);
        if suppress_browser {
            command.arg(NO_OPEN_ARGUMENT);
        }
        if let Some(home) = dsh_home {
            command.env("DSH_HOME", home);
        }
        configure_managed_command(&mut command);

        let mut child = command
            .spawn()
            .map_err(|error| format!("无法创建 Node 进程：{error}"))?;
        let process_id = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "无法读取 DSH 标准输出".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "无法读取 DSH 错误输出".to_string())?;
        let (ready_tx, ready_rx) = mpsc::channel();
        let output_log = self.runtime_log();
        let error_log = output_log.clone();
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(ERROR_TAIL_LINES)));
        let stderr_tail_writer = Arc::clone(&stderr_tail);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(url) = parse_ready_url(&line) {
                    append_log(
                        &output_log,
                        &format!("stdout | {}", loggable_runtime_stdout(&line)),
                    );
                    let _ = ready_tx.send(url);
                } else {
                    append_log(&output_log, &format!("stdout | {line}"));
                }
            }
        });
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                remember_stderr_line(&stderr_tail_writer, &line);
                append_log(&error_log, &format!("stderr | {line}"));
            }
        });

        let deadline = Instant::now() + timeout;
        loop {
            if self.shutting_down.load(Ordering::Acquire) {
                terminate_child(&mut child, process_id);
                return Err(environment::CANCELLED.to_string());
            }
            if let Ok(url) = ready_rx.try_recv() {
                return Ok((ManagedProcess { child, process_id }, url));
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    terminate_child(&mut child, process_id);
                    return Err(with_stderr_tail(
                        format!("进程在就绪前退出（{status}）"),
                        &stderr_tail,
                    ));
                }
                Ok(None) => {}
                Err(error) => {
                    terminate_child(&mut child, process_id);
                    return Err(with_stderr_tail(
                        format!("无法读取进程状态：{error}"),
                        &stderr_tail,
                    ));
                }
            }
            if Instant::now() >= deadline {
                terminate_child(&mut child, process_id);
                return Err(with_stderr_tail(
                    format!("{timeout:?} 内没有收到就绪信号"),
                    &stderr_tail,
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn node_entry_command(&self, entry: &Path, cwd: &Path) -> Result<Command, String> {
        let launcher_dir = self
            .paths
            .node_launcher
            .parent()
            .ok_or_else(|| "捆绑的 Node 启动器目录无效".to_string())?;
        let launcher_name = self
            .paths
            .node_launcher
            .file_name()
            .ok_or_else(|| "捆绑的 Node 启动器文件名无效".to_string())?;
        let mut command = Command::new(&self.paths.node);
        command
            .arg(launcher_name)
            .current_dir(launcher_dir)
            .env(NODE_ENTRY_ENV, node_compatible_path(entry))
            .env(NODE_CWD_ENV, node_compatible_path(cwd))
            .env("PATH", self.child_path(None)?);
        Ok(command)
    }

    fn child_path(&self, runtime: Option<&Path>) -> Result<std::ffi::OsString, String> {
        let mut paths = vec![self.tool_bin.clone()];
        paths.extend(env::split_paths(&self.environment.path(runtime)?));
        environment::join_path(&paths)
    }

    fn npm_command(&self, cwd: &Path) -> Result<Command, String> {
        let npm = self
            .environment
            .npm
            .as_ref()
            .filter(|tool| environment::npm_can_install(tool))
            .ok_or_else(|| "安装工具需要准备或修复，请打开“运行环境…”后重试。".to_string())?;
        let mut command = if self
            .paths
            .npm_cli
            .extension()
            .is_some_and(|ext| ext == "js" || ext == "mjs" || ext == "cjs")
        {
            self.node_entry_command(&self.paths.npm_cli, cwd)?
        } else {
            environment::command(
                npm,
                Some(&self.environment.node),
                &self.environment.search_path,
            )?
        };
        if !self
            .paths
            .npm_cli
            .extension()
            .is_some_and(|ext| ext == "js" || ext == "mjs" || ext == "cjs")
        {
            command.current_dir(cwd);
        }
        command.env("PATH", self.child_path(None)?);
        Ok(command)
    }

    fn check_for_updates_inner(
        &self,
        force: bool,
        channel: RuntimeUpdateChannel,
    ) -> Result<UpdateOutcome, String> {
        let now = unix_time();
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "版本状态锁已损坏".to_string())?;
            if state.update_channel != channel {
                return Ok(UpdateOutcome::Skipped);
            }
            if !update_check_is_due(&state, now, force) {
                return Ok(UpdateOutcome::Skipped);
            }
            state.last_update_attempt = Some(now);
            self.save_state(&state)?;
        }

        let mut command = self.npm_command(&self.paths.npm_cache)?;
        configure_background_command(&mut command);
        let selector = format!("dist-tags.{}", channel.dist_tag());
        command
            .args(["view", PACKAGE_NAME, selector.as_str(), "--json"])
            .env("npm_config_cache", &self.paths.npm_cache)
            .env("npm_config_update_notifier", "false");
        let output =
            environment::run_output(&mut command, Duration::from_secs(60), &self.shutting_down)?;
        if !output.status.success() {
            return Err(format!(
                "npm view 失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let candidate = parse_npm_version(&output.stdout)?;
        Version::parse(&candidate).map_err(|error| {
            format!(
                "npm {} 不是合法版本 {candidate:?}：{error}",
                channel.dist_tag()
            )
        })?;

        let active = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "版本状态锁已损坏".to_string())?;
            if state.update_channel != channel {
                return Ok(UpdateOutcome::Skipped);
            }
            state.last_successful_check = Some(now);
            self.save_state(&state)?;
            state
                .active
                .clone()
                .unwrap_or_else(|| self.manifest.dsh_version.clone())
        };
        match compare_runtime_versions(&candidate, &active)? {
            std::cmp::Ordering::Less => {
                return Ok(UpdateOutcome::Ahead {
                    active,
                    target: candidate,
                });
            }
            std::cmp::Ordering::Equal => return Ok(UpdateOutcome::Current),
            std::cmp::Ordering::Greater => {}
        }

        self.set_update_status(
            RuntimeUpdatePhase::Installing,
            Some(candidate.clone()),
            Some(format!("正在安装 DSH {candidate}")),
        );
        let destination = self.paths.runtimes.join(&candidate);
        if !runtime_cli(&destination).is_file() {
            self.install_and_verify(&candidate, &destination)?;
        }
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "版本状态锁已损坏".to_string())?;
            if state.update_channel != channel {
                return Ok(UpdateOutcome::Skipped);
            }
            state.pending = Some(candidate.clone());
            self.save_state(&state)?;
        }
        Ok(UpdateOutcome::Staged(candidate))
    }

    pub(crate) fn install_and_verify(
        &self,
        version: &str,
        destination: &Path,
    ) -> Result<(), String> {
        Version::parse(version).map_err(|error| error.to_string())?;
        if destination != self.paths.runtimes.join(version) {
            return Err("安装目标不属于应用的版本目录。".to_string());
        }
        let temporary = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(&self.paths.runtimes)
            .map_err(|error| error.to_string())?;
        let stage = temporary.path().to_path_buf();
        let package_json = update_package_manifest(version);
        fs::write(
            stage.join("package.json"),
            serde_json::to_vec_pretty(&package_json).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("无法写入更新清单：{error}"))?;

        append_log(
            &self.desktop_log(),
            &format!("installing {PACKAGE_NAME}@{version}"),
        );
        let mut install_command = self.npm_command(&stage)?;
        configure_background_command(&mut install_command);
        install_command
            .args([
                "install",
                "--omit=dev",
                "--save-exact",
                "--no-audit",
                "--no-fund",
                "--package-lock=false",
                "--global=false",
                "--prefix=.",
                "--registry=https://registry.npmjs.org",
                "--engine-strict=true",
            ])
            .env("npm_config_cache", &self.paths.npm_cache)
            .env("npm_config_update_notifier", "false")
            .env("npm_config_strict_allow_scripts", "true");
        let output = environment::run_output(
            &mut install_command,
            Duration::from_secs(900),
            &self.shutting_down,
        )?;
        append_log(
            &self.runtime_log(),
            &format!(
                "npm install stdout | {}",
                String::from_utf8_lossy(&output.stdout).trim()
            ),
        );
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let _ = fs::remove_dir_all(&stage);
            return Err(format!("npm install 失败：{message}"));
        }

        self.set_update_status(
            RuntimeUpdatePhase::Verifying,
            Some(version.to_string()),
            Some("正在验证版本与启动能力".to_string()),
        );
        let mut version_command = self.node_entry_command(&runtime_cli(&stage), &stage)?;
        configure_background_command(&mut version_command);
        version_command.arg("--version");
        let version_output = environment::run_output(
            &mut version_command,
            environment::PROBE_TIMEOUT,
            &self.shutting_down,
        )?;
        let reported = String::from_utf8_lossy(&version_output.stdout)
            .trim()
            .to_string();
        if !version_output.status.success() || reported != version {
            let _ = fs::remove_dir_all(&stage);
            return Err(format!("新版本验证失败：预期 {version}，实际 {reported:?}"));
        }

        let smoke_home = self.paths.smoke.join(format!("{version}-{}", unix_time()));
        fs::create_dir_all(&smoke_home)
            .map_err(|error| format!("无法创建冒烟测试目录：{error}"))?;
        let smoke = self.launch_at(version, &stage, Some(&smoke_home), START_TIMEOUT);
        match smoke {
            Ok((mut process, _)) => process.terminate(),
            Err(error) => {
                let _ = fs::remove_dir_all(&smoke_home);
                let _ = fs::remove_dir_all(&stage);
                return Err(format!("新版本启动验证失败：{error}"));
            }
        }
        let _ = fs::remove_dir_all(&smoke_home);
        prune_other_platforms(&stage);

        environment::check_cancel(&self.shutting_down)?;
        if destination.exists() {
            fs::rename(
                destination,
                destination.with_extension(format!("replaced-{}", unix_time())),
            )
            .map_err(|error| format!("无法保留旧版本目录：{error}"))?;
        }
        fs::rename(&stage, destination).map_err(|error| format!("无法提交新版本：{error}"))?;
        Ok(())
    }

    fn save_state(&self, state: &RuntimeState) -> Result<(), String> {
        environment::write_json(&self.paths.state, state)
    }

    fn desktop_log(&self) -> PathBuf {
        self.paths.logs.join("desktop.log")
    }

    fn runtime_log(&self) -> PathBuf {
        self.paths.logs.join("dsh.log")
    }

    fn update_snapshot(&self) -> RuntimeUpdateSnapshot {
        self.update_status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| RuntimeUpdateSnapshot {
                phase: RuntimeUpdatePhase::Error,
                detail: Some("无法读取 DSH 更新状态".to_string()),
                ..RuntimeUpdateSnapshot::default()
            })
    }

    fn set_update_status(
        &self,
        phase: RuntimeUpdatePhase,
        target_version: Option<String>,
        detail: Option<String>,
    ) {
        if let Ok(mut status) = self.update_status.lock() {
            *status = RuntimeUpdateSnapshot {
                phase,
                target_version,
                detail,
            };
        }
    }

    fn restore_resting_update_status(&self) {
        let pending = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.pending.clone());
        if let Some(version) = pending {
            self.set_update_status(
                RuntimeUpdatePhase::Ready,
                Some(version),
                Some("更新已就绪，重启后生效".to_string()),
            );
        } else {
            self.set_update_status(RuntimeUpdatePhase::Idle, None, None);
        }
    }
}

enum UpdateOutcome {
    Current,
    Ahead { active: String, target: String },
    Staged(String),
    Skipped,
}

fn resolve_resource(root: &Path, relative: &str) -> PathBuf {
    let direct = root.join(relative);
    if direct.exists() {
        direct
    } else {
        root.join("resources").join(relative)
    }
}

pub(crate) fn node_compatible_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        dunce::simplified(path).to_path_buf()
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

fn configure_background_command(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    #[cfg(not(windows))]
    let _ = command;
}

pub(crate) fn configure_managed_command(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

pub(crate) fn runtime_cli(runtime: &Path) -> PathBuf {
    runtime.join("node_modules/@deepseek-ai/dsh/lib/bin.js")
}

fn user_home() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            Some(PathBuf::from(drive).join(path))
        })
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_state(path: &Path) -> Option<RuntimeState> {
    serde_json::from_reader(File::open(path).ok()?).ok()
}

fn append_log(path: &Path, message: &str) {
    let timestamp = unix_time();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "[{timestamp}] {message}");
    }
}

fn remember_stderr_line(tail: &Mutex<VecDeque<String>>, line: &str) {
    let mut captured = line.chars().take(ERROR_TAIL_LINE_CHARS).collect::<String>();
    if line.chars().count() > ERROR_TAIL_LINE_CHARS {
        captured.push('…');
    }
    if let Ok(mut lines) = tail.lock() {
        if lines.len() == ERROR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(captured);
    }
}

fn with_stderr_tail(message: String, tail: &Mutex<VecDeque<String>>) -> String {
    let Ok(lines) = tail.lock() else {
        return message;
    };
    if lines.is_empty() {
        message
    } else {
        format!(
            "{message}\n\n关键日志：\n{}",
            lines.iter().cloned().collect::<Vec<_>>().join("\n")
        )
    }
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_ready_url(line: &str) -> Option<String> {
    let url = line.strip_prefix(READY_PREFIX)?.split_whitespace().next()?;
    if url.starts_with("http://127.0.0.1:") {
        Some(url.to_string())
    } else {
        None
    }
}

fn loggable_runtime_stdout(line: &str) -> &str {
    if parse_ready_url(line).is_some() {
        "dsh web: [authenticated loopback URL]"
    } else {
        line
    }
}

fn no_open_is_unsupported(error: &str) -> bool {
    error.contains("unknown option '--no-open'")
}

fn parse_npm_version(bytes: &[u8]) -> Result<String, String> {
    if let Ok(value) = serde_json::from_slice::<String>(bytes) {
        return Ok(value);
    }
    let value = String::from_utf8_lossy(bytes)
        .trim()
        .trim_matches('"')
        .to_string();
    if value.is_empty() {
        Err("npm view 返回了空版本".to_string())
    } else {
        Ok(value)
    }
}

fn update_package_manifest(version: &str) -> serde_json::Value {
    let dependencies = serde_json::Map::from_iter([(
        PACKAGE_NAME.to_string(),
        serde_json::Value::String(version.to_string()),
    )]);
    let allow_scripts = serde_json::Map::from_iter(
        INSTALL_SCRIPT_PACKAGES
            .into_iter()
            .map(|name| (name.to_string(), serde_json::Value::Bool(true))),
    );
    serde_json::json!({
        "private": true,
        "dependencies": dependencies,
        "allowScripts": allow_scripts,
    })
}

fn compare_runtime_versions(candidate: &str, active: &str) -> Result<std::cmp::Ordering, String> {
    let candidate = Version::parse(candidate)
        .map_err(|error| format!("候选 DSH 版本 {candidate:?} 无效：{error}"))?;
    let active = Version::parse(active)
        .map_err(|error| format!("当前 DSH 版本 {active:?} 无效：{error}"))?;
    Ok(candidate.cmp(&active))
}

fn version_is_newer(candidate: &str, active: &str) -> bool {
    compare_runtime_versions(candidate, active).is_ok_and(std::cmp::Ordering::is_gt)
}

fn select_update_channel(state: &mut RuntimeState, channel: RuntimeUpdateChannel) -> bool {
    if state.update_channel == channel {
        return false;
    }
    state.update_channel = channel;
    state.pending = None;
    state.last_update_attempt = None;
    state.last_successful_check = None;
    true
}

fn node_platform_name(rust_name: &str) -> &str {
    match rust_name {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch_name(rust_name: &str) -> &str {
    match rust_name {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    }
}

fn update_is_due(state: &RuntimeState, now: u64) -> bool {
    let success_due = state
        .last_successful_check
        .is_none_or(|last| now.saturating_sub(last) >= UPDATE_INTERVAL_SECS);
    let retry_due = state
        .last_update_attempt
        .is_none_or(|last| now.saturating_sub(last) >= FAILED_RETRY_SECS);
    success_due && retry_due
}

fn update_check_is_due(state: &RuntimeState, now: u64, force: bool) -> bool {
    force || update_is_due(state, now)
}

fn launch_candidates(state: &RuntimeState, bundled: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    [
        state.pending.as_deref(),
        state.active.as_deref(),
        state.previous.as_deref(),
        Some(bundled),
    ]
    .into_iter()
    .flatten()
    .filter(|version| seen.insert((*version).to_string()))
    .map(ToString::to_string)
    .collect()
}

#[cfg(unix)]
pub(crate) fn terminate_child(child: &mut Child, process_id: u32) {
    let process_group = process_id as i32;
    unsafe {
        libc::kill(-process_group, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let exited = matches!(child.try_wait(), Ok(Some(_)));
        let group_exists = unsafe { libc::kill(-process_group, 0) == 0 };
        if exited && !group_exists {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(windows)]
pub(crate) fn terminate_child(child: &mut Child, process_id: u32) {
    let mut terminate = Command::new("taskkill.exe");
    configure_background_command(&mut terminate);
    let _ = terminate
        .args(["/PID", &process_id.to_string(), "/T"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let mut force = Command::new("taskkill.exe");
    configure_background_command(&mut force);
    let _ = force
        .args(["/F", "/PID", &process_id.to_string(), "/T"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.wait();
}

fn prune_other_platforms(runtime: &Path) {
    let prebuilds = runtime.join("node_modules/node-pty/prebuilds");
    let retained = format!(
        "{}-{}",
        node_platform_name(env::consts::OS),
        node_arch_name(env::consts::ARCH)
    );
    let Ok(entries) = fs::read_dir(prebuilds) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() != retained.as_str() {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_node() -> PathBuf {
        let output = Command::new("node")
            .args(["-p", "process.execPath"])
            .output()
            .expect("build-host Node is required for runtime subprocess tests");
        assert!(output.status.success());
        node_compatible_path(&PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim(),
        ))
    }

    fn test_manager(resources: PathBuf, app_data: PathBuf, runtime: &Path) -> RuntimeManager {
        let node = test_node();
        let tool = environment::Tool {
            path: node.clone(),
            bin_dir: node.parent().unwrap().to_path_buf(),
            version: "24.19.0".into(),
        };
        RuntimeManager::from_environment(
            resources,
            app_data,
            Environment {
                source: RuntimeSource::Managed,
                node: tool.clone(),
                npm: None,
                pnpm: tool,
                dsh: environment::Tool {
                    path: runtime_cli(runtime),
                    bin_dir: runtime.join("node_modules/.bin"),
                    version: "1.0.0".into(),
                },
                search_path: env::var_os("PATH")
                    .map(|path| env::split_paths(&path).collect())
                    .unwrap_or_default(),
            },
        )
        .expect("initialize runtime fixture")
    }

    #[test]
    fn user_runtime_is_not_owned_by_the_managed_updater() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app");
        let node = test_node();
        let tool = environment::Tool {
            path: node.clone(),
            bin_dir: node.parent().unwrap().to_path_buf(),
            version: "24.19.0".into(),
        };
        let user_entry = temp.path().join("user-dsh.mjs");
        fs::write(&user_entry, "console.log('1.2.3')").unwrap();
        let manager = RuntimeManager::from_environment(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources"),
            app_data.clone(),
            Environment {
                source: RuntimeSource::Local,
                node: tool.clone(),
                npm: None,
                pnpm: tool,
                dsh: environment::Tool {
                    path: user_entry.clone(),
                    bin_dir: temp.path().to_path_buf(),
                    version: "1.2.3".into(),
                },
                search_path: vec![],
            },
        )
        .unwrap();
        assert!(manager
            .set_update_channel(RuntimeUpdateChannel::Next)
            .is_err());
        assert!(manager.install_terminal_command().is_err());
        assert_eq!(manager.snapshot().source, Some(RuntimeSource::Local));
        assert!(manager.snapshot().pending_version.is_none());
        assert!(!app_data.join("runtime/state.json").exists());
        assert_eq!(
            fs::read_to_string(user_entry).unwrap(),
            "console.log('1.2.3')"
        );
    }

    #[test]
    fn extracts_only_loopback_readiness_lines() {
        assert_eq!(
            parse_ready_url("dsh web: http://127.0.0.1:43121"),
            Some("http://127.0.0.1:43121".to_string())
        );
        assert_eq!(parse_ready_url("dsh web: http://0.0.0.0:43121"), None);
        assert_eq!(parse_ready_url("noise http://127.0.0.1:43121"), None);
    }

    #[test]
    fn omits_authenticated_urls_from_runtime_logs() {
        let ready = "dsh web: http://127.0.0.1:43121/?token=launch-secret";
        assert_eq!(
            loggable_runtime_stdout(ready),
            "dsh web: [authenticated loopback URL]"
        );
        assert!(!loggable_runtime_stdout(ready).contains("launch-secret"));
        assert_eq!(loggable_runtime_stdout("runtime detail"), "runtime detail");
    }

    #[test]
    fn recognizes_only_the_no_open_compatibility_error() {
        assert!(no_open_is_unsupported("error: unknown option '--no-open'"));
        assert!(!no_open_is_unsupported("error: unknown option '--port'"));
        assert!(!no_open_is_unsupported("listen EADDRINUSE"));
    }

    #[test]
    fn retries_a_legacy_runtime_without_no_open() {
        let resources = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources");
        let app_data = env::temp_dir().join(format!(
            "dsh-desktop-legacy-web-{}-{}",
            std::process::id(),
            unix_time()
        ));
        let runtime = app_data.join("runtime/versions/1.0.0");
        let cli = runtime_cli(&runtime);
        fs::create_dir_all(cli.parent().expect("legacy CLI parent"))
            .expect("create legacy CLI directory");
        fs::write(
            &cli,
            r#"
if (process.argv.includes("--no-open")) {
  console.error("error: unknown option '--no-open'");
  process.exit(1);
}
console.log("dsh web: http://127.0.0.1:43123");
setInterval(() => {}, 1000);
"#,
        )
        .expect("write legacy CLI");
        let manager = test_manager(resources, app_data.clone(), &runtime);
        let dsh_home = app_data.join("dsh-home");
        fs::create_dir_all(&dsh_home).expect("create legacy DSH home");

        let (mut process, url) = manager
            .launch_at("legacy-test", &runtime, Some(&dsh_home), START_TIMEOUT)
            .expect("retry legacy runtime without --no-open");
        assert_eq!(url, "http://127.0.0.1:43123");
        process.terminate();
        let _ = fs::remove_dir_all(app_data);
    }

    #[test]
    fn orders_pending_before_rollback_candidates_without_duplicates() {
        let state = RuntimeState {
            active: Some("1.0.0".to_string()),
            previous: Some("0.9.0".to_string()),
            pending: Some("1.1.0".to_string()),
            ..RuntimeState::default()
        };
        assert_eq!(
            launch_candidates(&state, "1.0.0"),
            ["1.1.0", "1.0.0", "0.9.0"]
        );
    }

    #[test]
    fn respects_half_day_checks_and_short_failure_retries() {
        let now = 200_000;
        assert!(update_is_due(&RuntimeState::default(), now));
        assert!(!update_is_due(
            &RuntimeState {
                last_update_attempt: Some(now - 30),
                ..RuntimeState::default()
            },
            now
        ));
        assert!(!update_is_due(
            &RuntimeState {
                last_update_attempt: Some(now - FAILED_RETRY_SECS),
                last_successful_check: Some(now - 60),
                ..RuntimeState::default()
            },
            now
        ));
        assert!(update_is_due(
            &RuntimeState {
                last_update_attempt: Some(now - FAILED_RETRY_SECS),
                last_successful_check: Some(now - UPDATE_INTERVAL_SECS),
                ..RuntimeState::default()
            },
            now
        ));
        assert!(update_check_is_due(
            &RuntimeState {
                last_update_attempt: Some(now),
                last_successful_check: Some(now),
                ..RuntimeState::default()
            },
            now,
            true
        ));
    }

    #[test]
    fn defaults_legacy_runtime_state_to_latest_channel() {
        let state: RuntimeState = serde_json::from_str(
            r#"{
  "active": "0.1.0-rc.7",
  "pending": null
}"#,
        )
        .expect("read legacy runtime state");
        assert_eq!(state.update_channel, RuntimeUpdateChannel::Latest);
        assert_eq!(RuntimeUpdateChannel::Latest.dist_tag(), "latest");
        assert_eq!(RuntimeUpdateChannel::Next.dist_tag(), "next");
    }

    #[test]
    fn switching_channels_cancels_pending_work_without_downgrading_active() {
        let mut state = RuntimeState {
            active: Some("0.1.0-rc.8".to_string()),
            pending: Some("0.1.0-rc.9".to_string()),
            last_update_attempt: Some(100),
            last_successful_check: Some(90),
            update_channel: RuntimeUpdateChannel::Next,
            ..RuntimeState::default()
        };

        assert!(select_update_channel(
            &mut state,
            RuntimeUpdateChannel::Latest
        ));
        assert_eq!(state.active.as_deref(), Some("0.1.0-rc.8"));
        assert_eq!(state.pending, None);
        assert_eq!(state.last_update_attempt, None);
        assert_eq!(state.last_successful_check, None);
        assert!(!select_update_channel(
            &mut state,
            RuntimeUpdateChannel::Latest
        ));
        assert_eq!(
            serde_json::to_value(&state)
                .expect("serialize runtime state")
                .pointer("/updateChannel"),
            Some(&serde_json::Value::String("latest".to_string()))
        );
    }

    #[test]
    fn compares_prereleases_with_semver() {
        assert!(version_is_newer("0.1.0", "0.1.0-rc.6"));
        assert!(version_is_newer("0.1.0-rc.7", "0.1.0-rc.6"));
        assert!(!version_is_newer("0.1.0-rc.5", "0.1.0-rc.6"));
        assert_eq!(
            compare_runtime_versions("0.1.0-rc.7", "0.1.0-rc.8"),
            Ok(std::cmp::Ordering::Less)
        );
    }

    #[test]
    fn normalizes_rust_target_names_to_node_names() {
        assert_eq!(node_platform_name("macos"), "darwin");
        assert_eq!(node_arch_name("aarch64"), "arm64");
        assert_eq!(node_platform_name("windows"), "win32");
        assert_eq!(node_arch_name("x86_64"), "x64");
        assert_eq!(node_platform_name("linux"), "linux");
    }

    #[cfg(unix)]
    #[test]
    fn exposes_selected_node_to_lifecycle_scripts_without_a_system_node() {
        let node = test_node();
        let empty_host_path = env::temp_dir().join(format!(
            "dsh-desktop-empty-path-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&empty_host_path).expect("create empty host PATH directory");
        let path = environment::join_path(&[
            node.parent().expect("selected Node parent").to_path_buf(),
            empty_host_path.clone(),
        ])
        .expect("construct lifecycle PATH");

        let output = Command::new("/bin/sh")
            .args(["-c", "node -p process.execPath"])
            .env("PATH", path)
            .output()
            .expect("run lifecycle shell");
        let _ = fs::remove_dir_all(&empty_host_path);

        assert!(
            output.status.success(),
            "lifecycle shell failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reported = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        assert_eq!(
            fs::canonicalize(reported).expect("canonicalize reported Node"),
            fs::canonicalize(node).expect("canonicalize selected Node")
        );
    }

    #[test]
    fn keeps_recent_stderr_in_the_startup_error() {
        let tail = Mutex::new(VecDeque::new());
        remember_stderr_line(&tail, "first failure");
        remember_stderr_line(&tail, "second failure");
        assert_eq!(
            with_stderr_tail("启动失败".to_string(), &tail),
            "启动失败\n\n关键日志：\nfirst failure\nsecond failure"
        );
    }

    #[cfg(windows)]
    #[test]
    fn simplifies_tauri_verbatim_paths_before_launching_node() {
        assert_eq!(
            node_compatible_path(Path::new(
                r"\\?\C:\Users\test\AppData\Local\DSH Desktop\resources"
            )),
            PathBuf::from(r"C:\Users\test\AppData\Local\DSH Desktop\resources")
        );
        assert_eq!(
            node_compatible_path(Path::new(r"C:\Users\test\AppData\Roaming")),
            PathBuf::from(r"C:\Users\test\AppData\Roaming")
        );
    }

    #[cfg(windows)]
    #[test]
    fn launches_managed_js_from_a_tauri_verbatim_resource_path() {
        let resource_dir =
            fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources"))
                .expect("canonicalize launcher resources");
        assert!(resource_dir.to_string_lossy().starts_with(r"\\?\"));

        let app_data = env::temp_dir().join(format!(
            "dsh-desktop-windows-runtime-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&app_data).expect("create temporary app data");
        let app_data = fs::canonicalize(&app_data).expect("canonicalize temporary app data");
        let runtime = node_compatible_path(&app_data).join("runtime/versions/1.0.0");
        let cli = runtime_cli(&runtime);
        fs::create_dir_all(cli.parent().unwrap()).unwrap();
        fs::write(
            &cli,
            "console.log('dsh web: http://127.0.0.1:43123'); setInterval(() => {}, 1000);\n",
        )
        .unwrap();
        let manager = test_manager(resource_dir, app_data.clone(), &runtime);
        let version = manager.manifest.dsh_version.clone();
        let runtime = manager.paths.selected_runtime.clone();
        let dsh_home = manager.paths.smoke.join("launch-test");
        fs::create_dir_all(&dsh_home).expect("create temporary dsh home");

        let (mut process, url) = manager
            .launch_at(&version, &runtime, Some(&dsh_home), START_TIMEOUT)
            .expect("launch managed runtime fixture");
        assert!(url.starts_with("http://127.0.0.1:"));
        process.terminate();
        let _ = fs::remove_dir_all(app_data);
    }

    #[test]
    fn writes_the_scoped_dsh_dependency_and_install_script_policy() {
        let manifest = update_package_manifest("1.2.3");
        assert_eq!(
            manifest.pointer("/dependencies/@deepseek-ai~1dsh"),
            Some(&serde_json::Value::String("1.2.3".to_string()))
        );
        assert_eq!(
            manifest.pointer("/allowScripts/node-pty"),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(manifest.pointer("/dependencies/PACKAGE_NAME").is_none());
    }
}
