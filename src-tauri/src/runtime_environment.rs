//! Discovers user tools without changing their installation or the host environment.

use std::{
    collections::HashSet,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::runtime::{configure_managed_command, node_compatible_path, terminate_child};

pub const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
pub const CANCELLED: &str = "已取消准备，原来的安装没有改变。";

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub path: PathBuf,
    pub bin_dir: PathBuf,
    pub version: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeSource {
    Local,
    Managed,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub source: RuntimeSource,
    pub node: Tool,
    pub npm: Option<Tool>,
    pub pnpm: Tool,
    pub dsh: Tool,
    pub search_path: Vec<PathBuf>,
}

impl Environment {
    pub fn path(&self, runtime: Option<&Path>) -> Result<std::ffi::OsString, String> {
        let mut paths = vec![self.node.bin_dir.clone(), self.pnpm.bin_dir.clone()];
        if let Some(npm) = &self.npm {
            paths.push(npm.bin_dir.clone());
        }
        if let Some(root) = runtime {
            paths.push(root.join("node_modules/.bin"));
        }
        paths.extend(self.search_path.clone());
        join_path(&paths)
    }

    /// Private command aliases keep a Node distribution's bundled shims from shadowing selected tools.
    pub fn create_tool_aliases(&self, app_data: &Path) -> Result<PathBuf, String> {
        use sha2::{Digest, Sha256};
        let mut scripts = Vec::new();
        for (name, tool) in [("npm", self.npm.as_ref()), ("pnpm", Some(&self.pnpm))] {
            let Some(tool) = tool else { continue };
            let resolved = fs::canonicalize(&tool.path).map_err(|error| error.to_string())?;
            let javascript = resolved
                .extension()
                .is_some_and(|ext| ext == "js" || ext == "mjs" || ext == "cjs");
            #[cfg(unix)]
            let script = {
                let quote =
                    |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"));
                let invocation = if javascript {
                    format!("{} {}", quote(&self.node.path), quote(&tool.path))
                } else {
                    quote(&tool.path)
                };
                format!("#!/bin/sh\nexec {invocation} \"$@\"\n")
            };
            #[cfg(windows)]
            let script = {
                let quote = |path: &Path| -> Result<String, String> {
                    let text = path.to_string_lossy();
                    if text.contains(['%', '!', '"', '^', '\r', '\n']) {
                        return Err(
                            "工具路径含有批处理无法安全转义的字符，请选择其他安装位置。".into()
                        );
                    }
                    Ok(format!("\"{text}\""))
                };
                let invocation = if javascript {
                    format!("{} {}", quote(&self.node.path)?, quote(&tool.path)?)
                } else {
                    quote(&tool.path)?
                };
                format!("@echo off\r\n{invocation} %*\r\n")
            };
            scripts.push((name, script));
        }
        let key = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&scripts).map_err(|error| error.to_string())?)
        );
        let root = app_data.join("runtime/commands");
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let destination = root.join(key);
        if destination.is_dir() {
            return Ok(destination);
        }
        let stage = tempfile::tempdir_in(&root).map_err(|error| error.to_string())?;
        for (name, script) in scripts {
            let file = stage.path().join(if cfg!(windows) {
                format!("{name}.cmd")
            } else {
                name.into()
            });
            fs::write(&file, script).map_err(|error| error.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&file, fs::Permissions::from_mode(0o755))
                    .map_err(|error| error.to_string())?;
            }
        }
        fs::rename(stage.path(), &destination).map_err(|error| error.to_string())?;
        Ok(destination)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    pub node: Option<Tool>,
    pub npm: Option<Tool>,
    pub pnpm: Option<Tool>,
    pub dsh: Option<Tool>,
    pub detail: Option<String>,
    #[serde(skip)]
    pub search_path: Vec<PathBuf>,
}

impl Discovery {
    pub fn environment(&self, source: RuntimeSource) -> Option<Environment> {
        Some(Environment {
            source,
            node: self.node.clone()?,
            npm: self.npm.clone(),
            pnpm: self.pnpm.clone()?,
            dsh: self.dsh.clone()?,
            search_path: self.search_path.clone(),
        })
    }
}

pub fn node_is_compatible(version: &Version) -> bool {
    version.pre.is_empty() && ((version.major == 22 && version.minor >= 19) || version.major >= 24)
}

