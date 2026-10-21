//! 测试用的公共设施：内存假平台与构造器。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use heybot::bot::{Bot, Message};
use heybot::context::{Context, TargetKind};
use heybot::error::{Error, Result};
use heybot::mentions::Mention;
use heybot::puppet::Puppet;
use heybot::reply::{DedupStore, ReplyOutcome};
use tokio::sync::Mutex;

/// 假平台模拟的失败类型。
#[derive(Clone, Copy)]
pub enum FailMode {
    /// 网络错误：结果未知，不能重发。
    Network,
    /// 服务端拒绝：确定失败，可以重试。
    Rejected,
}

/// 假平台：消息和回复都存在内存里。
pub struct FakePuppet {
    mentions: Mutex<Vec<Mention>>,
    sent: Mutex<Vec<(String, String)>>,
    /// `say` 时返回的错误，用来模拟「结果未知」与「确定失败」。
    fail_with: Option<FailMode>,
}

impl FakePuppet {
    /// 一个只有消息、没有故障的平台。
    pub fn with(mentions: Vec<Mention>) -> Self {
        Self {
            mentions: Mutex::new(mentions),
            sent: Mutex::new(Vec::new()),
            fail_with: None,
        }
    }

    /// 一个会失败的平台。
    pub fn failing(mentions: Vec<Mention>, mode: FailMode) -> Self {
        Self {
            fail_with: Some(mode),
            ..Self::with(mentions)
        }
    }

    /// 已经发出的回复：`(消息 id, 内容)`。
    pub async fn sent(&self) -> Vec<(String, String)> {
        self.sent.lock().await.clone()
    }
}

#[async_trait]
impl Puppet for FakePuppet {
    async fn mentions(&self, _offset: i64, _limit: i64) -> Result<Vec<Mention>> {
        Ok(self.mentions.lock().await.clone())
    }

    async fn target_exists(&self, _mention: &Mention) -> Result<bool> {
        Ok(true)
    }

    async fn context(&self, mention: &Mention) -> Result<Context> {
        Ok(Context {
            target_comment: format!("评论正文-{}", mention.message_id),
            target_author: "某人".into(),
            trigger_text: "@heybot 锐评一下".into(),
            target_kind: TargetKind::Comment,
            ..Default::default()
        })
    }

    async fn say(&self, mention: &Mention, text: &str) -> Result<String> {
        match self.fail_with {
            Some(FailMode::Network) => return Err(Error::Network("连接被重置".into())),
            Some(FailMode::Rejected) => return Err(Error::Rejected("服务端错误".into())),
            None => {}
        }
        self.sent
            .lock()
            .await
            .push((mention.message_id.clone(), text.to_string()));
        Ok("9001".into())
    }
}

/// 造一条 @ 消息。
pub fn mention(id: &str) -> Mention {
    Mention {
        message_id: id.into(),
        message_type: "1".into(),
        link_id: "123".into(),
        link_title: "标题".into(),
        root_id: "900".into(),
        reply_id: "901".into(),
        quote: "锐评一下".into(),
        from_user: "张三".into(),
        from_user_id: "u1".into(),
        timestamp: "1700000000".into(),
        link_url: "u".into(),
        comment_a_id: "902".into(),
    }
}

/// 造一个临时去重表。
pub fn temp_store(name: &str) -> DedupStore {
    let path: PathBuf =
        std::env::temp_dir().join(format!("heybot-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    DedupStore::open(path).expect("打开去重表")
}

/// 建一个 bot，并注册一个「固定回复」的处理器。
pub fn bot_with(puppet: Arc<FakePuppet>, store: Arc<Mutex<DedupStore>>) -> Bot {
    let mut bot = Bot::new(puppet, store);
    bot.on_message(|msg: Message| async move {
        if msg.is_replied().await {
            return Ok(ReplyOutcome::Skipped);
        }
        msg.say("这是机器生成的回复").await
    });
    bot
}

/// 走一遍完整流程：准备 → 派发。
pub async fn run_once(bot: &Bot, mention: Mention) -> Result<ReplyOutcome> {
    let prepared = heybot::reply::prepare(
        bot.puppet().as_ref(),
        &heybot::reply::default_policy(),
        &mention,
    )
    .await?
    .expect("目标存在");
    let message = Message::from_prepared(prepared, bot.puppet().clone(), bot.dedup().clone());
    bot.emit(message).await
}
