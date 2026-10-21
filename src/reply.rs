//! 自动回复编排。
//!
//! 核心约束：
//! - 幂等：同一条 @ 消息只回一次，进程重启后仍然成立。
//! - 写请求结果未知时不重发（可能重复提交）。
//! - 回复内容走模板 + AI，AI 失败不阻塞主流程。

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::ai::AiClient;
use crate::api::HeyboxApi;
use crate::context::{self, Context, TargetKind};
use crate::error::{Error, Result};
use crate::mentions::Mention;

/// 已处理消息的本地记录。
pub struct DedupStore {
    path: PathBuf,
    entries: HashSet<String>,
}

impl DedupStore {
    /// 打开或创建记录文件。
    pub fn open(path: PathBuf) -> Result<Self> {
        let mut entries = HashSet::new();
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|error| Error::Storage(error.to_string()))?;
            for line in content.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    entries.insert(line.to_string());
                }
            }
        }
        Ok(Self { path, entries })
    }

    /// 是否已处理过。
    pub fn contains(&self, message_id: &str) -> bool {
        self.entries.contains(message_id)
    }

    /// 标记为已处理并落盘。
    pub fn mark(&mut self, message_id: &str) -> Result<()> {
        if self.entries.insert(message_id.to_string()) {
            self.flush()?;
        }
        Ok(())
    }

    /// 只保留最近的若干条，避免文件无限增长。
    pub fn prune(&mut self, keep: usize) {
        if self.entries.len() <= keep {
            return;
        }
        let mut items: Vec<String> = self.entries.drain().collect();
        items.sort();
        let drop_count = items.len() - keep;
        items.drain(..drop_count);
        self.entries = items.into_iter().collect();
    }

    /// 落盘。
    pub fn flush(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Error::Storage(error.to_string()))?;
        }
        let mut items: Vec<&String> = self.entries.iter().collect();
        items.sort();
        let content = items
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&self.path, content).map_err(|error| Error::Storage(error.to_string()))?;
        crate::store::restrict(&self.path);
        Ok(())
    }
}

/// 一次回复的结果。
#[derive(Debug)]
pub enum ReplyOutcome {
    /// 已回复。
    Replied { comment_id: String },
    /// 已处理过，跳过。
    Skipped,
    /// 目标评论已被删除，无法回复。
    TargetMissing,
    /// AI 失败，记录后跳过。
    AiFailed(String),
    /// 请求结果未知，没有重发。
    Uncertain(String),
}

/// 回复策略。
pub struct ReplyPolicy {
    /// Prompt 模板。
    pub prompts: crate::prompt::Prompts,
    /// 每次轮询间隔。
    pub poll_interval: Duration,
    /// 单轮最多处理多少条。
    pub max_per_round: usize,
    /// 回复前是否先回读确认目标存在。
    pub verify_target: bool,
}

/// 默认策略：保守、只回自己被 @ 的、回复前确认目标。
pub fn default_policy() -> ReplyPolicy {
    ReplyPolicy {
        prompts: crate::prompt::Prompts::default(),
        poll_interval: Duration::from_secs(30),
        max_per_round: 10,
        verify_target: true,
    }
}

/// 构造策略：默认用 `src/prompt.rs` 的内置模板；
/// 若存在 `prompts/` 目录（或 `HEYBOT_PROMPT_DIR`），则用其覆盖，改风格不用重编译。
///
/// 加载失败时回落到内置模板，不影响运行。
pub fn policy_with_prompts() -> ReplyPolicy {
    let mut policy = default_policy();
    match crate::prompt::Prompts::from_config() {
        Ok(prompts) => policy.prompts = prompts,
        Err(error) => tracing::warn!("加载 prompt 模板失败，使用内置默认：{error}"),
    }
    policy
}

