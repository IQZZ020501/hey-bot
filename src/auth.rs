//! 扫码登录。
//!
//! 真实契约（2026-10 实测）：
//! - `GET /account/get_qrcode_url/?app=web`
//!   → `{"result":{"qr_url":".../account/qr_login/?qr=<id>&app=heybox","expire":120}}`
//!   二维码 id **不单独返回**，藏在 `qr_url` 的 `qr=` 参数里。
//! - `GET /account/qr_state/?app=web&qr=<id>`
//!   → `{"result":{"error":"wait|ready|ok|failed","error_msg":"..."}}`
//!   状态在 `result.error`，**参数名是 `qr` 而不是 `qr_id`**。
//!
//! 凭据只落在本地会话文件，不进仓库、不打日志。

use std::time::Duration;

use serde_json::Value;

use crate::api::{Cookie, HeyboxApi, Session};
use crate::error::{Error, Result};

const QR_URL_PATH: &str = "/account/get_qrcode_url/";
const QR_STATE_PATH: &str = "/account/qr_state/";
const RESTORE_PATH: &str = "/account/restore_login";

/// 一个待扫描的二维码。
#[derive(Debug, Clone)]
pub struct QrCode {
    /// 二维码内容，即 `qr_url`。
    pub url: String,
    /// 从 `qr_url` 里解析出的二维码 id。
    pub qr_id: String,
    /// 有效期（秒）。
    pub expire_seconds: i64,
}

/// 扫码状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrState {
    /// 等待扫描。
    Wait,
    /// 已扫描，等待手机确认。
    Ready,
    /// 已确认。
    Confirmed,
    /// 二维码失效。
    Failed,
    /// 需要二次验证。
    NeedVerify,
}

/// 登录结果。
#[derive(Debug, Clone)]
pub struct LoginResult {
    pub session: Session,
    pub user_id: String,
}

/// 取二维码。
pub async fn fetch_qrcode(api: &HeyboxApi) -> Result<QrCode> {
    let data = api.get(QR_URL_PATH, &[("app", "web".into())]).await?;
    let url = data
        .pointer("/result/qr_url")
        .or_else(|| data.pointer("/result/qrcode_url"))
        .or_else(|| data.pointer("/result/url"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::Protocol(format!(
                "响应里没有二维码地址，原始响应：{}",
                data.to_string().chars().take(300).collect::<String>()
            ))
        })?
        .to_string();
    let expire = data
        .pointer("/result/expire")
        .and_then(Value::as_i64)
        .unwrap_or(120);
    let qr_id = extract_qr_id(&url);
    Ok(QrCode {
        url,
        qr_id,
        expire_seconds: expire,
    })
}

