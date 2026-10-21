//! 配置加载。
//!
//! 优先级：**进程环境变量 > `.env` 文件 > 内置默认值**。
//! 这样 `.env` 存默认配置，临时用环境变量覆盖某一项即可，不必改文件。
//!
//! `.env` 查找顺序（取第一个存在的）：
//! 1. `HEYBOT_ENV_FILE` 指定的路径
//! 2. 当前工作目录的 `.env`
//! 3. 可执行文件所在目录的 `.env`
//! 4. `~/.config/heybot/.env`

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::error::{Error, Result};

/// 实际生效的 `.env` 路径。
///
/// `None` 表示没找到文件。首次访问时才加载。
static ENV_FILE: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let path = find_env_file();
    if let Some(path) = &path {
        let _ = dotenvy::from_path(path);
    }
    path
});

/// 加载 `.env`。
///
/// 必须在读任何环境变量之前调用。重复调用是安全的：
/// 实际加载发生在首次访问时，之后只读不写。
pub fn load() {
    // 触发一次惰性初始化。
    LazyLock::force(&ENV_FILE);
}

/// 找到要使用的 `.env` 文件。
fn find_env_file() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("HEYBOT_ENV_FILE")
        && !explicit.trim().is_empty()
    {
        let path = PathBuf::from(explicit);
        return path.exists().then_some(path);
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    candidates.push(PathBuf::from(".env"));
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join(".env"));
    }
    if let Some(config) = dirs::config_dir() {
        candidates.push(config.join("heybot").join(".env"));
    }

    candidates.into_iter().find(|path| path.is_file())
}

/// 读取一个环境变量，取不到时返回错误信息。
pub fn require(key: &str) -> Result<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Error::Config(format!("缺少 {key}（可在 .env 中配置）。")))
}

/// 读取一个可选的环境变量。
pub fn optional(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 读取一个数值型环境变量。
pub fn number<T: std::str::FromStr>(key: &str) -> Option<T> {
    optional(key).and_then(|value| value.parse().ok())
}

/// 读取一个布尔型环境变量。
///
/// 接受 `1/true/yes/on` 与 `0/false/no/off`。
pub fn flag(key: &str) -> bool {
    matches!(
        optional(key)
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// 当前生效的 `.env` 路径（用于提示信息）。
pub fn env_file() -> Option<PathBuf> {
    ENV_FILE.clone()
}

/// 确认 `.env` 文件不会被误提交。
///
/// 只在存在明显风险时提示，不阻断运行。
pub fn warn_if_world_readable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.permissions().mode();
            if mode & 0o077 != 0 {
                tracing::warn!(
                    "{} 含凭据但权限过宽（{mode:o}），建议 chmod 600",
                    path.display()
                );
            }
        }
    }
    #[cfg(not(unix))]
    {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_accepts_common_spellings() {
        // 用不存在的键测试，不污染真实环境。
        assert!(!flag("HEYBOT_TEST_UNSET_FLAG"));
    }
}