/// 构造给 AI 的上下文。
///
/// 把「要评价什么」明确告诉模型，否则它只知道有人在 @ 它，
/// 会反问「你要我评价什么」。
///
/// 具体措辞由 reply_comment / reply_post 模板决定，这里只负责填占位符。
pub fn build_prompt(
    mention: &Mention,
    context: &Context,
    prompts: &crate::prompt::Prompts,
) -> String {
    let mut values: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    values.insert("post_title", context.post_title.clone());
    values.insert("post_author", context.post_author.clone());
    values.insert("post_body", context.post_body.clone());
    values.insert("target_text", context.target_text().to_string());
    values.insert("target_comment", context.target_comment.clone());
    values.insert("target_author", context.target_author.clone());
    values.insert("trigger_text", context.trigger_text.clone());
    values.insert(
        "from_user",
        if mention.from_user.is_empty() {
            "有人".to_string()
        } else {
            mention.from_user.clone()
        },
    );

    let template = match context.target_kind {
        TargetKind::Comment => &prompts.reply_comment,
        TargetKind::Post => &prompts.reply_post,
        TargetKind::Unknown => &prompts.reply_post,
    };
    crate::prompt::render(template, &values)
}

/// 读取一条 @ 消息的目标评论是否仍然存在。
///
/// 避免对着已经被删掉的评论回复。
pub async fn target_exists(api: &HeyboxApi, mention: &Mention) -> Result<bool> {
    let (reply_id, root_id) = mention.reply_targets();
    if reply_id == "-1" {
        // 顶层评论：目标就是主楼。
        return Ok(true);
    }
    // 用楼层收集统一判断：主楼在 link/tree 里，楼中楼在 sub_comments 里。
    // 注意 commentid 是数字，不能用 as_str() 比较。
    let floor = context::collect_floor(api, &mention.link_id, root_id).await?;
    Ok(floor.iter().any(|item| item.id == reply_id))
}

/// 处理单条 @ 消息。
///
/// 这是「准备 + 发送」的串行组合，供 `heybot reply` 这类一次性命令使用。
/// 常驻模式走 `prepare` / `post` 两阶段，好把耗时的 AI 生成并发起来。
pub async fn handle_mention(
    api: &HeyboxApi,
    ai: &AiClient,
    store: &mut DedupStore,
    policy: &ReplyPolicy,
    mention: &Mention,
) -> Result<ReplyOutcome> {
    if store.contains(&mention.message_id) {
        return Ok(ReplyOutcome::Skipped);
    }
    let Some(prepared) = prepare(api, policy, mention).await? else {
        // 目标评论已被删除。
        store.mark(&mention.message_id)?;
        return Ok(ReplyOutcome::TargetMissing);
    };
    let text = match generate(ai, policy, &prepared).await {
        Ok(text) => text,
        Err(error) => {
            // AI 失败**不写去重表**：模型超时、网络抖动都可能是暂时的，
            // 写进去等于永久放弃这条 @。交给调用方决定是否重试。
            return Ok(ReplyOutcome::AiFailed(error.to_string()));
        }
    };
    post(api, store, policy, &prepared, &text).await
}

/// 一条消息准备好但还没生成回复的状态。
pub struct Prepared {
    pub mention: Mention,
    pub context: Context,
    pub prompt: String,
}

/// 阶段一：确认目标存在并收集上下文。
///
/// 返回 `None` 表示目标评论已被删除，不该回复。
pub async fn prepare(
    api: &HeyboxApi,
    policy: &ReplyPolicy,
    mention: &Mention,
) -> Result<Option<Prepared>> {
    let (reply_id, _) = mention.reply_targets();
    if policy.verify_target && reply_id != "-1" {
        match target_exists(api, mention).await? {
            true => {}
            false => return Ok(None),
        }
    }

    // 收集上下文：要知道「评价什么」，否则模型只能反问。
    let context = context::collect(api, mention).await?;
    let prompt = build_prompt(mention, &context, &policy.prompts);
    Ok(Some(Prepared {
        mention: mention.clone(),
        context,
        prompt,
    }))
}

/// 阶段二：调用 AI 生成回复。
///
/// 这是整条链路里唯一不受小黑盒接口限速约束的 IO，
/// 所以常驻模式把它并发做。
pub async fn generate(ai: &AiClient, policy: &ReplyPolicy, prepared: &Prepared) -> Result<String> {
    let _ = &prepared.context;
    ai.complete(&policy.prompts.system, &prepared.prompt).await
}

