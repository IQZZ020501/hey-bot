//! 本地会话存储。
//!
//! 凭据只落本地文件，权限 0600，不进仓库、不打日志。
//!
//! 路径解析顺序：
//! 1. `HEYBOT_SESSION_FILE` / `HEYBOT_DEDUP_FILE` 显式指定。
//! 2. 项目根目录（当前工作目录，或可执行文件所在目录）。
//! 3. 用户配置目录 `~/.config/heybot/`（找不到项目目录时的兜底）。

use std::path::{Path, PathBuf};

use crate::api::Session;
use crate::error::{Error, Result};

/// 项目根目录。
///
/// 依次尝试：
/// 1. `HEYBOT_HOME` 显式指定。
/// 2. 当前工作目录，或它向上的目录（找到 `Cargo.toml` 为止）。
/// 3. 可执行文件所在目录。
/// 4. 兜底：用户配置目录 `~/.config/heybot/`。
pub fn project_root() -> PathBuf {
    if let Ok(explicit) = std::env::var("HEYBOT_HOME")
        && !explicit.trim().is_empty()
    {
        return PathBuf::from(explicit);
    }
    if let Ok(current) = std::env::current_dir() {
        if let Some(root) = find_root_upwards(&current) {
            return root;
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        return dir.to_path_buf();
    }
    // 兜底：用户配置目录。
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("heybot")
}

/// 从给定目录向上找带 `Cargo.toml` 的项目根。
fn find_root_upwards(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join("Cargo.toml").is_file() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// 会话文件路径。
pub fn session_path() -> PathBuf {
    if let Ok(path) = std::env::var("HEYBOT_SESSION_FILE")
        && !path.trim().is_empty()
    {
        return PathBuf::from(path);
    }
    project_root().join("session.json")
}

/// 去重记录路径。
pub fn dedup_path() -> PathBuf {
    if let Ok(path) = std::env::var("HEYBOT_DEDUP_FILE")
        && !path.trim().is_empty()
    {
        return PathBuf::from(path);
    }
    project_root().join("replied.txt")
}

/// 水位线文件路径。
///
/// 水位线落盘后，bot 重启（或崩溃、换机）期间到达的 @
/// 仍然会被补上——否则它们会被当成历史消息永久跳过。
pub fn watermark_path() -> PathBuf {
    if let Ok(path) = std::env::var("HEYBOT_WATERMARK_FILE")
        && !path.trim().is_empty()
    {
        return PathBuf::from(path);
    }
    project_root().join("watermark.txt")
}

/// 写水位线。
pub fn save_watermark(path: &Path, value: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::Storage(error.to_string()))?;
    }
    std::fs::write(path, format!("{value}\n"))
        .map_err(|error| Error::Storage(error.to_string()))?;
    restrict(path);
    Ok(())
}

/// 读水位线。文件不存在时返回 None。
pub fn load_watermark(path: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(path).ok()?;
    content.trim().parse::<u64>().ok()
}

/// 保存会话。
pub fn save_session(path: &Path, session: &Session) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::Storage(error.to_string()))?;
    }
    let json = serde_json::to_string_pretty(session)
        .map_err(|error| Error::Storage(error.to_string()))?;
    std::fs::write(path, json).map_err(|error| Error::Storage(error.to_string()))?;
    restrict(path);
    Ok(())
}

/// 读取会话。
///
/// 文件不存在、为空或损坏时都返回**空会话**（当作未登录），
/// 而不是报错——这样上层能直接走扫码登录，而不是崩在解析失败上。
pub fn load_session(path: &Path) -> Result<Session> {
    if !path.exists() {
        return Ok(Session::default());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|error| Error::Storage(error.to_string()))?;
    if content.trim().is_empty() {
        return Ok(Session::default());
    }
    match serde_json::from_str(&content) {
        Ok(session) => Ok(session),
        Err(error) => {
            tracing::warn!(
                "会话文件 {} 损坏（{error}），按未登录处理。",
                path.display()
            );
            Ok(Session::default())
        }
    }
}

/// 把文件权限收紧到 0600。
pub fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Cookie;

    #[test]
    fn corrupt_session_is_treated_as_not_logged_in() {
        // 损坏的会话文件不能让程序崩掉：应当按未登录处理，交给上层扫码。
        let path = std::env::temp_dir().join(format!("heybot-session-{}.json", std::process::id()));
        std::fs::write(&path, "corrupted").expect("写文件");
        let session = load_session(&path).expect("不应报错");
        assert!(session.cookies.is_empty(), "损坏文件应解析为空会话");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_session_is_empty() {
        let path = std::env::temp_dir().join("heybot-nonexistent-session.json");
        let _ = std::fs::remove_file(&path);
        let session = load_session(&path).expect("不应报错");
        assert!(session.cookies.is_empty());
    }

    #[test]
    fn session_round_trips() {
        let path = std::env::temp_dir().join(format!("heybot-rt-{}.json", std::process::id()));
        let session = Session {
            cookies: vec![Cookie {
                name: "heybox_id".into(),
                value: "12345".into(),
                domain: ".xiaoheihe.cn".into(),
                path: "/".into(),
            }],
        };
        save_session(&path, &session).expect("保存");
        let loaded = load_session(&path).expect("读取");
        assert_eq!(loaded.cookies.len(), 1);
        assert_eq!(loaded.cookies[0].value, "12345");
        let _ = std::fs::remove_file(&path);
    }
}
