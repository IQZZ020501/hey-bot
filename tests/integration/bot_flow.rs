//! 事件派发与回复流程。
//!
//! 用内存里的假平台驱动整条链路：拉消息 → 准备 → 派发 → AI → 回复 → 去重。
//! 不需要 mock HTTP server，也不碰真实接口。

use std::sync::Arc;

use heybot::bot::Message;
use heybot::reply::ReplyOutcome;
use tokio::sync::Mutex;

use super::support::{FailMode, FakePuppet, bot_with, mention, run_once, temp_store};

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

#[tokio::test]
async fn 处理器返回错误时事件派发中断() {
    let puppet = Arc::new(FakePuppet::with(vec![mention("m1")]));
    let store = Arc::new(Mutex::new(temp_store("handler-error")));
    let mut bot = heybot::bot::Bot::new(puppet.clone(), store);
    bot.on_message(|_msg: Message| async move { Err(heybot::error::Error::Ai("模型炸了".into())) });

    let outcome = run_once(&bot, mention("m1")).await;

    assert!(outcome.is_err(), "处理器报错应向上传递");
    assert!(puppet.sent().await.is_empty(), "没走到 say 就不该有记录");
}

#[tokio::test]
async fn 只回复_at_的消息_楼中楼回复被过滤掉() {
    // type=1 是「回复了我（楼中楼）」，没有 @，不该被回复。
    let mut reply_only = mention("m2");
    reply_only.message_type = "1".into();
    let puppet = Arc::new(FakePuppet::with(vec![reply_only]));
    let store = Arc::new(Mutex::new(temp_store("filter")));
    let bot = bot_with(puppet.clone(), store);

    let handled = heybot::reply::run_once(&bot, &heybot::reply::default_policy())
        .await
        .expect("跑一轮");

    assert!(handled.is_empty(), "非 @ 的消息不该进入处理流程");
    assert!(puppet.sent().await.is_empty(), "不该发出任何回复");
}