/// 阶段三：把回复发出去。
pub async fn post(
    api: &HeyboxApi,
    store: &mut DedupStore,
    policy: &ReplyPolicy,
    prepared: &Prepared,
    text: &str,
) -> Result<ReplyOutcome> {
    let mention = &prepared.mention;
    let (reply_id, root_id) = mention.reply_targets();
    let _ = policy;
    match api
        .create_comment(&mention.link_id, text, reply_id, root_id)
        .await
    {
        Ok(data) => {
            // 服务端把新评论 id 放在**响应顶层**的 commentid，
            // result 里只有评论详情。注意它是 JSON 数字。
            let comment_id = context::numeric_id(data.get("commentid"))
                .or_else(|| context::numeric_id(data.pointer("/result/commentid")))
                .or_else(|| {
                    context::numeric_id(data.pointer("/result/comment/comment/0/commentid"))
                })
                .unwrap_or_default();
            store.mark(&mention.message_id)?;
            Ok(ReplyOutcome::Replied { comment_id })
        }
        Err(Error::Network(message)) => {
            // 响应丢失：可能已经发出去了。记录为不确定，不重发。
            store.mark(&mention.message_id)?;
            Ok(ReplyOutcome::Uncertain(message))
        }
        Err(error) => Err(error),
    }
}

/// 跑一轮：拉取 @ 消息，处理未回复过的。
pub async fn run_once(
    api: &HeyboxApi,
    ai: &AiClient,
    store: &mut DedupStore,
    policy: &ReplyPolicy,
) -> Result<Vec<(Mention, ReplyOutcome)>> {
    let response = api.mentions(0, 50).await?;
    let mentions = crate::mentions::parse_mentions(&response);
    let mut handled = Vec::new();

    for mention in mentions.into_iter().take(policy.max_per_round) {
        if !crate::mentions::is_reply_to_me(&mention) {
            continue;
        }
        let outcome = handle_mention(api, ai, store, policy, &mention).await?;
        let should_stop = matches!(
            outcome,
            ReplyOutcome::Uncertain(_) | ReplyOutcome::AiFailed(_)
        );
        handled.push((mention, outcome));
        if should_stop {
            break;
        }
    }

    store.prune(5000);
    store.flush()?;
    Ok(handled)
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
            link_title: "标题".into(),
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
    fn dedup_store_persists_across_instances() {
        let dir = std::env::temp_dir().join(format!("heybot-test-{}", std::process::id()));
        let path = dir.join("dedup.txt");
        let _ = std::fs::remove_file(&path);

        {
            let mut store = DedupStore::open(path.clone()).expect("打开记录");
            assert!(!store.contains("m1"));
            store.mark("m1").expect("记录");
        }
        let store = DedupStore::open(path.clone()).expect("重新打开");
        assert!(store.contains("m1"), "重启后仍应记得已处理");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prompt_contains_context() {
        let mention = mention("m1");
        let context = Context {
            post_title: "标题".into(),
            post_body: "帖子正文".into(),
            post_author: "发帖人".into(),
            target_comment: "要锐评的评论".into(),
            target_author: "评论者".into(),
            trigger_text: "锐评一下".into(),
            target_kind: TargetKind::Comment,
        };
        let prompt = build_prompt(&mention, &context, &crate::prompt::Prompts::default());
        assert!(prompt.contains("标题"), "{prompt}");
        assert!(prompt.contains("要锐评的评论"), "{prompt}");
        assert!(prompt.contains("评论者"), "{prompt}");
        assert!(prompt.contains("锐评一下"), "{prompt}");
        // 楼中楼场景不该让模型评价帖子正文。
        assert!(!prompt.contains("帖子正文"), "{prompt}");
    }

    #[test]
    fn post_target_prompt_uses_post_body() {
        let mention = mention("m1");
        let context = Context {
            post_title: "标题".into(),
            post_body: "帖子正文".into(),
            target_kind: TargetKind::Post,
            ..Context::default()
        };
        let prompt = build_prompt(&mention, &context, &crate::prompt::Prompts::default());
        assert!(prompt.contains("帖子正文"), "{prompt}");
        assert!(prompt.contains("这个帖子"), "{prompt}");
    }
}
