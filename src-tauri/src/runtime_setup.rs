//! First-run preparation and explicit runtime selection, separate from the running process.

use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{
    runtime::{
        self, RuntimeManager, RuntimeSnapshot, RuntimeUpdateChannel, RuntimeUpdateSnapshot,
        StartupInfo,
    },
    runtime_download,
    runtime_environment::{self as environment, Discovery, Environment, RuntimeSource, Tool},
    terminal_command::{TerminalCommandInstall, TerminalCommandManager},
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SetupMode {
    Current,
    Local,
    Managed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupSnapshot {
    pub phase: String,
    pub ready: bool,
    pub source: Option<RuntimeSource>,
    pub tools: Discovery,
    pub detail: Option<String>,
    pub saved: bool,
}

impl Default for SetupSnapshot {
    fn default() -> Self {
        Self {
            phase: "idle".to_string(),
            ready: false,
            source: None,
            tools: Discovery::default(),
            detail: None,
            saved: false,
        }
    }
}

pub struct RuntimeService {
    resources: PathBuf,
    app_data: PathBuf,
    candidate: Mutex<Option<Environment>>,
    running: Mutex<Option<Arc<RuntimeManager>>>,
    preparing: Mutex<Option<Arc<RuntimeManager>>>,
    status: Mutex<SetupSnapshot>,
    operation: Mutex<()>,
    cancel: AtomicBool,
}

impl RuntimeService {
    pub fn new(resources: PathBuf, app_data: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        fs::create_dir_all(app_data.join("runtime/versions"))?;
        fs::create_dir_all(app_data.join("runtime/npm-cache"))?;
        fs::create_dir_all(app_data.join("logs"))?;
        Ok(Self {
            resources: runtime::node_compatible_path(&resources),
            app_data: runtime::node_compatible_path(&app_data),
            candidate: Mutex::new(None),
            running: Mutex::new(None),
            preparing: Mutex::new(None),
            status: Mutex::new(SetupSnapshot::default()),
            operation: Mutex::new(()),
            cancel: AtomicBool::new(false),
        })
    }

    pub fn inspect(&self, mode: SetupMode, path: Option<&str>) -> Result<SetupSnapshot, String> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| "运行环境正在准备，请稍候或取消。".to_string())?;
        self.cancel.store(false, Ordering::Release);
        self.set_phase("checking", "正在查找本机运行环境…");
        *self.candidate.lock().map_err(lock_error)? = None;
        let result = self.inspect_inner(mode, path);
        match &result {
            Ok((snapshot, candidate)) => {
                *self.candidate.lock().map_err(lock_error)? = candidate.clone();
                *self.status.lock().map_err(lock_error)? = snapshot.clone();
            }
            Err(error) => self.set_phase("error", error),
        }
        result.map(|(snapshot, _)| snapshot)
    }

    fn inspect_inner(
        &self,
        mode: SetupMode,
        path: Option<&str>,
    ) -> Result<(SetupSnapshot, Option<Environment>), String> {
        if mode == SetupMode::Current {
            if let Some(mut saved) = self.read_selection()? {
                if saved.source == RuntimeSource::Managed {
                    if let Some(tool) = self.cached_dsh(&saved.node, &saved.search_path)? {
                        saved.dsh = tool;
                    }
                }
                return match environment::validate(&saved, &self.cancel) {
                    Ok(verified) => {
                        let mut snapshot = snapshot_of(&verified, true);
                        if verified.source == RuntimeSource::Managed
                            && !verified
                                .npm
                                .as_ref()
                                .is_some_and(environment::npm_can_install)
                        {
                            snapshot.ready = false;
                            snapshot.detail =
                                Some("应用的安装工具需要修复；当前 DSH 安装没有改变。".to_string());
                        }
                        let candidate = snapshot.ready.then_some(verified);
                        Ok((snapshot, candidate))
                    }
                    Err(error) => Ok((
                        SetupSnapshot {
                            phase: "idle".into(),
                            tools: tools_of(&saved),
                            source: Some(saved.source),
                            saved: true,
                            detail: Some(format!(
                                "之前选择的运行环境不可用。{error} 请重新检测或选择其他安装。"
                            )),
                            ..SetupSnapshot::default()
                        },
                        None,
                    )),
                };
            }
        }
        let mut tools = environment::discover(&self.app_data, path, &self.cancel)?;
        let cached = if mode == SetupMode::Local {
            None
        } else {
            tools
                .node
                .as_ref()
                .map(|node| self.cached_dsh(node, &tools.search_path))
                .transpose()?
                .flatten()
        };
        let source =
            if mode == SetupMode::Managed || (mode == SetupMode::Current && cached.is_some()) {
                tools.dsh = cached;
                RuntimeSource::Managed
            } else {
                RuntimeSource::Local
            };
        let mut candidate = tools.environment(source);
        if source == RuntimeSource::Managed
            && !tools.npm.as_ref().is_some_and(environment::npm_can_install)
        {
            candidate = None;
        }
        let detail = tools.detail.clone();
        Ok((
            SetupSnapshot {
                phase: "idle".into(),
                ready: candidate.is_some(),
                source: tools.dsh.as_ref().map(|_| source),
                tools,
                detail,
                saved: false,
            },
            candidate,
        ))
    }

    pub fn prepare(&self, mode: SetupMode, path: Option<&str>) -> Result<SetupSnapshot, String> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| "已经有准备任务正在进行。".to_string())?;
        self.cancel.store(false, Ordering::Release);
        self.set_phase("checking", "正在确认需要准备的组件…");
        let result = self.prepare_inner(mode, path);
        *self.preparing.lock().map_err(lock_error)? = None;
        match result {
            Ok(selected) => {
                let snapshot = snapshot_of(&selected, false);
                *self.candidate.lock().map_err(lock_error)? = Some(selected);
                *self.status.lock().map_err(lock_error)? = snapshot.clone();
                Ok(snapshot)
            }
            Err(error) => {
                self.set_phase(
                    if self.cancel.load(Ordering::Acquire) {
                        "cancelled"
                    } else {
                        "error"
                    },
                    &error,
                );
                Err(error)
            }
        }
    }

    fn prepare_inner(&self, mode: SetupMode, path: Option<&str>) -> Result<Environment, String> {
        let saved = if mode == SetupMode::Current {
            self.read_selection()?
        } else {
            None
        };
        let saved_path = saved
            .as_ref()
            .filter(|selected| selected.source == RuntimeSource::Local)
            .map(|selected| selected.dsh.path.to_string_lossy().to_string());
        let requested_path = path.or(saved_path.as_deref());
        let mut tools = environment::discover(&self.app_data, requested_path, &self.cancel)?;
        runtime_download::prepare_tools(
            &mut tools,
            &self.app_data,
            &self.cancel,
            &|phase, detail, tools| {
                self.set_tools(tools);
                self.set_phase(phase, detail);
            },
        )?;
        // Re-detection can now use the Node and pnpm that were missing during inspection.
        let refreshed = environment::discover(&self.app_data, requested_path, &self.cancel)?;
        tools.dsh = refreshed.dsh;
        self.set_tools(&tools);
        let force_managed = mode == SetupMode::Managed
            || saved
                .as_ref()
                .is_some_and(|selected| selected.source == RuntimeSource::Managed);
        let source = if force_managed {
            RuntimeSource::Managed
        } else if mode == SetupMode::Local || saved_path.is_some() || tools.dsh.is_some() {
            RuntimeSource::Local
        } else {
            RuntimeSource::Managed
        };
        if source == RuntimeSource::Local {
            let selected = tools.environment(source).ok_or_else(|| {
                "没有找到可用的 DSH。请先安装、指定位置，或返回选择由应用下载。".to_string()
            })?;
            let manager = self.preparation_manager(selected.clone())?;
            self.set_phase("verifying", "正在确认本机 DSH 可以启动…");
            manager.smoke_runtime()?;
            environment::check_cancel(&self.cancel)?;
            return Ok(selected);
        }
        let node = tools.node.as_ref().expect("prepared Node");
        if let Some(cached) = self.cached_dsh(node, &tools.search_path)? {
            tools.dsh = Some(cached);
            let selected = tools
                .environment(RuntimeSource::Managed)
                .expect("prepared tools and cached DSH");
            let manager = self.preparation_manager(selected.clone())?;
            self.set_phase("verifying", "正在确认已有组件可以启动…");
            manager.smoke_runtime()?;
            environment::check_cancel(&self.cancel)?;
            return Ok(selected);
        }
        self.set_phase("installing", "正在获取 DSH 版本…");
        let npm = tools.npm.as_ref().expect("prepared installer");
        let mut command = environment::command(npm, Some(node), &tools.search_path)?;
        command
            .args([
                "view",
                "@deepseek-ai/dsh",
                "dist-tags.latest",
                "--json",
                "--registry=https://registry.npmjs.org",
            ])
            .env("npm_config_cache", self.app_data.join("runtime/npm-cache"));
        let output = environment::run_output(&mut command, Duration::from_secs(60), &self.cancel)?;
        if !output.status.success() {
            return Err("无法连接 npm 获取 DSH，请检查网络后重试。".to_string());
        }
        let version: String = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("DSH 版本信息无效：{error}"))?;
        Version::parse(&version).map_err(|_| "DSH 版本信息未通过校验。".to_string())?;
        let destination = self.app_data.join("runtime/versions").join(&version);
        tools.dsh = Some(Tool {
            path: runtime::runtime_cli(&destination),
            bin_dir: destination.join("node_modules/.bin"),
            version: version.clone(),
        });
        let selected = tools
            .environment(RuntimeSource::Managed)
            .expect("prepared selection");
        let manager = self.preparation_manager(selected.clone())?;
        self.set_phase("installing", &format!("正在安装 DSH {version}…"));
        manager.install_and_verify(&version, &destination)?;
        environment::check_cancel(&self.cancel)?;
        Ok(selected)
    }

    fn preparation_manager(&self, selected: Environment) -> Result<Arc<RuntimeManager>, String> {
        let manager = Arc::new(
            RuntimeManager::from_environment(
                self.resources.clone(),
                self.app_data.clone(),
                selected,
            )
            .map_err(|error| error.to_string())?,
        );
        *self.preparing.lock().map_err(lock_error)? = Some(Arc::clone(&manager));
        if self.cancel.load(Ordering::Acquire) {
            manager.terminate();
            return Err(environment::CANCELLED.into());
        }
        Ok(manager)
    }

    pub fn cancel_preparation(&self) {
        self.cancel.store(true, Ordering::Release);
        if let Ok(preparing) = self.preparing.lock() {
            if let Some(manager) = preparing.as_ref() {
                manager.terminate();
            }
        }
    }

    pub fn setup_snapshot(&self) -> SetupSnapshot {
        let mut snapshot = self
            .status
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        if let Ok(preparing) = self.preparing.lock() {
            if let Some(manager) = preparing.as_ref() {
                let update = manager.snapshot().update;
                if update.phase == runtime::RuntimeUpdatePhase::Verifying {
                    snapshot.phase = "verifying".into();
                    snapshot.detail = update.detail;
                }
            }
        }
        snapshot
    }

    pub fn start(&self, app: &tauri::AppHandle) -> Result<StartupInfo, String> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| "运行环境正在准备，请稍候。".to_string())?;
        self.cancel.store(false, Ordering::Release);
        let selected = self
            .candidate
            .lock()
            .map_err(lock_error)?
            .clone()
            .ok_or_else(|| "请先选择或准备一个可用的运行环境。".to_string())?;
        let manager = Arc::new(
            RuntimeManager::from_environment(
                self.resources.clone(),
                self.app_data.clone(),
                selected.clone(),
            )
            .map_err(|error| error.to_string())?,
        );
        if let Some(previous) = self
            .running
            .lock()
            .map_err(lock_error)?
            .replace(Arc::clone(&manager))
        {
            previous.terminate();
        }
        environment::check_cancel(&self.cancel)?;
        let info = match manager.start(app) {
            Ok(info) => info,
            Err(error) => {
                manager.terminate();
                return Err(error);
            }
        };
        if let Err(error) = environment::write_json(&self.selection_path(), &selected) {
            manager.terminate();
            return Err(error);
        }
        *self.running.lock().map_err(lock_error)? = Some(manager);
        Ok(info)
    }

    pub fn save_candidate(&self) -> Result<(), String> {
        let _operation = self
            .operation
            .try_lock()
            .map_err(|_| "运行环境正在准备，请稍候。".to_string())?;
        let selected = self
            .candidate
            .lock()
            .map_err(lock_error)?
            .clone()
            .ok_or_else(|| "请先选择一个可用的运行环境。".to_string())?;
        self.smoke_runtime()?;
        environment::write_json(&self.selection_path(), &selected)
    }

    fn selection_path(&self) -> PathBuf {
        self.app_data.join("runtime/selection.json")
    }

    fn read_selection(&self) -> Result<Option<Environment>, String> {
        let file = match fs::File::open(self.selection_path()) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("无法读取已选运行环境：{error}")),
        };
        let selected: Environment = serde_json::from_reader(file).map_err(|_| {
            "已保存的运行环境配置损坏，请明确选择本机安装或应用管理，不会自动改用其他环境。"
                .to_string()
        })?;
        if selected.source == RuntimeSource::Managed
            && !selected
                .dsh
                .path
                .starts_with(self.app_data.join("runtime/versions"))
        {
            return Err("应用管理的 DSH 安装路径不属于应用目录。".to_string());
        }
        Ok(Some(selected))
    }

    fn cached_dsh(&self, node: &Tool, paths: &[PathBuf]) -> Result<Option<Tool>, String> {
        let state_path = self.app_data.join("runtime/state.json");
        let state: serde_json::Value = match fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| "应用版本状态无法读取，请先查看日志。".to_string())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("无法读取应用版本状态：{error}")),
        };
        for key in ["active", "previous"] {
            let Some(version) = state.get(key).and_then(|value| value.as_str()) else {
                continue;
            };
            if Version::parse(version).is_err() {
                return Err("应用版本状态包含无效版本。".to_string());
            }
            let root = self.app_data.join("runtime/versions").join(version);
            if let Ok(mut tool) = environment::probe(
                &runtime::runtime_cli(&root),
                Some(node),
                paths,
                "DSH",
                &self.cancel,
            ) {
                tool.bin_dir = root.join("node_modules/.bin");
                return Ok(Some(tool));
            }
        }
        Ok(None)
    }

    fn set_phase(&self, phase: &str, detail: &str) {
        if let Ok(mut status) = self.status.lock() {
            status.phase = phase.into();
            status.detail = Some(detail.into());
            status.ready = false;
        }
    }

    fn set_tools(&self, tools: &Discovery) {
        if let Ok(mut status) = self.status.lock() {
            status.tools = tools.clone();
        }
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.running
            .lock()
            .ok()
            .and_then(|manager| manager.as_ref().map(|manager| manager.snapshot()))
            .unwrap_or_else(|| RuntimeSnapshot {
                current_version: "未启动".into(),
                pending_version: None,
                node_version: "未准备".into(),
                npm_version: "未准备".into(),
                pnpm_version: "未准备".into(),
                source: None,
                path: None,
                update_channel: RuntimeUpdateChannel::Latest,
                update: RuntimeUpdateSnapshot {
                    detail: Some("请先完成运行环境准备。".into()),
                    ..RuntimeUpdateSnapshot::default()
                },
            })
    }

    pub fn has_started(&self) -> bool {
        self.running.lock().is_ok_and(|running| {
            running
                .as_ref()
                .is_some_and(|manager| manager.has_started())
        })
    }

    pub fn schedule_update_check(&self, app: tauri::AppHandle) {
        if let Ok(running) = self.running.lock() {
            if let Some(manager) = running.as_ref() {
                manager.schedule_update_check(app);
            }
        }
    }
    pub fn check_for_updates(&self, app: tauri::AppHandle, force: bool) -> bool {
        self.running
            .lock()
            .ok()
            .and_then(|manager| {
                manager
                    .as_ref()
                    .map(|manager| manager.check_for_updates(app, force))
            })
            .unwrap_or(false)
    }
    pub fn set_update_channel(&self, channel: RuntimeUpdateChannel) -> Result<bool, String> {
        self.running
            .lock()
            .map_err(lock_error)?
            .as_ref()
            .ok_or_else(|| "请先启动 DSH。".to_string())?
            .set_update_channel(channel)
    }
    pub fn terminate(&self) {
        self.cancel_preparation();
        if let Ok(running) = self.running.lock() {
            if let Some(manager) = running.as_ref() {
                manager.terminate();
            }
        }
    }
    pub fn open_logs(&self) -> Result<(), String> {
        let mut command = std::process::Command::new(if cfg!(windows) {
            "explorer.exe"
        } else if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        });
        command
            .arg(self.app_data.join("logs"))
            .spawn()
            .map_err(|error| error.to_string())?;
        Ok(())
    }
    pub fn install_terminal_command(&self) -> Result<TerminalCommandInstall, String> {
        self.running
            .lock()
            .map_err(lock_error)?
            .as_ref()
            .ok_or_else(|| "请先启动 DSH。".to_string())?
            .install_terminal_command()
    }
    pub fn uninstall_terminal_command(&self) -> Result<String, String> {
        TerminalCommandManager::new(
            &self.app_data,
            PathBuf::new(),
            PathBuf::new(),
            self.app_data.join("runtime/versions"),
            self.app_data.join("runtime/state.json"),
        )
        .uninstall()
    }
    pub fn smoke_runtime(&self) -> Result<StartupInfo, String> {
        let selected = self
            .candidate
            .lock()
            .map_err(lock_error)?
            .clone()
            .ok_or_else(|| "还没有准备好的运行环境。".to_string())?;
        self.preparation_manager(selected)?.smoke_runtime()
    }
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> String {
    "运行环境状态锁已损坏，请重新打开应用。".to_string()
}