pub fn npm_can_install(tool: &Tool) -> bool {
    Version::parse(&tool.version)
        .is_ok_and(|version| version.major > 11 || (version.major == 11 && version.minor >= 19))
}

pub fn home_dir() -> Result<PathBuf, String> {
    env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "无法确定当前用户目录。".to_string())
}

pub fn expand_path(value: &str) -> Result<PathBuf, String> {
    let value = value.trim();
    let path = if let Some(relative) = value.strip_prefix("~/") {
        home_dir()?.join(relative)
    } else {
        PathBuf::from(value)
    };
    if !path.is_absolute() {
        return Err("请输入 DSH 可执行文件的完整路径。".to_string());
    }
    if !path.is_file() {
        return Err("这个位置没有可执行文件，请重新选择。".to_string());
    }
    Ok(node_compatible_path(&path))
}

pub fn join_path(paths: &[PathBuf]) -> Result<std::ffi::OsString, String> {
    let mut seen = HashSet::new();
    env::join_paths(
        paths
            .iter()
            .filter(|path| !path.as_os_str().is_empty() && seen.insert((*path).clone())),
    )
    .map_err(|error| format!("无法构造运行环境 PATH：{error}"))
}

pub fn command(tool: &Tool, node: Option<&Tool>, paths: &[PathBuf]) -> Result<Command, String> {
    let javascript = tool
        .path
        .extension()
        .is_some_and(|ext| ext == "js" || ext == "mjs" || ext == "cjs");
    let mut cmd = if javascript {
        let node = node.ok_or_else(|| "运行这个命令需要兼容的 Node.js。".to_string())?;
        let mut cmd = Command::new(&node.path);
        cmd.arg(&tool.path);
        cmd
    } else {
        // Rust invokes Windows .cmd launchers without concatenating shell arguments.
        #[cfg(windows)]
        if tool
            .path
            .extension()
            .is_some_and(|ext| ext == "cmd" || ext == "bat")
            && tool
                .path
                .to_string_lossy()
                .contains(['%', '!', '&', '^', '\n', '\r'])
        {
            return Err(
                "这个命令路径包含批处理特殊字符，请直接选择 JavaScript 入口或 .exe。".to_string(),
            );
        }
        Command::new(&tool.path)
    };
    let mut child_path = Vec::new();
    if let Some(node) = node {
        child_path.push(node.bin_dir.clone());
    }
    child_path.extend_from_slice(paths);
    cmd.env("PATH", join_path(&child_path)?)
        .env("COREPACK_ENABLE_NETWORK", "0")
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .env("npm_config_update_notifier", "false")
        .stdin(Stdio::null());
    if let Ok(home) = home_dir() {
        cmd.current_dir(home);
    }
    Ok(cmd)
}

pub fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(CANCELLED.to_string())
    } else {
        Ok(())
    }
}

/// Drains both pipes while polling cancellation; only the last 128 KiB is retained.
pub fn run_output(
    cmd: &mut Command,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Output, String> {
    check_cancel(cancel)?;
    configure_managed_command(cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|error| format!("无法运行命令：{error}"))?;
    let pid = child.id();
    fn drain(mut reader: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut tail = Vec::new();
            let mut buffer = [0_u8; 8192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                tail.extend_from_slice(&buffer[..count]);
                if tail.len() > 128 * 1024 {
                    tail.drain(..tail.len() - 128 * 1024);
                }
            }
            let _ = tx.send(tail);
        });
        rx
    }
    let stdout = drain(child.stdout.take().expect("piped stdout"));
    let stderr = drain(child.stderr.take().expect("piped stderr"));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            terminate_child(&mut child, pid);
            return Err(if cancel.load(Ordering::Acquire) {
                CANCELLED.to_string()
            } else {
                "命令执行超时，请检查环境后重试。".to_string()
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(40)),
            Err(error) => {
                terminate_child(&mut child, pid);
                return Err(error.to_string());
            }
        }
    };
    let stdout = stdout.recv_timeout(Duration::from_secs(1)).map_err(|_| {
        terminate_child(&mut child, pid);
        "命令输出没有正常结束。".to_string()
    })?;
    let stderr = stderr.recv_timeout(Duration::from_secs(1)).map_err(|_| {
        terminate_child(&mut child, pid);
        "命令错误输出没有正常结束。".to_string()
    })?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

