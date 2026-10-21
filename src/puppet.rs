//! 聊天平台的统一抽象。
//!
//! 借鉴 wechaty 的 Puppet 思路：把「平台特有」的读写收敛到一个实现里，
//! 上层的 bot 与事件处理只面向这个接口。
//!
//! 接口只暴露 bot 真正需要的四个操作，不铺开整个 HTTP API：
//! 上层因此不需要知道小黑盒的字段名、评论树结构和 id 语义。

use async_trait::async_trait;

use crate::context::Context;
use crate::error::Result;
use crate::mentions::Mention;

/// 平台接口。
///
/// 实现者负责把平台的原始协议翻译成 [`Mention`] / [`Context`]，
/// 并把「发一条回复」翻译成平台的写操作。
#[async_trait]
pub trait Puppet: Send + Sync {
    /// 拉取被 @ 的消息，按时间倒序（新的在前）。
    async fn mentions(&self, offset: i64, limit: i64) -> Result<Vec<Mention>>;

    /// 要回复的目标评论是否仍然存在。
    ///
    /// 顶层评论（回复 id 为 `-1`）恒为存在。
    async fn target_exists(&self, mention: &Mention) -> Result<bool>;

    /// 收集要评价的内容与上下文。
    async fn context(&self, mention: &Mention) -> Result<Context>;

    /// 发出一条回复，返回新评论 id。
    ///
    /// 返回 `Err(Error::Network(..))` 表示**结果未知**——请求可能已经送达，
    /// 调用方据此避免重复发送。
    async fn say(&self, mention: &Mention, text: &str) -> Result<String>;
}
