//! 常驻监听。
//!
//! 行为对齐 wechaty 这类常驻 bot：
//! - 启动时只建立水位线，**不回复历史消息**。
//! - 之后只处理水位线之后的新消息。
//! - 断线自动重连；会话失效则明确退出，让人重新登录。
//!
//! 水位线用 `message_id`：实测它按时间严格递减，新的消息 id 更大。
//!
//! ## 消息不会丢
//!
//! 三条规则保证「有人 @ 了但没被回复」不会发生：
//! 1. **水位线只在处理成功后推进**。一条消息处理失败（网络抖动、AI 超时），
//!    水位线停在它前面，下一轮会重新处理。
//! 2. **失败的消息不写去重表**。去重表只记录「确实回复了」的消息。
//! 3. **分页追平**。一次来超过一页的 @ 时，递增 `offset` 一直拉到追上水位线。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bot::{Bot, Message};
use crate::error::{Error, Result};
use crate::mentions::Mention;
use crate::puppet::Puppet;
use crate::reply::{self, DedupStore, Prepared, ReplyOutcome, ReplyPolicy};

/// 监听配置。
pub struct WatchConfig {
    /// 轮询间隔。
    pub interval: Duration,
    /// 每页拉多少条。
    pub page_size: i64,
    /// 启动时是否把历史消息标记为已处理（true = 不回历史）。
    pub skip_history: bool,
    /// 只处理指定用户发的消息（空 = 不过滤）。
    pub only_user: String,
    /// 连续失败多少次后放弃。
    pub max_consecutive_failures: u32,
    /// 单轮最多处理多少条新消息（背压，防止一轮跑太久）。
    pub max_per_round: usize,
    /// 同时进行的 AI 生成数。
    ///
    /// 接口调用由客户端内部的节流器串行化（600ms 间隔），并发不会打爆。
    pub concurrency: usize,
    /// 「目标评论查不到」重试多少次后才认定它真被删了。
    ///
    /// 评论刚发出来时，消息中心可能先于评论树索引到达，
    /// 这时查不到不等于被删除。重试几次都查不到才当作已删。
    pub target_missing_retries: u32,
}

/// 默认配置。
pub fn default_config() -> WatchConfig {
    WatchConfig {
        interval: Duration::from_secs(20),
        page_size: 20,
        skip_history: true,
        only_user: String::new(),
        max_consecutive_failures: 10,
        max_per_round: 20,
        concurrency: 4,
        target_missing_retries: 5,
    }
}

/// 记录「目标评论查不到」的重试次数。
#[derive(Debug, Default)]
pub struct MissingTracker {
    counts: HashMap<String, u32>,
}