pub fn probe(
    path: &Path,
    node: Option<&Tool>,
    paths: &[PathBuf],
    name: &str,
    cancel: &AtomicBool,
) -> Result<Tool, String> {
    if !path.is_file() {
        return Err(format!("{name} 的安装位置已不存在。"));
    }
    let bin_dir = path
        .parent()
        .ok_or_else(|| "命令目录无效。".to_string())?
        .to_path_buf();
    let resolved =
        node_compatible_path(&fs::canonicalize(path).map_err(|error| error.to_string())?);
    let mut tool = Tool {
        path: resolved,
        bin_dir,
        version: String::new(),
    };
    let mut cmd = command(&tool, node, paths)?;
    cmd.arg("--version");
    let output = run_output(&mut cmd, PROBE_TIMEOUT, cancel)?;
    if !output.status.success() {
        return Err(format!(
            "{name} 无法运行，请检查它所用的 Node 或原安装方式。"
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find_map(|line| Version::parse(line.trim().trim_start_matches('v')).ok())
        .ok_or_else(|| format!("无法识别 {name} 的版本。"))?;
    if name == "Node.js" && !node_is_compatible(&version) {
        return Err(format!(
            "Node.js {version} 不兼容，需要 22.19+（22.x）或 24 及以上正式版。"
        ));
    }
    if name == "pnpm" && version.major < 10 {
        return Err("需要 pnpm 10 或更高版本。".to_string());
    }
    tool.version = version.to_string();
    if name == "Node.js" {
        // Resolve fnm/nvm/Volta shims to the actual executable before saving the selection.
        let mut cmd = command(&tool, None, paths)?;
        cmd.args(["-p", "process.execPath"]);
        let output = run_output(&mut cmd, PROBE_TIMEOUT, cancel)?;
        let actual = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if !output.status.success() || !actual.is_file() {
            return Err("无法定位 Node.js 可执行文件。".to_string());
        }
        tool.path =
            node_compatible_path(&fs::canonicalize(actual).map_err(|error| error.to_string())?);
        tool.bin_dir = tool
            .path
            .parent()
            .expect("absolute node path")
            .to_path_buf();
    } else {
        tool.path = node_compatible_path(path);
    }
    Ok(tool)
}

pub fn search_paths(cancel: &AtomicBool) -> Result<Vec<PathBuf>, String> {
    if cfg!(debug_assertions) || env::var_os("DSH_DESKTOP_RUNTIME_SMOKE").is_some() {
        if let Some(path) = env::var_os("DSH_DESKTOP_SMOKE_PATH") {
            return Ok(env::split_paths(&path)
                .filter(|path| path.is_absolute())
                .collect());
        }
    }
    let mut paths = Vec::new();
    #[cfg(unix)]
    if let Some(shell) = env::var_os("SHELL").filter(|path| Path::new(path).is_file()) {
        let mut cmd = Command::new(shell);
        cmd.args([
            "-ilc",
            "printf '\\n__DSH_DESKTOP_PATH__'; /usr/bin/printenv PATH",
        ]);
        if let Ok(output) = run_output(&mut cmd, Duration::from_secs(5), cancel) {
            if let Some(value) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.strip_prefix("__DSH_DESKTOP_PATH__"))
            {
                paths.extend(env::split_paths(value));
            }
        }
    }
    check_cancel(cancel)?;
    if let Some(path) = env::var_os("PATH") {
        paths.extend(env::split_paths(&path));
    }
    if let Ok(home) = home_dir() {
        paths.extend([
            home.join(".local/bin"),
            home.join("Library/pnpm"),
            home.join(".volta/bin"),
        ]);
    }
    #[cfg(unix)]
    paths.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]);
    #[cfg(windows)]
    {
        if let Some(appdata) = env::var_os("APPDATA") {
            paths.push(PathBuf::from(appdata).join("npm"));
        }
        if let Some(program_files) = env::var_os("ProgramFiles") {
            paths.push(PathBuf::from(program_files).join("nodejs"));
        }
    }
    let mut seen = HashSet::new();
    paths.retain(|path| path.is_absolute() && seen.insert(path.clone()));
    Ok(paths)
}

