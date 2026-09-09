//! Installs missing tools into app-owned directories from pinned official archives.

use std::{
    fs::{self, File},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256, Sha512};

use crate::runtime_environment::{self as environment, Discovery, Tool};

pub const NODE_VERSION: &str = "24.19.0";
pub const NPM_VERSION: &str = "11.19.0";
pub const PNPM_VERSION: &str = "11.7.0";
const NPM_INTEGRITY: &str =
    "SDd/hHg3KqHE5Ht2NHWxNYNtqCQ2pXAPLl6OtQhPyED5PHsRfrOtO199MZTIG2cQoQ1ZRI9t28shrD+2cr3AAw==";
const PNPM_INTEGRITY: &str =
    "GcyFLBIMcSV2DyRD7mvgyltA+fUFmN4aCaHxd1A+AQ5Xwjx3ZG4B52HeWb+HT7IqM5jDOrlpH8E+uUa28PTWIA==";

pub fn cached_bins(app_data: &Path) -> Vec<PathBuf> {
    let root = app_data.join("runtime/toolchains");
    vec![
        root.join("node")
            .join(if cfg!(windows) { "" } else { "bin" }),
        root.join("npm/.bin"),
        root.join("pnpm/.bin"),
    ]
}

pub fn prepare_tools(
    discovery: &mut Discovery,
    app_data: &Path,
    cancel: &AtomicBool,
    report: &impl Fn(&str, &str, &Discovery),
) -> Result<(), String> {
    let root = app_data.join("runtime/toolchains");
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    if discovery.node.is_none() {
        report("downloading", "正在下载 Node.js…", discovery);
        discovery.node = Some(install_node(&root, &discovery.search_path, cancel)?);
    }
    let node = discovery.node.as_ref().expect("prepared Node");
    let mut paths = vec![node.bin_dir.clone()];
    paths.extend(discovery.search_path.clone());
    paths.extend(cached_bins(app_data));
    if !discovery
        .npm
        .as_ref()
        .is_some_and(environment::npm_can_install)
    {
        report("downloading", "正在准备安装工具 npm…", discovery);
        discovery.npm = Some(install_package_tool(
            "npm",
            NPM_VERSION,
            NPM_INTEGRITY,
            "bin/npm-cli.js",
            &root,
            node,
            &paths,
            cancel,
        )?);
    }
    if discovery.pnpm.is_none() {
        report("downloading", "正在准备插件安装工具 pnpm…", discovery);
        discovery.pnpm = Some(install_package_tool(
            "pnpm",
            PNPM_VERSION,
            PNPM_INTEGRITY,
            "bin/pnpm.mjs",
            &root,
            node,
            &paths,
            cancel,
        )?);
    }
    discovery.search_path = paths;
    environment::check_cancel(cancel)
}

fn node_target() -> Result<(&'static str, &'static str), String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok((
            "darwin-arm64.tar.gz",
            "8294b7aa9b03997481c06babf1e8b270c859358f27da57a11509afe537ac381d",
        )),
        ("macos", "x86_64") => Ok((
            "darwin-x64.tar.gz",
            "d1b5e999db158c62fe8f7267a4476b035d8bd93b1a605bac24a3f0dd166e3316",
        )),
        ("windows", "x86_64") => Ok((
            "win-x64.zip",
            "57f71ab3652e797d84acddc79c81cc9ff1c6ddb2a1974cdb83f00fee9bff4c73",
        )),
        ("windows", "aarch64") => Ok((
            "win-arm64.zip",
            "8502f4a50b458d4cc38ed8f2001556c2cd239d464920f74017926ccb1e1c157f",
        )),
        ("linux", "x86_64") => Ok((
            "linux-x64.tar.gz",
            "f625d97cd707df4ff96254916fbc5ff014f09c09effe5a1e0ca8f6d41a8789d4",
        )),
        ("linux", "aarch64") => Ok((
            "linux-arm64.tar.gz",
            "d28c8a5bf0a808f0ed434a1dce8c54ae98f0371c0bd86ac58abc613f73e6643f",
        )),
        _ => Err("当前平台还不支持自动下载 Node.js，请先安装兼容版本后重新检测。".to_string()),
    }
}

