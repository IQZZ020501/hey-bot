//! heybot：小黑盒 @ 自动回复。
//!
//! 模块划分：
//! - [`signing`]：请求签名（公开网页协议）。
//! - [`api`]：API 客户端，负责限速、错误分类与写请求不重试。
//! - [`auth`]：扫码登录。
//! - [`mentions`]：@ 消息解析。
//! - [`reply`]：自动回复编排（幂等、限速、结果未知不重发）。
//! - [`ai`]：OpenAI 兼容接口客户端。

pub mod ai;
pub mod api;
pub mod auth;
pub mod config;
pub mod context;
pub mod error;
pub mod jar;
pub mod mentions;
pub mod probe;
pub mod prompt;
pub mod reply;
pub mod signing;
pub mod store;
pub mod watch;