fn candidates(name: &str, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .flat_map(|directory| {
            if cfg!(windows) {
                vec![
                    directory.join(format!("{name}.exe")),
                    directory.join(format!("{name}.cmd")),
                    directory.join(format!("{name}.bat")),
                ]
            } else {
                vec![directory.join(name)]
            }
        })
        .filter(|path| path.is_file())
        .collect()
}

pub fn discover(
    app_data: &Path,
    manual_dsh: Option<&str>,
    cancel: &AtomicBool,
) -> Result<Discovery, String> {
    let mut paths = search_paths(cancel)?;
    paths.extend(crate::runtime_download::cached_bins(app_data));
    if let Some(value) = manual_dsh {
        let path = expand_path(value)?;
        paths.insert(
            0,
            path.parent()
                .expect("absolute executable path")
                .to_path_buf(),
        );
    }
    let mut result = Discovery {
        search_path: paths.clone(),
        ..Discovery::default()
    };
    for path in candidates("node", &paths) {
        match probe(&path, None, &paths, "Node.js", cancel) {
            Ok(node) => {
                result.node = Some(node);
                break;
            }
            Err(error) => {
                check_cancel(cancel)?;
                result.detail = Some(error);
            }
        }
    }
    if let Some(node) = &result.node {
        let mut tool_paths = vec![node.bin_dir.clone()];
        tool_paths.extend(paths.clone());
        let npm_tools: Vec<_> = candidates("npm", &tool_paths)
            .iter()
            .filter_map(|path| probe(path, Some(node), &tool_paths, "npm", cancel).ok())
            .collect();
        result.npm = npm_tools
            .iter()
            .find(|tool| npm_can_install(tool))
            .or(npm_tools.first())
            .cloned();
        result.pnpm = candidates("pnpm", &tool_paths)
            .iter()
            .find_map(|path| probe(path, Some(node), &tool_paths, "pnpm", cancel).ok());
        let commands = match manual_dsh {
            Some(value) => vec![expand_path(value)?],
            None => candidates("dsh", &paths),
        };
        for path in commands {
            if is_desktop_command(&path, app_data) {
                if manual_dsh.is_some() {
                    return Err(
                        "这是 DSH Desktop 自己的终端命令，请选择原来安装的 DSH。".to_string()
                    );
                }
                continue;
            }
            match probe(&path, Some(node), &tool_paths, "DSH", cancel) {
                Ok(dsh) => {
                    result.dsh = Some(dsh);
                    result.detail = None;
                    break;
                }
                Err(error) => {
                    check_cancel(cancel)?;
                    result.detail = Some(error);
                }
            }
        }
    }
    check_cancel(cancel)?;
    Ok(result)
}

fn is_desktop_command(path: &Path, app_data: &Path) -> bool {
    if path.starts_with(app_data) {
        return true;
    }
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    let _ = file.take(4096).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).contains("Managed by DSH Desktop")
}