fn tools_of(selected: &Environment) -> Discovery {
    Discovery {
        node: Some(selected.node.clone()),
        npm: selected.npm.clone(),
        pnpm: Some(selected.pnpm.clone()),
        dsh: Some(selected.dsh.clone()),
        search_path: selected.search_path.clone(),
        detail: None,
    }
}
fn snapshot_of(selected: &Environment, saved: bool) -> SetupSnapshot {
    SetupSnapshot {
        phase: "ready".into(),
        ready: true,
        source: Some(selected.source),
        tools: tools_of(selected),
        detail: None,
        saved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initializes_without_any_bundled_runtime_or_download() {
        let data = tempfile::tempdir().unwrap();
        let service =
            RuntimeService::new(data.path().join("no-resources"), data.path().to_path_buf())
                .unwrap();
        assert_eq!(service.setup_snapshot().phase, "idle");
        assert!(service.read_selection().unwrap().is_none());
        assert!(!data.path().join("runtime/toolchains/node").exists());
        assert!(service.save_candidate().is_err());
    }
    #[test]
    fn corrupt_selection_never_silently_falls_back() {
        let data = tempfile::tempdir().unwrap();
        let service = RuntimeService::new(PathBuf::new(), data.path().to_path_buf()).unwrap();
        fs::write(service.selection_path(), "broken").unwrap();
        assert!(service.read_selection().is_err());
        assert_eq!(
            fs::read_to_string(service.selection_path()).unwrap(),
            "broken"
        );
    }
}
