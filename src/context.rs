//! @ 消息的上下文收集。
//!
//! 收到「@bot 锐评一下」时，bot 必须知道**要锐评什么**：
//! - @ 触发评论是某条评论的回复 → 目标是**那条被回复的评论**（楼中楼场景）
//! - @ 触发评论是帖子下的顶层评论 → 目标是**整个帖子**（帖子场景）
//!
//! 两种场景的消息结构完全一样，只能靠「触发评论挂在哪」来区分：
//! 顶层评论的 `root_comment_id == comment_a_id`；回复则不等。
//!
//! 收集到：
//! - 帖子标题与正文（`link.text` 是文本块数组）
//! - 目标（被回复的评论，或整篇帖子）
//! - 触发评论的正文

use serde_json::Value;

use crate::api::HeyboxApi;
use crate::error::Result;
use crate::mentions::{Mention, plain_text};

/// 一个 @ 消息的完整上下文。
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// 帖子标题。
    pub post_title: String,
    /// 帖子正文（纯文本）。
    pub post_body: String,
    /// 帖子作者。
    pub post_author: String,
    /// 目标评论正文（楼中楼场景）。
    pub target_comment: String,
    /// 目标评论作者。
    pub target_author: String,
    /// 触发这次 @ 的评论正文。
    pub trigger_text: String,
    /// 目标类型：帖子还是评论。
    pub target_kind: TargetKind,
}

/// 这次该评价什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TargetKind {
    /// 不知道，退回帖子。
    #[default]
    Unknown,
    /// 评价整篇帖子。
    Post,
    /// 评价某条评论。
    Comment,
}

impl Context {
    /// 是否拿到了可评价的内容。
    pub fn has_target(&self) -> bool {
        !self.target_comment.trim().is_empty() || !self.post_body.trim().is_empty()
    }

    /// 目标描述，用于给 AI 看。
    pub fn target_text(&self) -> &str {
        if self.target_kind == TargetKind::Comment {
            &self.target_comment
        } else {
            &self.post_body
        }
    }
}

/// 解析帖子正文（`link.text` 是文本块数组）。
pub fn parse_post_body(text: &str) -> String {
    let Ok(blocks) = serde_json::from_str::<Vec<Value>>(text) else {
        // 不是 JSON 数组就当纯文本。
        return text.trim().to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "text"
            && let Some(content) = block.get("text").and_then(Value::as_str)
        {
            parts.push(content.to_string());
        }
    }
    parts.join("\n").trim().to_string()
}

/// 一条评论的最小信息。
#[derive(Debug, Clone)]
pub struct CommentInfo {
    pub id: String,
    pub text: String,
    pub author: String,
    /// 它回复的是哪条（楼中楼）。
    pub reply_id: String,
}

/// 收集上下文。
pub async fn collect(api: &HeyboxApi, mention: &Mention) -> Result<Context> {
    let mut context = Context {
        post_title: mention.link_title.clone(),
        trigger_text: mention.quote.clone(),
        ..Context::default()
    };

    // 帖子信息（标题、正文、作者）。
    let post = fetch_post(api, &mention.link_id).await?;
    if let Some(post) = &post {
        if !post.title.is_empty() {
            context.post_title = post.title.clone();
        }
        context.post_body = post.body.clone();
        context.post_author = post.author.clone();
    }

    // 找触发评论，以及它挂在哪。
    let trigger = find_comment(api, &mention.link_id, &mention.root_id, &mention.comment_a_id).await?;

    match trigger {
        Some(trigger) if !trigger.reply_id.is_empty() && trigger.reply_id != trigger.id => {
            // 触发评论是某条评论的回复 → 目标是它回复的那条。
            context.target_kind = TargetKind::Comment;
            if let Some(parent) =
                find_comment(api, &mention.link_id, &mention.root_id, &trigger.reply_id).await?
            {
                context.target_comment = parent.text;
                context.target_author = parent.author;
            }
        }
        _ => {
            // 顶层 @ → 目标是整篇帖子。
            context.target_kind = TargetKind::Post;
        }
    }

    Ok(context)
}

/// 帖子信息。
struct PostInfo {
    title: String,
    body: String,
    author: String,
}