pub fn validate(environment: &Environment, cancel: &AtomicBool) -> Result<Environment, String> {
    let paths = search_paths(cancel)?;
    let node = probe(&environment.node.path, None, &paths, "Node.js", cancel)?;
    let validate_tool = |tool: &Tool, name| -> Result<Tool, String> {
        let mut verified = probe(&tool.path, Some(&node), &paths, name, cancel)?;
        verified.bin_dir = tool.bin_dir.clone();
        Ok(verified)
    };
    Ok(Environment {
        source: environment.source,
        pnpm: validate_tool(&environment.pnpm, "pnpm")?,
        dsh: validate_tool(&environment.dsh, "DSH")?,
        npm: environment
            .npm
            .as_ref()
            .and_then(|tool| validate_tool(tool, "npm").ok()),
        node,
        search_path: paths,
    })
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| "配置目录无效。".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut stage = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    serde_json::to_writer_pretty(&mut stage, value).map_err(|error| error.to_string())?;
    stage.write_all(b"\n").map_err(|error| error.to_string())?;
    stage
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    stage
        .persist(path)
        .map_err(|error| format!("无法保存运行环境：{error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_supported_node_releases() {
        for version in ["22.19.0", "22.20.0", "24.0.0", "26.3.1"] {
            assert!(node_is_compatible(&Version::parse(version).unwrap()));
        }
        for version in ["20.19.0", "22.18.0", "23.9.0", "24.0.0-rc.1"] {
            assert!(!node_is_compatible(&Version::parse(version).unwrap()));
        }
    }
    #[test]
    fn rejects_relative_manual_paths() {
        assert!(expand_path("dsh").is_err());
    }
    #[test]
    fn never_treats_desktop_shims_as_user_installations() {
        let root = tempfile::tempdir().unwrap();
        let shim = root.path().join("dsh");
        fs::write(&shim, "#!/bin/sh\n# Managed by DSH Desktop\n").unwrap();
        assert!(is_desktop_command(&shim, Path::new("/different-app-data")));
    }
    #[test]
    fn replaces_saved_selection_atomically() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selection.json");
        write_json(&path, &vec!["local"]).unwrap();
        write_json(&path, &vec!["managed"]).unwrap();
        assert_eq!(
            serde_json::from_slice::<Vec<String>>(&fs::read(path).unwrap()).unwrap(),
            ["managed"]
        );
    }
    #[cfg(unix)]
    #[test]
    fn remembers_the_user_command_instead_of_its_package_store_target() {
        let temp = tempfile::tempdir().unwrap();
        let output = Command::new("node")
            .args(["-p", "process.execPath"])
            .output()
            .unwrap();
        let node_path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let node = probe(&node_path, None, &[], "Node.js", &AtomicBool::new(false)).unwrap();
        let entry = temp.path().join("one.mjs");
        fs::write(&entry, "console.log('1.0.0')").unwrap();
        let command = temp.path().join("dsh");
        std::os::unix::fs::symlink(&entry, &command).unwrap();
        let first = probe(&command, Some(&node), &[], "DSH", &AtomicBool::new(false)).unwrap();
        assert_eq!(first.path, command);
        let next = temp.path().join("two.mjs");
        fs::write(&next, "console.log('2.0.0')").unwrap();
        fs::remove_file(&command).unwrap();
        std::os::unix::fs::symlink(next, &command).unwrap();
        assert_eq!(
            probe(
                &first.path,
                Some(&node),
                &[],
                "DSH",
                &AtomicBool::new(false)
            )
            .unwrap()
            .version,
            "2.0.0"
        );
    }
    #[cfg(unix)]
    #[test]
    fn selected_package_manager_wins_over_other_shims_without_editing_them() {
        let temp = tempfile::tempdir().unwrap();
        let output = Command::new("node")
            .args(["-p", "process.execPath"])
            .output()
            .unwrap();
        let node_path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let node = probe(&node_path, None, &[], "Node.js", &AtomicBool::new(false)).unwrap();
        let user_tool = temp.path().join("my pnpm's entry.mjs");
        fs::write(&user_tool, "console.log('11.7.0')").unwrap();
        let tool = Tool {
            path: user_tool.clone(),
            bin_dir: temp.path().to_path_buf(),
            version: "11.7.0".into(),
        };
        let selected = Environment {
            source: RuntimeSource::Local,
            node: node.clone(),
            npm: None,
            pnpm: tool.clone(),
            dsh: tool,
            search_path: vec![],
        };
        let aliases = selected
            .create_tool_aliases(&temp.path().join("app"))
            .unwrap();
        let output = run_output(Command::new(&node.path).args(["-e", "console.log(require('node:child_process').execFileSync('pnpm',['--version'],{encoding:'utf8'}).trim())"]).env("PATH", join_path(&[aliases, node.bin_dir]).unwrap()), Duration::from_secs(5), &AtomicBool::new(false)).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "11.7.0");
        assert_eq!(
            fs::read_to_string(user_tool).unwrap(),
            "console.log('11.7.0')"
        );
    }
    #[cfg(unix)]
    #[test]
    fn bounds_commands_and_drains_large_output() {
        let cancel = AtomicBool::new(false);
        let output = run_output(
            Command::new("/bin/sh").args([
                "-c",
                "i=0; while [ $i -lt 20000 ]; do echo output-output-output; i=$((i+1)); done",
            ]),
            Duration::from_secs(5),
            &cancel,
        )
        .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.len() <= 128 * 1024);
        let start = Instant::now();
        assert!(run_output(
            Command::new("/bin/sleep").arg("10"),
            Duration::from_millis(80),
            &cancel
        )
        .is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
