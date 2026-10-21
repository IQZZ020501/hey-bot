//! 签名连通性自检。
//!
//! 不需要登录态：用真实签名请求一个公开接口，观察服务端是否认可签名。
//! 服务端用 `msg` 区分"签名不对"和"没登录"，据此判断签名实现是否正确。

use crate::api::HeyboxApi;
use crate::error::Result;

/// 探测结果。
#[derive(Debug)]
pub struct Probe {
    /// 端点。
    pub path: String,
    /// 服务端 `status`。
    pub status: String,
    /// 服务端 `msg`。
    pub message: String,
    /// 是否被服务端判定为签名错误。
    pub signature_rejected: bool,
}

/// 探测一个端点。
pub async fn probe(api: &HeyboxApi, path: &str) -> Result<Probe> {
    match api.get(path, &[]).await {
        Ok(data) => Ok(Probe {
            path: path.to_string(),
            status: data
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
            message: data
                .get("msg")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
            signature_rejected: false,
        }),
        Err(crate::error::Error::Protocol(message)) => Ok(Probe {
            path: path.to_string(),
            status: "protocol_error".into(),
            message,
            signature_rejected: false,
        }),
        Err(error) => {
            let text = error.to_string();
            // 服务端对无效签名返回"系统时间不正确"或类似提示。
            let signature_rejected = text.contains("时间") || text.contains("签名");
            Ok(Probe {
                path: path.to_string(),
                status: "error".into(),
                message: text,
                signature_rejected,
            })
        }
    }
}