impl MissingTracker {
    /// 记一次，返回这是第几次查不到。
    pub fn record(&mut self, message_id: &str) -> u32 {
        let count = self.counts.entry(message_id.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    /// 清除记录（消息已处理）。
    pub fn clear(&mut self, message_id: &str) {
        self.counts.remove(message_id);
    }
}

/// 水位线：记录已成功处理的最大 message_id。
#[derive(Debug, Default, Clone, Copy)]
pub struct Watermark {
    highest_message_id: u64,
}

impl Watermark {
    /// 记录一条消息。
    pub fn observe(&mut self, mention: &Mention) {
        if let Ok(id) = mention.message_id.parse::<u64>() {
            if id > self.highest_message_id {
                self.highest_message_id = id;
            }
        }
    }

    /// 是否是水位线之后的新消息。
    pub fn is_new(&self, mention: &Mention) -> bool {
        match mention.message_id.parse::<u64>() {
            Ok(id) => id > self.highest_message_id,
            // id 解析不了就保守处理：交给去重表判断。
            Err(_) => true,
        }
    }

    /// 当前水位线。
    pub fn value(&self) -> u64 {
        self.highest_message_id
    }
}

/// 一次轮询的结果。
#[derive(Debug, Default)]
pub struct Round {
    /// 本轮新消息数。
    pub fresh: usize,
    /// 本轮回复数。
    pub replied: usize,
    /// 本轮失败数。
    pub failed: usize,
}

/// 一页消息的筛选结果。
#[derive(Debug, Default)]
pub struct PageOutcome {
    /// 本页里的新消息。
    pub fresh: Vec<Mention>,
    /// 是否已追到旧消息（可以停止翻页）。
    pub caught_up: bool,
}

/// 从一页消息里挑出新消息。
///
/// 纯函数，方便单测。`seen` 用来去掉跨页重复的条目。
pub fn select_fresh(
    page: &[Mention],
    watermark: &Watermark,
    seen: &mut HashSet<String>,
) -> PageOutcome {
    let mut outcome = PageOutcome::default();
    for mention in page {
        if !seen.insert(mention.message_id.clone()) {
            continue;
        }
        if !watermark.is_new(mention) {
            // 这条不比水位线新，后面只会更旧 —— 停止翻页。
            outcome.caught_up = true;
            continue;
        }
        outcome.fresh.push(mention.clone());
    }
    outcome
}

/// 拉取所有未处理的新消息。
///
/// 递增 `offset` 一直拉到：没有更多消息，或追上了水位线。
/// 否则一次来超过一页的 @ 时，最老的那批永远拉不到。
async fn fetch_new_mentions(
    puppet: &dyn Puppet,
    watermark: &Watermark,
    config: &WatchConfig,
) -> Result<Vec<Mention>> {
    let mut collected: Vec<Mention> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut offset = 0i64;

    loop {
        let page = puppet.mentions(offset, config.page_size).await?;
        if page.is_empty() {
            break;
        }
        let page_count = page.len() as i64;
        let outcome = select_fresh(&page, watermark, &mut seen);
        collected.extend(outcome.fresh);
        if outcome.caught_up {
            break;
        }
        offset += page_count;
        if page_count < config.page_size {
            break;
        }
        // 防止接口异常时无限翻页。
        if offset > 10_000 {
            break;
        }
    }

    // 从旧到新处理，回复顺序与 @ 顺序一致。
    collected.sort_by_key(|mention| mention.message_id.parse::<u64>().unwrap_or_default());
    Ok(collected)
}

/// 跑一轮。
///
/// 三个阶段：
/// 1. **准备**（串行）：确认目标存在、收集上下文、渲染 prompt。走小黑盒接口，受限速保护。
///    这一步保持串行，因为「目标评论暂时查不到」要按顺序停下来重试。
/// 2. **派发事件**（并发）：把准备好的消息交给处理器，处理器里做 AI 生成与发送。
///    AI 是唯一不受接口限速约束的 IO，并发能显著提速；接口调用由客户端内部的
///    节流器串行化，并发不会打爆。
/// 3. **结算**（按消息顺序）：从旧到新推进水位线。
///
/// 失败语义：只有「确实处理完」的消息才推进水位线。
/// AI 失败或读取异常时水位线停在它前面，下一轮重试；只有「结果未知」
/// （请求可能已送达）才写去重表防止重复发。
pub async fn run_round(
    bot: &Bot,
    store: &Arc<Mutex<DedupStore>>,
    policy: &ReplyPolicy,
    watermark: &mut Watermark,
    missing: &mut MissingTracker,
    config: &WatchConfig,
) -> Result<Round> {
    let mut round = Round::default();

    let pending = fetch_new_mentions(bot.puppet().as_ref(), watermark, config).await?;
    let candidates: Vec<Mention> = pending
        .into_iter()
        .take(config.max_per_round)
        .filter(|mention| {
            // 只处理需要回复的类型。
            crate::mentions::is_reply_to_me(mention)
                // 只回指定用户。
                && (config.only_user.is_empty() || mention.from_user == config.only_user)
        })
        .collect();
    round.fresh = candidates.len();
    if candidates.is_empty() {
        return Ok(round);
    }

    // 阶段一：准备（串行）。
    let mut prepared = Vec::new();
    for mention in candidates {
        match reply::prepare(bot.puppet().as_ref(), policy, &mention).await {
            Ok(Some(item)) => {
                missing.clear(&mention.message_id);
                prepared.push(item);
            }
            Ok(None) => {
                // 查不到目标评论。可能是评论树索引还没跟上，
                // 也可能评论真被删了。重试几次都查不到才算删。
                let attempts = missing.record(&mention.message_id);
                if attempts >= config.target_missing_retries {
                    tracing::warn!(
                        "目标评论确认不存在（重试 {attempts} 次），跳过消息 {}",
                        mention.message_id
                    );
                    store.lock().await.mark(&mention.message_id)?;
                    watermark.observe(&mention);
                } else {
                    tracing::info!(
                        "目标评论暂时查不到（{attempts}/{}），稍后重试",
                        config.target_missing_retries
                    );
                    // 不推进水位线，下一轮重新处理。
                    break;
                }
            }
            Err(error) => {
                // 读取异常：不推进水位线，下一轮重试。
                round.failed += 1;
                tracing::warn!("准备消息失败，稍后重试：{error}");
                break;
            }
        }
    }

    // 阶段二：派发事件（并发）。
    //
    // handler 内部做「AI 生成 + 发送」两件事。AI 不受接口限速约束，可以并发；
    // 小黑盒接口调用由客户端内部的节流器串行化（600ms 间隔），并发不会打爆。
    let concurrency = config.concurrency.max(1);
    let mut outcomes: Vec<Result<ReplyOutcome>> = Vec::with_capacity(prepared.len());
    let mut next = 0usize;
    while next < prepared.len() {
        let batch: Vec<Prepared> = prepared[next..].iter().take(concurrency).cloned().collect();
        next += batch.len();
        let mut tasks = Vec::new();
        for item in batch {
            let bot = bot.clone();
            let message = Message::from_prepared(item, bot.puppet().clone(), bot.dedup().clone());
            tasks.push(tokio::spawn(async move { bot.emit(message).await }));
        }
        for task in tasks {
            match task.await {
                Ok(result) => outcomes.push(result),
                Err(error) => outcomes.push(Err(Error::Ai(format!("处理任务异常：{error}")))),
            }
        }
    }

    // 阶段三：结算（按消息顺序）。
    //
    // 水位线只推进到「连续成功」的边界：遇到失败就停下，
    // 失败的消息留在水位线之外，下一轮重试。
    for (item, outcome) in prepared.iter().zip(outcomes) {
        match outcome {
            Ok(ReplyOutcome::Replied { .. }) => {
                round.replied += 1;
                watermark.observe(&item.mention);
            }
            Ok(ReplyOutcome::Uncertain(message)) => {
                // 结果未知：已写去重表，不重发。水位线推进。
                round.failed += 1;
                tracing::warn!("结果未知，未重发：{message}");
                watermark.observe(&item.mention);
            }
            Ok(ReplyOutcome::Skipped) => {
                watermark.observe(&item.mention);
            }
            Ok(ReplyOutcome::TargetMissing) => {
                // 目标评论已被删除（阶段一已确认存在，这里是兜底）。
                tracing::debug!("目标评论已删除，跳过。");
            }
            Ok(ReplyOutcome::AiFailed(message)) => {
                // AI 失败：不写去重表、不推进水位线，下一轮重试。
                round.failed += 1;
                tracing::warn!("AI 生成失败，稍后重试：{message}");
                break;
            }
            Err(error) => {
                // 发送失败：同样不推进水位线。
                round.failed += 1;
                tracing::warn!("发送失败，稍后重试：{error}");
                break;
            }
        }
    }

    {
        let mut store = store.lock().await;
        store.prune(20_000);
        store.flush()?;
    }
    Ok(round)
}

/// 建立初始水位线：把当前能看到的消息都标记为已见。
pub async fn prime_watermark(
    puppet: &dyn Puppet,
    store: &Arc<Mutex<DedupStore>>,
    page_size: i64,
) -> Result<Watermark> {
    let mut watermark = Watermark::default();
    for mention in puppet.mentions(0, page_size).await? {
        watermark.observe(&mention);
        store.lock().await.mark(&mention.message_id)?;
    }
    store.lock().await.flush()?;
    Ok(watermark)
}

/// 常驻主循环。
///
/// 返回 `Ok(())` 表示正常退出（收到退出信号）。
pub async fn watch(
    bot: &Bot,
    store: &Arc<Mutex<DedupStore>>,
    policy: &ReplyPolicy,
    config: &WatchConfig,
) -> Result<()> {
    // 水位线落盘：重启后接着上次的进度走，
    // 这样 bot 停机期间到达的 @ 仍会被补上。
    let watermark_path = crate::store::watermark_path();
    let mut watermark = match crate::store::load_watermark(&watermark_path) {
        Some(value) if config.skip_history => {
            tracing::info!("沿用上次的水位线（message_id={value}），停机期间到达的消息会补上。");
            Watermark {
                highest_message_id: value,
            }
        }
        _ => {
            let watermark = prime_watermark(bot.puppet().as_ref(), store, config.page_size).await?;
            tracing::info!(
                "已建立水位线，忽略历史消息。最新 message_id={}",
                watermark.value()
            );
            watermark
        }
    };

    let mut failures = 0u32;
    let mut missing = MissingTracker::default();
    loop {
        match run_round(bot, store, policy, &mut watermark, &mut missing, config).await {
            Ok(round) => {
                failures = 0;
                // 水位线落盘：崩了也不丢进度。
                if let Err(error) = crate::store::save_watermark(&watermark_path, watermark.value())
                {
                    tracing::warn!("保存水位线失败：{error}");
                }
                if round.fresh > 0 {
                    tracing::info!(
                        "新消息 {} 条，已回复 {} 条，失败 {} 条",
                        round.fresh,
                        round.replied,
                        round.failed
                    );
                }
            }
            Err(Error::RateLimit) => {
                tracing::warn!("被限流，暂停 60 秒");
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
            Err(Error::Auth) => {
                tracing::error!("会话失效，请重新执行 heybot login");
                return Err(Error::Auth);
            }
            Err(error) => {
                failures += 1;
                tracing::error!("本轮失败（连续 {failures} 次）：{error}");
                if failures >= config.max_consecutive_failures {
                    return Err(error);
                }
                // 失败后退避，避免把服务端打挂。
                let backoff = Duration::from_secs((failures as u64 * 5).min(60));
                tokio::time::sleep(backoff).await;
                continue;
            }
        }
        tokio::time::sleep(config.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mentions::Mention;

    fn mention(id: &str) -> Mention {
        Mention {
            message_id: id.into(),
            message_type: "1".into(),
            link_id: "123".into(),
            link_title: String::new(),
            root_id: "900".into(),
            reply_id: "901".into(),
            quote: "在吗".into(),
            from_user: "张三".into(),
            from_user_id: "u1".into(),
            timestamp: "1".into(),
            link_url: "u".into(),
            comment_a_id: "902".into(),
        }
    }

    #[test]
    fn watermark_only_accepts_newer_messages() {
        let mut watermark = Watermark::default();
        watermark.observe(&mention("100"));
        assert!(!watermark.is_new(&mention("100")), "同一条不该算新");
        assert!(!watermark.is_new(&mention("99")), "更旧的不算新");
        assert!(watermark.is_new(&mention("101")), "更新的算新");

        // 真实流程是先判断再记录：判断不会推进水位线。
        assert_eq!(watermark.value(), 100);
        watermark.observe(&mention("101"));
        assert_eq!(watermark.value(), 101);
        assert!(!watermark.is_new(&mention("101")), "记录后不再算新");
    }

    #[test]
    fn watermark_handles_unparsable_ids() {
        let mut watermark = Watermark::default();
        watermark.observe(&mention("100"));
        // 解析不了就交给去重表判断，不在这里误杀。
        assert!(watermark.is_new(&mention("not-a-number")));
    }

    #[test]
    fn select_fresh_keeps_new_and_stops_at_old() {
        let mut watermark = Watermark::default();
        watermark.observe(&mention("100"));
        let mut seen = HashSet::new();

        // 一页里既有新消息也有旧消息。
        let page = vec![mention("101"), mention("100"), mention("99")];
        let outcome = select_fresh(&page, &watermark, &mut seen);
        assert_eq!(outcome.fresh.len(), 1);
        assert_eq!(outcome.fresh[0].message_id, "101");
        // 遇到旧消息就说明后面只会更旧 → 停止翻页。
        assert!(outcome.caught_up);
    }

    #[test]
    fn select_fresh_dedupes_across_pages() {
        let watermark = Watermark::default();
        let mut seen = HashSet::new();
        let page = vec![mention("101"), mention("100")];
        let first = select_fresh(&page, &watermark, &mut seen);
        // 第二页又出现同样的消息，不该重复收集。
        let second = select_fresh(&page, &watermark, &mut seen);
        assert_eq!(first.fresh.len(), 2);
        assert_eq!(second.fresh.len(), 0);
    }

    #[test]
    fn select_fresh_keeps_going_when_all_new() {
        let watermark = Watermark::default();
        let mut seen = HashSet::new();
        let page = vec![mention("101"), mention("102")];
        let outcome = select_fresh(&page, &watermark, &mut seen);
        assert_eq!(outcome.fresh.len(), 2);
        // 全是新消息 → 还要继续翻页找更老的。
        assert!(!outcome.caught_up);
    }

    #[test]
    fn backpressure_limits_one_round() {
        let config = WatchConfig {
            max_per_round: 3,
            ..default_config()
        };
        let many: Vec<Mention> = (1..=10).map(|i| mention(&i.to_string())).collect();
        // 模拟 max_per_round 的截断。
        let limited = many.iter().take(config.max_per_round).count();
        assert_eq!(limited, 3);
    }

    #[test]
    fn missing_tracker_counts_and_clears() {
        let mut tracker = MissingTracker::default();
        assert_eq!(tracker.record("m1"), 1);
        assert_eq!(tracker.record("m1"), 2);
        // 不同消息各记各的。
        assert_eq!(tracker.record("m2"), 1);
        tracker.clear("m1");
        // 清除后重新计数。
        assert_eq!(tracker.record("m1"), 1);
    }
}