fn install_node(root: &Path, paths: &[PathBuf], cancel: &AtomicBool) -> Result<Tool, String> {
    let relative = if cfg!(windows) {
        "node.exe"
    } else {
        "bin/node"
    };
    let destination = root.join("node");
    if let Ok(tool) =
        environment::probe(&destination.join(relative), None, paths, "Node.js", cancel)
    {
        return Ok(tool);
    }
    environment::check_cancel(cancel)?;
    let (target, checksum) = node_target()?;
    let archive_name = format!("node-v{NODE_VERSION}-{target}");
    let folder = archive_name
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".zip");
    let stage = tempfile::tempdir_in(root).map_err(|error| error.to_string())?;
    let archive = stage.path().join(&archive_name);
    download(
        &format!("https://nodejs.org/dist/v{NODE_VERSION}/{archive_name}"),
        &archive,
        cancel,
    )?;
    verify_sha256(&archive, checksum)?;
    unpack(
        &archive,
        stage.path(),
        folder,
        target.ends_with(".zip"),
        cancel,
    )?;
    let extracted = stage.path().join(folder);
    let tool = environment::probe(&extracted.join(relative), None, paths, "Node.js", cancel)?;
    if tool.version != NODE_VERSION {
        return Err("Node.js 下载版本与预期不符，已停止安装。".to_string());
    }
    environment::check_cancel(cancel)?;
    commit_directory(&extracted, &destination)?;
    environment::probe(&destination.join(relative), None, paths, "Node.js", cancel)
}

#[allow(clippy::too_many_arguments)]
fn install_package_tool(
    name: &str,
    version: &str,
    integrity: &str,
    entry: &str,
    root: &Path,
    node: &Tool,
    paths: &[PathBuf],
    cancel: &AtomicBool,
) -> Result<Tool, String> {
    let destination = root.join(name);
    if let Ok(mut tool) =
        environment::probe(&destination.join(entry), Some(node), paths, name, cancel)
    {
        if tool.version == version {
            install_aliases(&destination, name, entry)?;
            tool.bin_dir = destination.join(".bin");
            return Ok(tool);
        }
    }
    environment::check_cancel(cancel)?;
    let stage = tempfile::tempdir_in(root).map_err(|error| error.to_string())?;
    let archive = stage.path().join("package.tgz");
    download(
        &format!("https://registry.npmjs.org/{name}/-/{name}-{version}.tgz"),
        &archive,
        cancel,
    )?;
    verify_sha512(&archive, integrity)?;
    unpack(&archive, stage.path(), "package", false, cancel)?;
    let extracted = stage.path().join("package");
    let tool = environment::probe(&extracted.join(entry), Some(node), paths, name, cancel)?;
    if tool.version != version {
        return Err(format!("{name} 下载版本与预期不符，已停止安装。"));
    }
    install_aliases(&extracted, name, entry)?;
    environment::check_cancel(cancel)?;
    commit_directory(&extracted, &destination)?;
    let mut tool = environment::probe(&destination.join(entry), Some(node), paths, name, cancel)?;
    tool.bin_dir = destination.join(".bin");
    Ok(tool)
}

