//! Обновяване на програмата от последния GitHub Release.
//!
//! Новият exe се сваля до текущия като `.new`; работещият се преименува на `.old`
//! (Windows позволява преименуване на работещ exe), новият заема мястото му и се пуска.

use serde_json::Value;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

/// GitHub хранилището с версиите (owner/repo).
pub const REPO: &str = "shtefko55/pp-downloader";
pub const ASSET: &str = "PP_Downloader.exe";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available { version: String, url: String, notes: String },
    Downloading,
    /// Новата версия е на мястото си — програмата се рестартира с нея.
    Ready(PathBuf),
    Failed(String),
}

fn curl() -> Command {
    let mut cmd = Command::new("curl.exe");
    cmd.creation_flags(CREATE_NO_WINDOW).args(["-sSfL", "--max-time", "300", "-A", "PP-Downloader"]);
    cmd
}

/// Последната версия в GitHub, ако е по-нова от текущата.
pub fn check() -> Result<UpdateState, String> {
    let mut cmd = curl();
    cmd.args(["-H", "Accept: application/vnd.github+json"]).arg(format!("https://api.github.com/repos/{REPO}/releases/latest"));
    let json = crate::worker::run_capture(cmd, &AtomicBool::new(false)).map_err(|e| format!("Няма връзка с GitHub: {e}"))?;
    let release: Value = serde_json::from_str(&json).map_err(|e| format!("Неразбираем отговор от GitHub: {e}"))?;
    let tag = release["tag_name"].as_str().ok_or("В GitHub няма публикувана версия")?;
    let version = tag.trim_start_matches('v').to_string();
    if !is_newer(&version, VERSION) {
        return Ok(UpdateState::UpToDate);
    }
    let url = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(ASSET))
        .and_then(|a| a["browser_download_url"].as_str())
        .ok_or(format!("Версия {version} няма файл {ASSET}"))?
        .to_string();
    let notes = release["body"].as_str().unwrap_or("").trim().to_string();
    Ok(UpdateState::Available { version, url, notes })
}

/// Сваля новия exe и го слага на мястото на текущия. След това програмата трябва да се рестартира.
pub fn install(url: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let new = with_suffix(&exe, ".new");
    let old = with_suffix(&exe, ".old");
    let mut cmd = curl();
    cmd.arg("-o").arg(&new).arg(url);
    crate::worker::run_capture(cmd, &AtomicBool::new(false)).map_err(|e| format!("Изтеглянето не успя: {e}"))?;

    let head = std::fs::read(&new).map_err(|e| e.to_string())?;
    if head.len() < 1_000_000 || !head.starts_with(b"MZ") {
        let _ = std::fs::remove_file(&new);
        return Err("Изтегленият файл не е валидна програма".into());
    }
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("Текущата версия не може да се премести: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("Новата версия не може да се постави: {e}"));
    }
    Ok(exe)
}

/// Пуска новата версия (самата програма се затваря след това).
pub fn restart(exe: &Path) {
    let _ = Command::new(exe).spawn();
}

/// Изтрива остатъка от предишно обновяване.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(with_suffix(&exe, ".old"));
    }
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// "0.10.0" > "0.9.3"
fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| v.split('.').map(|p| p.trim().parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parse(candidate) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn versions() {
        assert!(is_newer("0.10.0", "0.9.3"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
    }
}
