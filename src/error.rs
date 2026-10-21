//! 错误类型。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("网络请求失败：{0}")]
    Network(String),

    #[error("认证失败，请重新扫码登录。")]
    Auth,

    #[error("小黑盒要求验证码，请重新登录完成验证。")]
    Captcha,

    #[error("小黑盒限流，已停止请求。")]
    RateLimit,

    #[error("协议不兼容：小黑盒返回了预期之外的结构。{0}")]
    Protocol(String),

    #[error("接口拒绝了操作：{0}")]
    Rejected(String),

    #[error("结果未知（请求可能已送达），未自动重发。{0}")]
    Uncertain(String),

    #[error("本地存储错误：{0}")]
    Storage(String),

    #[error("AI 接口错误：{0}")]
    Ai(String),

    #[error("配置错误：{0}")]
    Config(String),
}

/// 本 crate 的错误类型别名。
///
/// 错误类型作为默认参数暴露，调用点可以写 `Result<T>`，
/// 也可以在需要精确错误类型时写 `Result<T, SomeError>`。
pub type Result<T, E = Error> = std::result::Result<T, E>;