fn install_aliases(root: &Path, name: &str, entry: &str) -> Result<(), String> {
    let bin = root.join(".bin");
    fs::create_dir_all(&bin).map_err(|error| error.to_string())?;
    let mut entries = vec![(name, entry)];
    if name == "npm" {
        entries.push(("npx", "bin/npx-cli.js"));
    }
    if name == "pnpm" {
        entries.push(("pnpx", "bin/pnpx.mjs"));
    }
    for (command, entry) in entries {
        #[cfg(unix)]
        {
            let path = bin.join(command);
            if fs::symlink_metadata(&path).is_err() {
                std::os::unix::fs::symlink(Path::new("..").join(entry), path)
                    .map_err(|error| error.to_string())?;
            }
        }
        #[cfg(windows)]
        fs::write(
            bin.join(format!("{command}.cmd")),
            format!("@\"node\" \"%~dp0..\\{}\" %*\r\n", entry.replace('/', "\\")),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn commit_directory(stage: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let backup = destination.with_extension(format!("replaced-{stamp}"));
        fs::rename(destination, backup)
            .map_err(|error| format!("无法保留旧的应用组件：{error}"))?;
    }
    fs::rename(stage, destination).map_err(|error| format!("无法保存下载的组件：{error}"))
}

fn download(url: &str, path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    let curl = if cfg!(windows) {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32/curl.exe")
    } else {
        PathBuf::from("/usr/bin/curl")
    };
    let mut command = Command::new(curl);
    command
        .args([
            "--fail",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "15",
            "--max-time",
            "600",
            "--retry",
            "1",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(path)
        .args(["--url", url]);
    let output = environment::run_output(&mut command, Duration::from_secs(630), cancel)?;
    if !output.status.success() {
        return Err(format!(
            "下载失败，请检查网络后重试。{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    environment::check_cancel(cancel)
}

pub fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).map_err(|error| error.to_string())?;
    if format!("{:x}", hash.finalize()) != expected {
        return Err("下载文件未通过 SHA-256 校验，已停止安装。".to_string());
    }
    Ok(())
}

fn verify_sha512(path: &Path, expected: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hash = Sha512::new();
    std::io::copy(&mut file, &mut hash).map_err(|error| error.to_string())?;
    if STANDARD.encode(hash.finalize()) != expected {
        return Err("安装工具未通过 SHA-512 校验，已停止安装。".to_string());
    }
    Ok(())
}

fn archive_path_is_safe(path: &Path, prefix: &str) -> bool {
    let mut parts = path.components();
    matches!(parts.next(), Some(Component::Normal(name)) if name == prefix)
        && parts.all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
}

fn unpack(
    archive: &Path,
    root: &Path,
    prefix: &str,
    zip: bool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut total = 0_u64;
    let file = File::open(archive).map_err(|error| error.to_string())?;
    if zip {
        let mut archive = zip::ZipArchive::new(file).map_err(|error| error.to_string())?;
        for index in 0..archive.len() {
            environment::check_cancel(cancel)?;
            let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
            let path = entry
                .enclosed_name()
                .ok_or_else(|| "下载包包含无效路径。".to_string())?;
            if !archive_path_is_safe(&path, prefix)
                || entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                return Err("下载包包含不安全的路径。".to_string());
            }
            total += entry.size();
            if total > 800 * 1024 * 1024 {
                return Err("下载包展开大小超出限制。".to_string());
            }
            let destination = root.join(path);
            if entry.is_dir() {
                fs::create_dir_all(destination).map_err(|error| error.to_string())?;
            } else {
                fs::create_dir_all(destination.parent().expect("archive entry parent"))
                    .map_err(|error| error.to_string())?;
                let mut file = File::create(destination).map_err(|error| error.to_string())?;
                std::io::copy(&mut entry, &mut file).map_err(|error| error.to_string())?;
            }
        }
    } else {
        let mut archive = tar::Archive::new(GzDecoder::new(file));
        for entry in archive.entries().map_err(|error| error.to_string())? {
            environment::check_cancel(cancel)?;
            let mut entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path().map_err(|error| error.to_string())?;
            if !archive_path_is_safe(&path, prefix) {
                return Err("下载包包含不安全的路径。".to_string());
            }
            total += entry.size();
            if total > 800 * 1024 * 1024 {
                return Err("下载包展开大小超出限制。".to_string());
            }
            if !entry.unpack_in(root).map_err(|error| error.to_string())? {
                return Err("下载包试图写入安装目录之外。".to_string());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn rejects_checksum_mismatches_and_escaping_paths() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"abc").unwrap();
        verify_sha256(
            file.path(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .unwrap();
        assert!(verify_sha256(file.path(), "wrong").is_err());
        assert!(verify_sha512(file.path(), "wrong").is_err());
        for path in ["/package/file", "package/../escape", "elsewhere/file"] {
            assert!(!archive_path_is_safe(Path::new(path), "package"));
        }
        assert!(archive_path_is_safe(
            Path::new("package/bin/npm-cli.js"),
            "package"
        ));
    }
    #[test]
    fn interrupted_download_preserves_existing_components() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("node");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("user-data"), "retained").unwrap();
        let result = install_node(root.path(), &[], &AtomicBool::new(true));
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(old.join("user-data")).unwrap(),
            "retained"
        );
    }
}
