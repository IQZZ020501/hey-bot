//! 事件派发与消息对象。
//!
//! 借鉴 wechaty 的使用形态：注册处理器，收到消息时自动调用。
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use heybot::bot::Bot;
//! # use heybot::provider::XiaoheihePuppet;
//! # use heybot::reply::{DedupStore, ReplyOutcome};
//! # async fn demo(api: heybot::api::HeyboxApi, store: DedupStore) -> anyhow::Result<()> {
//! let mut bot = Bot::new(Arc::new(XiaoheihePuppet::new(api)), Arc::new(tokio::sync::Mutex::new(store)));
//! bot.on_message(|msg| async move {
//!     if msg.is_replied().await {
//!         return Ok(ReplyOutcome::Skipped);
//!     }
//!     let text = "收到，我来评价一下。".to_string();
//!     msg.say(&text).await
//! });
//! # Ok(())
//! # }
//! ```
//!
//! 幂等由 [`Message::say`] 自己保证：同一条消息重复调用不会重复发送；
//! 结果未知（网络错误）时也会写去重表，避免重发。

use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::context::Context;
use crate::error::{Error, Result};
use crate::mentions::Mention;
use crate::puppet::Puppet;
use crate::reply::{DedupStore, ReplyOutcome};

/// 消息处理器的返回类型。
pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<ReplyOutcome>> + Send>>;

/// 一条待回复的消息。
///
/// 消息本身已经准备好：上下文已收集、prompt 已渲染，
/// 处理器只需要生成回复并调用 [`Message::say`]。
#[derive(Clone)]
pub struct Message {
    mention: Mention,
    context: Context,
    prompt: String,
    puppet: Arc<dyn Puppet>,
    dedup: Arc<Mutex<DedupStore>>,
}

impl Message {
    /// 构造一条消息。
    pub fn new(
        mention: Mention,
        context: Context,
        prompt: String,
        puppet: Arc<dyn Puppet>,
        dedup: Arc<Mutex<DedupStore>>,
    ) -> Self {
        Self {
            mention,
            context,
            prompt,
            puppet,
            dedup,
        }
    }

    /// 消息 id，用于幂等去重。
    pub fn id(&self) -> &str {
        &self.mention.message_id
    }

    /// 触发 @ 的那条评论正文。
    pub fn text(&self) -> &str {
        &self.mention.quote
    }

    /// 原始消息。
    pub fn mention(&self) -> &Mention {
        &self.mention
    }

    /// 收集到的上下文：帖子标题、目标评论、触发文本等。
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// 已渲染的 user prompt。
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// 用 [`crate::reply::prepare`] 的结果构造消息。
    pub fn from_prepared(
        prepared: crate::reply::Prepared,
        puppet: Arc<dyn Puppet>,
        dedup: Arc<Mutex<DedupStore>>,
    ) -> Self {
        Self {
            mention: prepared.mention,
            context: prepared.context,
            prompt: prepared.prompt,
            puppet,
            dedup,
        }
    }

    /// 这条消息是否已经回复过。
    pub async fn is_replied(&self) -> bool {
        self.dedup.lock().await.contains(&self.mention.message_id)
    }

    /// 回复这条消息。
    ///
    /// - 已回复过：返回 [`ReplyOutcome::Skipped`]，不重复发送。
    /// - 发送成功：写去重表，返回 [`ReplyOutcome::Replied`]。
    /// - 网络错误：**结果未知**（请求可能已送达），写去重表后返回
    ///   [`ReplyOutcome::Uncertain`]，避免重发。
    /// - 其他错误：向上传递，不写去重表，调用方可以重试。
    pub async fn say(&self, text: &str) -> Result<ReplyOutcome> {
        if self.is_replied().await {
            return Ok(ReplyOutcome::Skipped);
        }
        match self.puppet.say(&self.mention, text).await {
            Ok(comment_id) => {
                self.dedup.lock().await.mark(&self.mention.message_id)?;
                Ok(ReplyOutcome::Replied { comment_id })
            }
            Err(Error::Network(message)) => {
                // 结果未知：可能已经发出去了。写去重表，不重发。
                self.dedup.lock().await.mark(&self.mention.message_id)?;
                Ok(ReplyOutcome::Uncertain(message))
            }
            Err(error) => Err(error),
        }
    }
}

/// 机器人：持有平台实现，派发消息事件。
#[derive(Clone)]
pub struct Bot {
    puppet: Arc<dyn Puppet>,
    dedup: Arc<Mutex<DedupStore>>,
    handlers: Vec<Arc<dyn Fn(Message) -> HandlerFuture + Send + Sync>>,
}

impl Bot {
    /// 构造机器人。
    pub fn new(puppet: Arc<dyn Puppet>, dedup: Arc<Mutex<DedupStore>>) -> Self {
        Self {
            puppet,
            dedup,
            handlers: Vec::new(),
        }
    }

    /// 注册一个消息处理器。
    ///
    /// 处理器按注册顺序调用；第一个「确实处理了」的处理器会中断后续调用。
    pub fn on_message<F, Fut>(&mut self, handler: F)
    where
        F: Fn(Message) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ReplyOutcome>> + Send + 'static,
    {
        self.handlers.push(Arc::new(move |message| {
            Box::pin(handler(message)) as HandlerFuture
        }));
    }

    /// 平台实现。
    pub fn puppet(&self) -> &Arc<dyn Puppet> {
        &self.puppet
    }

    /// 去重记录。
    pub fn dedup(&self) -> &Arc<Mutex<DedupStore>> {
        &self.dedup
    }

    /// 派发一条消息给处理器。
    ///
    /// 处理器返回 `Err` 时中断派发并向上传递——调用方据此判断「本轮失败」，
    /// 不推进水位线，下一轮重试。
    pub async fn emit(&self, message: Message) -> Result<ReplyOutcome> {
        let mut outcome = ReplyOutcome::Skipped;
        for handler in &self.handlers {
            match handler(message.clone()).await {
                Ok(result) => {
                    if !matches!(result, ReplyOutcome::Skipped) {
                        outcome = result;
                        break;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(outcome)
    }
}
