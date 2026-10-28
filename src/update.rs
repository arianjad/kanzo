//! Updating Kanzo from the board: find the latest release, run its installer over this
//! program, then start the new one in place. It uses `curl` and `sh`, which macOS and
//! Linux have, so Kanzo needs no HTTP client of its own.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Where releases are published; `KANZO_RELEASES_URL` points elsewhere, for tests.
const RELEASES: &str = "https://github.com/emanuele-em/kanzo/releases/latest/download";

/// How long a check for the latest version is reused: the board checks once a day.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

static LATEST: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);

fn releases() -> String {
    std::env::var("KANZO_RELEASES_URL").unwrap_or_else(|_| RELEASES.into())
}

fn curl(url: &str) -> Option<Vec<u8>> {
    let out = Command::new("curl").args(["-fsSL", "--max-time", "20", url]).output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// The latest released version, checked at most once a day unless `now`; `None` when
/// it can't be found, like offline.
pub fn latest(now: bool) -> Option<String> {
    let mut cached = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, version)) = &*cached
        && !now
        && at.elapsed() < CHECK_EVERY
    {
        return version.clone();
    }
    let version = curl(&format!("{}/dist-manifest.json", releases())).and_then(|body| {
        let manifest: serde_json::Value = serde_json::from_slice(&body).ok()?;
        let release = manifest["releases"].as_array()?.iter().find(|r| r["app_name"] == "kanzo")?;
        release["app_version"].as_str().map(String::from)
    });
    *cached = Some((Instant::now(), version.clone()));
    version
}

/// True when `version` is newer than this program: `0.2.0` > `0.1.9`.
pub fn newer(version: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect() };
    parts(version) > parts(CURRENT)
}

/// Runs the latest release's installer over this program and returns its path,
/// ready to restart.
pub fn install() -> Result<PathBuf, String> {
    // ponytail: releases ship no Windows build; the banner still says when one is out.
    if cfg!(windows) {
        return Err("There's no Windows release to install: build the new version from source".into());
    }
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).map_err(|e| e.to_string())?;
    // The installer writes to <dir>/bin, so it can only replace a program kept in a bin folder.
    let dir = match exe.parent() {
        Some(bin) if bin.file_name().is_some_and(|n| n == "bin") => bin.parent().unwrap_or(bin).to_path_buf(),
        _ => {
            return Err(format!(
                "Kanzo runs from {}, which the installer can't update; reinstall it with the command in the README",
                exe.display()
            ));
        }
    };
    let script = curl(&format!("{}/kanzo-installer.sh", releases())).ok_or("cannot download the installer")?;
    let mut sh = Command::new("sh")
        .arg("-s")
        .env("KANZO_INSTALL_DIR", &dir)
        .env("KANZO_NO_MODIFY_PATH", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run the installer: {e}"))?;
    sh.stdin.take().expect("stdin is piped").write_all(&script).map_err(|e| e.to_string())?;
    let out = sh.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let log = String::from_utf8_lossy(&out.stderr);
        let last = log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
        return Err(format!("the installer failed: {}", last.trim()));
    }
    Ok(exe)
}

/// The arguments to start again with: the same ones, without opening another browser tab.
#[cfg_attr(not(unix), allow(dead_code))] // only `restart` on Unix uses it
fn restart_args(args: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<OsString> = args.collect();
    if !args.iter().any(|a| a == "--no-open") {
        args.push("--no-open".into());
    }
    args
}

/// Replaces this process with the program at `exe`: same arguments, same process, so
/// the port, the data folder and a running service stay. Returns only on failure.
#[cfg(unix)]
pub fn restart(exe: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    Command::new(exe).args(restart_args(std::env::args_os().skip(1))).exec()
}

#[cfg(not(unix))]
pub fn restart(_exe: &Path) -> std::io::Error {
    std::io::Error::other("restart Kanzo to finish the update")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("99.0.0"));
        assert!(!newer(CURRENT));
        assert!(!newer("0.0.1"));
        assert!(!newer("not a version"));
    }

    #[test]
    fn restarts_without_opening_a_tab() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(restart_args(args(&["--port", "4300"]).into_iter()), args(&["--port", "4300", "--no-open"]));
        assert_eq!(restart_args(args(&["--no-open"]).into_iter()), args(&["--no-open"]));
    }
}
