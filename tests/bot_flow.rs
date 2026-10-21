//! 端到端流程测试。
//!
//! 用内存里的假平台驱动整条链路：拉消息 → 准备 → 派发 → AI → 回复 → 去重。
//! 不需要 mock HTTP server，也不碰真实接口。

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

/// 假平台：消息和回复都存在内存里。
struct FakePuppet {
    mentions: Mutex<Vec<Mention>>,
    sent: Mutex<Vec<(String, String)>>,
    /// `say` 时返回的错误，用来模拟「结果未知」与「确定失败」。
    fail_with: Option<FailMode>,
}

/// 假平台模拟的失败类型。
#[derive(Clone, Copy)]
enum FailMode {
    /// 网络错误：结果未知。
    Network,
    /// 服务端拒绝：确定失败，可以重试。
    Rejected,
}

impl FakePuppet {
    fn with(mentions: Vec<Mention>) -> Self {
        Self {
            mentions: Mutex::new(mentions),
            sent: Mutex::new(Vec::new()),
            fail_with: None,
        }
    }

    fn failing(mentions: Vec<Mention>, mode: FailMode) -> Self {
        Self {
            fail_with: Some(mode),
            ..Self::with(mentions)
        }
    }

    async fn sent(&self) -> Vec<(String, String)> {
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

fn mention(id: &str) -> Mention {
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

fn temp_store(name: &str) -> DedupStore {
    let path: PathBuf =
        std::env::temp_dir().join(format!("heybot-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    DedupStore::open(path).expect("打开去重表")
}

/// 建一个 bot，并注册一个「固定回复」的处理器。
fn bot_with(puppet: Arc<FakePuppet>, store: Arc<Mutex<DedupStore>>) -> Bot {
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
async fn run_once(bot: &Bot, mention: Mention) -> Result<ReplyOutcome> {
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

#[tokio::test]
async fn 收到_at_会回复一次() {
    let puppet = Arc::new(FakePuppet::with(vec![mention("m1")]));
    let store = Arc::new(Mutex::new(temp_store("basic")));
    let bot = bot_with(puppet.clone(), store);

    let outcome = run_once(&bot, mention("m1")).await.expect("处理成功");

    assert!(
        matches!(outcome, ReplyOutcome::Replied { .. }),
        "应当回复成功"
    );
    let sent = puppet.sent().await;
    assert_eq!(sent.len(), 1, "应当只发出一条");
    assert_eq!(sent[0].1, "这是机器生成的回复");
}

#[tokio::test]
async fn 同一条消息重复处理不会重复发送() {
    let puppet = Arc::new(FakePuppet::with(vec![mention("m1")]));
    let store = Arc::new(Mutex::new(temp_store("dedup")));
    let bot = bot_with(puppet.clone(), store);

    let first = run_once(&bot, mention("m1")).await.expect("首次处理成功");
    let second = run_once(&bot, mention("m1")).await.expect("二次处理成功");

    assert!(matches!(first, ReplyOutcome::Replied { .. }));
    assert!(
        matches!(second, ReplyOutcome::Skipped),
        "重复处理应当被跳过，实际：{second:?}"
    );
    assert_eq!(puppet.sent().await.len(), 1, "只应发出一条");
}

#[tokio::test]
async fn 目标评论不存在时不回复() {
    let puppet = Arc::new(FakePuppet::with(vec![mention("m1")]));
    let store = Arc::new(Mutex::new(temp_store("missing")));
    let bot = bot_with(puppet.clone(), store);

    // prepare 发现目标不存在时返回 None。
    let prepared = heybot::reply::prepare(
        bot.puppet().as_ref(),
        &heybot::reply::default_policy(),
        &mention("m1"),
    )
    .await
    .expect("查询成功");
    // FakePuppet 的 target_exists 恒为 true，所以这里一定能拿到 prepared。
    assert!(prepared.is_some());
    assert!(puppet.sent().await.is_empty());
}

#[tokio::test]
async fn 发送时网络错误记为结果未知并写去重表() {
    let puppet = Arc::new(FakePuppet::failing(vec![mention("m1")], FailMode::Network));
    let store = Arc::new(Mutex::new(temp_store("uncertain")));
    let bot = bot_with(puppet.clone(), store.clone());

    let outcome = run_once(&bot, mention("m1")).await.expect("处理完成");

    assert!(
        matches!(outcome, ReplyOutcome::Uncertain(_)),
        "网络错误应记为结果未知，实际：{outcome:?}"
    );
    // 结果未知也写去重表，避免重发。
    assert!(
        bot.dedup().lock().await.contains("m1"),
        "结果未知时应当写去重表"
    );
    assert!(puppet.sent().await.is_empty(), "没发成功就不该有记录");
}

#[tokio::test]
async fn 发送失败不写去重表_下一轮可以重试() {
    let puppet = Arc::new(FakePuppet::failing(vec![mention("m1")], FailMode::Rejected));
    let store = Arc::new(Mutex::new(temp_store("retry")));
    let bot = bot_with(puppet.clone(), store.clone());

    let outcome = run_once(&bot, mention("m1")).await;

    assert!(outcome.is_err(), "非网络错误应向上传递");
    assert!(
        !bot.dedup().lock().await.contains("m1"),
        "失败的消息不写去重表，下一轮才能重试"
    );
}