/// 从二维码地址里取出 `qr` 参数。
pub fn extract_qr_id(url: &str) -> String {
    if let Some(index) = url.find("qr=") {
        let rest = &url[index + 3..];
        let end = rest.find(['&', '#']).unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    url.to_string()
}

/// 轮询一次扫码状态。
pub async fn poll_qr_state(api: &HeyboxApi, qr_id: &str) -> Result<(QrState, Value)> {
    let data = api
        .get(
            QR_STATE_PATH,
            &[
                // 参数名是 `qr`，不是 `qr_id`。
                ("qr", qr_id.to_string()),
                ("app", "web".into()),
            ],
        )
        .await?;

    let result = data.get("result").cloned().unwrap_or(Value::Null);
    let state = result.get("error").and_then(Value::as_str).unwrap_or("");
    let message = result
        .get("error_msg")
        .and_then(Value::as_str)
        .unwrap_or("");

    let state = match state {
        "wait" => QrState::Wait,
        "ready" => QrState::Ready,
        "ok" => QrState::Confirmed,
        // `cancel` 是二维码过期，`failed` 是参数错误。实测：
        // 过期 -> {"error":"cancel","error_msg":"二维码已过期"}
        // 参数错误 -> {"error":"failed","error_msg":"参数错误"}
        "cancel" | "expired" | "expire" => QrState::Failed,
        "failed" => {
            if message.contains("验证") || message.contains("captcha") {
                QrState::NeedVerify
            } else {
                QrState::Failed
            }
        }
        // 未知状态按"继续等待"处理，避免因为上游新增状态而中断登录。
        _ => QrState::Wait,
    };
    Ok((state, result))
}

/// 从登录响应里取账号标识。
///
/// 依次尝试 `result.profile.heybox_id`、`result.account_detail.userid`、
/// `result.heybox_id`。传入的可以是完整响应，也可以是 `result` 本身。
pub fn read_user_id(response: &Value) -> Option<String> {
    let candidates = [
        response.pointer("/result/profile/heybox_id"),
        response.pointer("/result/account_detail/userid"),
        response.pointer("/result/heybox_id"),
        response.pointer("/profile/heybox_id"),
        response.pointer("/account_detail/userid"),
        response.get("heybox_id"),
    ];
    for value in candidates.into_iter().flatten() {
        // 0 与 "0" 都表示没有有效身份。
        if value.is_null() || value == 0 || value == "0" || value.is_boolean() {
            continue;
        }
        let text = match value {
            Value::String(text) => text.trim().to_string(),
            Value::Number(number) => number.to_string(),
            _ => continue,
        };
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// 归一化 Cookie：把身份与 pkey 显式写成 Cookie。
///
/// 登录响应里的 `heybox_id` / `pkey` 是后续请求必需的凭据，
/// 但不一定以 Cookie 形式下发，必须显式补上。
pub fn normalize_cookies(session: &Session, result: &Value) -> Session {
    let mut cookies: Vec<Cookie> = session
        .cookies
        .iter()
        .filter(|cookie| {
            let domain = cookie.domain.trim_start_matches('.');
            matches!(
                domain,
                "xiaoheihe.cn" | "api.xiaoheihe.cn" | "www.xiaoheihe.cn"
            )
        })
        .cloned()
        .collect();

    let named = |name: &str| -> Option<String> {
        cookies
            .iter()
            .find(|cookie| cookie.name == name)
            .map(|cookie| cookie.value.clone())
            .filter(|value| !value.is_empty())
    };

    let user = result
        .get("heybox_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| named("user_heybox_id"))
        .or_else(|| named("heybox_id"))
        .unwrap_or_default();
    let pkey = result
        .get("pkey")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| named("user_pkey"))
        .or_else(|| named("pkey"));

    let aliases = [
        ("user_heybox_id", Some(user.clone())),
        ("heybox_id", Some(user)),
        ("user_pkey", pkey.clone()),
        ("pkey", pkey),
    ];

    cookies.retain(|cookie| {
        !aliases
            .iter()
            .any(|(name, _)| *name == cookie.name.as_str())
    });
    for (name, value) in aliases {
        if let Some(value) = value
            && !value.is_empty()
        {
            cookies.push(Cookie {
                name: name.to_string(),
                value,
                domain: ".xiaoheihe.cn".into(),
                path: "/".into(),
            });
        }
    }

    Session { cookies }
}

/// 等待扫码完成。
pub async fn wait_for_login(api: &HeyboxApi, qr: &QrCode) -> Result<LoginResult> {
    let timeout = Duration::from_secs(qr.expire_seconds.max(60) as u64 + 30);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(Error::Protocol("等待扫码超时。".into()));
        }
        let (state, result) = poll_qr_state(api, &qr.qr_id).await?;
        match state {
            QrState::Wait | QrState::Ready => {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            QrState::Confirmed => {
                // 身份在 qr_state 的响应里。
                let user_id = read_user_id(&result)
                    .ok_or_else(|| Error::Protocol("登录响应里没有账号标识。".into()))?;
                // Cookie 从 jar 收集，并补上身份与 pkey。
                let raw = api.session().await;
                let session = normalize_cookies(&raw, &result);
                if session.cookies.is_empty() {
                    return Err(Error::Protocol(
                        "扫码已确认，但没有拿到会话 Cookie。".into(),
                    ));
                }
                // 用独立客户端验证身份，确保落盘的会话真的可用。
                let verified = HeyboxApi::new(session.clone())?;
                let confirmed = verify_session(&verified).await?;
                if confirmed != user_id {
                    return Err(Error::Protocol(format!(
                        "登录身份不一致：期望 {user_id}，实际 {confirmed}。"
                    )));
                }
                return Ok(LoginResult { session, user_id });
            }
            QrState::NeedVerify => return Err(Error::Captcha),
            QrState::Failed => {
                return Err(Error::Protocol(
                    "二维码已失效或状态异常，请重新执行 heybot login。".into(),
                ));
            }
        }
    }
}

/// 校验会话是否仍然有效。
pub async fn verify_session(api: &HeyboxApi) -> Result<String> {
    let data = api.get(RESTORE_PATH, &[]).await?;
    // 身份在 result.profile.heybox_id。
    read_user_id(&data).ok_or_else(|| {
        Error::Protocol(format!(
            "会话校验失败：响应里没有账号标识，原始响应：{}",
            data.to_string().chars().take(400).collect::<String>()
        ))
    })
}

/// 生成二维码图片（SVG）。
pub fn qr_svg(content: &str) -> Result<String> {
    let code = qrcode::QrCode::new(content.as_bytes())
        .map_err(|error| Error::Protocol(format!("二维码生成失败：{error}")))?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .build())
}

/// 在终端里画出二维码。
///
/// 用半块字符（▀▄█）拼两行一个像素，扫起来比 ASCII 码可靠得多。
/// 终端宽度不足 45 列时调用方应退回 SVG 文件。
pub fn qr_terminal(content: &str) -> Result<String> {
    let code = qrcode::QrCode::new(content.as_bytes())
        .map_err(|error| Error::Protocol(format!("二维码生成失败：{error}")))?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .min_dimensions(40, 40)
        .build())
}