async fn fetch_post(api: &HeyboxApi, link_id: &str) -> Result<Option<PostInfo>> {
    let data = api.post_comments(link_id, 0, 1).await?;
    let Some(link) = data.pointer("/result/link") else {
        return Ok(None);
    };
    Ok(Some(PostInfo {
        title: link
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        body: link
            .get("text")
            .and_then(Value::as_str)
            .map(parse_post_body)
            .unwrap_or_default(),
        author: link
            .pointer("/user/username")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }))
}

/// 收集某个楼层的全部评论（根评论 + 楼中楼）。
pub async fn collect_floor(
    api: &HeyboxApi,
    link_id: &str,
    root_id: &str,
) -> Result<Vec<CommentInfo>> {
    let mut items: Vec<CommentInfo> = Vec::new();

    // 根评论：link/tree 的 comments。
    if root_id.is_empty() {
        for page in 0..10 {
            let data = api.post_comments(link_id, page * 20, 20).await?;
            let groups = data
                .pointer("/result/comments")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            if groups.is_empty() {
                break;
            }
            for group in &groups {
                for comment in group.get("comment").and_then(|v| v.as_array()).unwrap_or(&Vec::new()) {
                    if let Some(info) = comment_info(comment) {
                        items.push(info);
                    }
                }
            }
            if data.pointer("/result/has_more_floors").and_then(|v| v.as_i64()).unwrap_or(0) == 0 {
                break;
            }
        }
        return Ok(items);
    }

    // 楼中楼：先取主楼（在 link/tree 里），再取它的回复。
    for page in 0..10 {
        let data = api.post_comments(link_id, page * 20, 20).await?;
        let groups = data
            .pointer("/result/comments")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        for group in &groups {
            for comment in group.get("comment").and_then(|v| v.as_array()).unwrap_or(&Vec::new()) {
                if let Some(info) = comment_info(comment) {
                    if info.id == root_id {
                        items.push(info);
                    }
                }
            }
        }
        if groups.is_empty() {
            break;
        }
        if data.pointer("/result/has_more_floors").and_then(|v| v.as_i64()).unwrap_or(0) == 0 {
            break;
        }
    }

    let data = api.sub_comments(link_id, root_id, "0", 50).await?;
    for comment in data
        .pointer("/result/comments")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default()
    {
        if let Some(info) = comment_info(&comment) {
            items.push(info);
        }
    }
    Ok(items)
}

/// 从评论对象里抽取最小信息。
fn comment_info(comment: &Value) -> Option<CommentInfo> {
    let id = numeric_id(comment.get("commentid"))?;
    Some(CommentInfo {
        id,
        text: comment
            .get("text")
            .and_then(Value::as_str)
            .map(plain_text)
            .unwrap_or_default(),
        author: comment
            .pointer("/user/username")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        reply_id: numeric_id(comment.get("replyid")).unwrap_or_default(),
    })
}

/// 读取数字 id：数字或数字字符串，null/缺失返回 None。
///
/// 小黑盒的 `commentid` / `replyid` 是 **JSON 数字**，不是字符串。
/// 用 `as_str()` 比较会永远匹配不上，必须统一走这里。
pub fn numeric_id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() || trimmed == "null" || trimmed == "0" {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    }
}

/// 在楼层里找指定 id 的评论。
async fn find_comment(
    api: &HeyboxApi,
    link_id: &str,
    root_id: &str,
    comment_id: &str,
) -> Result<Option<CommentInfo>> {
    if comment_id.is_empty() {
        return Ok(None);
    }
    for item in collect_floor(api, link_id, root_id).await? {
        if item.id == comment_id {
            return Ok(Some(item));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_blocks() {
        let raw = r#"[{"type":"text","text":"第一段"},{"height":"1080.0","type":"img","url":"x"},{"type":"text","text":"第二段"}]"#;
        assert_eq!(parse_post_body(raw), "第一段\n第二段");
    }

    #[test]
    fn plain_text_body_is_kept() {
        assert_eq!(parse_post_body("就是纯文本"), "就是纯文本");
    }

    #[test]
    fn comment_info_reads_real_fields() {
        let raw = serde_json::json!({
            "commentid": 980633067,
            "text": "<a href=\"x\">@heybot</a> 锐评一下",
            "replyid": null,
            "user": {"username": "玩家AF1008611"}
        });
        let info = comment_info(&raw).expect("应有评论");
        assert_eq!(info.id, "980633067");
        assert_eq!(info.author, "玩家AF1008611");
        assert_eq!(info.text, "@heybot 锐评一下");
        assert!(info.reply_id.is_empty());
    }
}