/// 从浏览器 Cookie 数组构造会话。
pub fn session_from_cookies(cookies: Vec<(String, String)>) -> Session {
    Session {
        cookies: cookies
            .into_iter()
            .map(|(name, value)| Cookie {
                name,
                value,
                domain: ".xiaoheihe.cn".into(),
                path: "/".into(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_qr_id_from_url() {
        assert_eq!(
            extract_qr_id("https://api.xiaoheihe.cn/account/qr_login/?qr=abc-123&app=heybox"),
            "abc-123"
        );
        assert_eq!(extract_qr_id("https://x/?qr=only"), "only");
    }

    #[test]
    fn reads_user_id_from_real_response_shapes() {
        // 实测：qr_state 确认后，身份在 result.profile.heybox_id。
        let confirmed = json!({
            "error": "ok",
            "heybox_id": "12345",
            "pkey": "pkey-value"
        });
        assert_eq!(read_user_id(&confirmed).as_deref(), Some("12345"));

        // restore_login 的形状。
        let restored = json!({"profile": {"heybox_id": "67890"}});
        assert_eq!(read_user_id(&restored).as_deref(), Some("67890"));

        let detail = json!({"account_detail": {"userid": "abc"}});
        assert_eq!(read_user_id(&detail).as_deref(), Some("abc"));

        // 0 和 "0" 表示没有有效身份。
        assert_eq!(read_user_id(&json!({"heybox_id": 0})), None);
        assert_eq!(read_user_id(&json!({"heybox_id": "0"})), None);
        assert_eq!(read_user_id(&json!({})), None);
    }

    #[test]
    fn normalize_cookies_adds_identity_and_pkey() {
        let session = Session {
            cookies: vec![
                Cookie {
                    name: "session_id".into(),
                    value: "keep-me".into(),
                    domain: ".xiaoheihe.cn".into(),
                    path: "/".into(),
                },
                Cookie {
                    name: "heybox_id".into(),
                    value: "stale".into(),
                    domain: ".xiaoheihe.cn".into(),
                    path: "/".into(),
                },
                Cookie {
                    name: "other".into(),
                    value: "drop".into(),
                    domain: ".example.com".into(),
                    path: "/".into(),
                },
            ],
        };
        let result = json!({"heybox_id": "42", "pkey": "pk"});
        let normalized = normalize_cookies(&session, &result);

        let names: Vec<&str> = normalized
            .cookies
            .iter()
            .map(|cookie| cookie.name.as_str())
            .collect();
        // 保留自己的会话 Cookie，丢掉外域 Cookie。
        assert!(names.contains(&"session_id"));
        assert!(!names.contains(&"other"));
        // 身份与 pkey 都显式补齐，旧的 heybox_id 被覆盖。
        assert!(names.contains(&"user_heybox_id"));
        assert!(names.contains(&"pkey"));
        let user = normalized
            .cookies
            .iter()
            .find(|cookie| cookie.name == "user_heybox_id")
            .map(|cookie| cookie.value.clone());
        assert_eq!(user.as_deref(), Some("42"));
        // 同一个名字不能出现两次。
        assert_eq!(names.iter().filter(|name| **name == "heybox_id").count(), 1);
    }
}
