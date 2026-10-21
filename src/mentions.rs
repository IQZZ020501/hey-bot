//! @ 消息解析。
//!
//! 小黑盒的消息接口返回结构随版本变动，这里做防御式提取：
//! 只依赖"帖子 id / 根评论 id / 被回复的评论 id / 正文"这四个语义字段。
//!
//! 社区其它项目（`better-XiaoHeiHe`、`xiaoheihe-cli`）都没有覆盖这块，
//! 它们的契约里只有评论读取和发布。

use serde_json::Value;

/// 匹配 `<a ...>内容</a>`，捕获组是内容。
///
/// 小黑盒的 @ 提及渲染成 `<a data-user-id="...">@昵称</a>`，
/// 昵称在标签内部，剥标签时会连昵称一起丢掉，所以先单独提取。
static ANCHOR_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?is)<a\b[^>]*>(.*?)</a>").expect("正则合法"));

/// 一条 @ 我的消息。
#[derive(Debug, Clone)]
pub struct Mention {
    /// 消息唯一标识，用于幂等去重。
    pub message_id: String,
    /// 消息类型。
    ///
    /// 官方语义（来自网页端消息页）：
    /// - `1`：回复了你（楼中楼）
    /// - `2`：评论了你的帖子（顶层评论）
    /// - `7`：点赞类
    ///
    /// `list_type` 是**入口**而不是类型：0 是回复与评论合并，1 是点赞。
    pub message_type: String,
    /// 所在帖子。
    pub link_id: String,
    /// 帖子标题，便于构造 AI 上下文。
    pub link_title: String,
    /// 根评论 id（楼中楼的主楼）。顶层评论为空。
    pub root_id: String,
    /// 被回复的评论 id。
    pub reply_id: String,
    /// 触发 @ 的那条评论正文。
    pub quote: String,
    /// 提及我的用户昵称。
    pub from_user: String,
    /// 提及我的用户 id。
    pub from_user_id: String,
    /// 消息时间戳（字符串形式，跨接口保真）。
    pub timestamp: String,
    /// 帖子链接。
    pub link_url: String,
    /// A 方评论 id。
    ///
    /// 两种形状下含义不同：
    /// - `message_type=1`：`comment_a_id` 是**我自己**被回复的那条评论。
    /// - `message_type=2`：没有 B 方，`comment_a_id` 是**对方**的顶层评论。
    pub comment_a_id: String,
}

impl Mention {
    /// 回复时应使用的 `reply_id` / `root_id`。
    ///
    /// 实测三种形状：
    /// - 楼中楼回复我（`message_type=1`）：`comment_b_id` 是对方那条，
    ///   `root_comment_id` 是主楼 → `reply_id=comment_b_id`。
    /// - 顶层评论了我的帖子（`message_type=2`）：只有 A 方，
    ///   `comment_a_id` 就是对方那条 → `reply_id=comment_a_id`。
    /// - **直接 @ 我（`message_type=17`）**：B 方为空，
    ///   `comment_a_id` 就是提及我的那条 → `reply_id=comment_a_id`。
    pub fn reply_targets(&self) -> (&str, &str) {
        if self.root_id.is_empty() {
            return ("-1", "-1");
        }
        if self.reply_id.is_empty() {
            // 没有 B 方：目标就是 A 方那条。
            return (self.comment_a_id.as_str(), self.root_id.as_str());
        }
        (self.reply_id.as_str(), self.root_id.as_str())
    }

    /// 是否是楼中楼回复。
    pub fn is_sub_reply(&self) -> bool {
        !self.reply_id.is_empty()
    }
}

/// 深度查找第一个非空的字符串字段。
fn deep_str(source: &Value, names: &[&str]) -> Option<String> {
    fn walk(source: &Value, names: &[&str], depth: usize) -> Option<String> {
        if depth > 8 {
            return None;
        }
        let Some(object) = source.as_object() else {
            return None;
        };
        for name in names {
            if let Some(value) = object.get(*name)
                && !value.is_null()
            {
                match value {
                    Value::String(text) if !text.trim().is_empty() => {
                        return Some(text.trim().to_string())
                    }
                    Value::Number(number) => return Some(number.to_string()),
                    _ => {}
                }
            }
        }
        for value in object.values() {
            if let Some(found) = walk(value, names, depth + 1) {
                return Some(found);
            }
        }
        None
    }
    walk(source, names, 0)
}

/// 去掉 HTML 标签，取纯文本。
///
/// 小黑盒的 @ 写法是 `<a data-user-id="...">@昵称</a>正文`，
/// 昵称在 `<a>` 标签**内部**。所以先把 a 标签的**内容**提取出来
/// （保留文字，去掉标签本身），再把其余标签剥掉。
pub fn plain_text(value: &str) -> String {
    // 第一步：把 <a ...>内容</a> 里的内容留下，标签本身去掉。
    let mut rest = String::with_capacity(value.len());
    let mut cursor = 0usize;
    for capture in ANCHOR_RE.captures_iter(value) {
        let whole = capture.get(0).expect("整体匹配一定存在");
        let inner = capture.get(1).expect("内容组一定存在");
        rest.push_str(&value[cursor..whole.start()]);
        rest.push_str(inner.as_str());
        rest.push(' ');
        cursor = whole.end();
    }
    rest.push_str(&value[cursor..]);
    let with_anchor_text = rest;

    // 第二步：剥掉其余标签，块级标签当作分隔符。
    let mut out = String::with_capacity(with_anchor_text.len());
    let mut inside = false;
    for ch in with_anchor_text.chars() {
        match ch {
            '<' => inside = true,
            '>' => {
                inside = false;
                out.push(' ');
            }
            _ if !inside => out.push(ch),
            _ => {}
        }
    }

    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 从消息列表响应里提取所有 @ 消息。
///
/// 只保留能定位到帖子的条目：没有 `link_id` 的通知（系统消息、点赞等）直接跳过。
pub fn parse_mentions(response: &Value) -> Vec<Mention> {
    let Some(list) = find_message_list(response) else {
        return Vec::new();
    };

    let mut result = Vec::new();
    for item in list {
        if let Some(mention) = parse_mention(&item) {
            result.push(mention);
        }
    }
    result
}

/// 在响应里找到消息数组。
fn find_message_list(response: &Value) -> Option<Vec<Value>> {
    const CANDIDATES: &[&str] = &[
        "message_list",
        "messages",
        "list",
        "data",
        "user_message",
        "comment_message",
    ];
    for name in CANDIDATES {
        if let Some(Value::Array(items)) = response.pointer(&format!("/result/{name}")) {
            return Some(items.clone());
        }
    }
    if let Some(Value::Array(items)) = response.pointer("/result") {
        return Some(items.clone());
    }
    if let Some(Value::Array(items)) = response.get("result") {
        return Some(items.clone());
    }
    None
}

/// 解析单条消息。
///
/// 真实字段（2026-10 实测，`message_type=16` 的「@我的」列表）：
/// - `message_type`：请求时传 16，返回时是 **17**。
/// - `comment_a_id` / `comment_a_text`：**A 方**，即提及我的那条评论。
///   直接 @ 我时正文里带 `<a>@昵称</a>`，需要剥掉标签。
/// - `comment_b_id` / `comment_b_text`：**B 方**，仅当对方是在回复我时才有；
///   直接 @ 我时为 null。
/// - `root_comment_id`：楼中楼主楼 id。
/// - `linkid`：帖子 id（注意没有下划线）。
pub fn parse_mention(item: &Value) -> Option<Mention> {
    let link_id = deep_str(item, &["linkid", "link_id", "linkId", "post_id"])?;
    if link_id.is_empty() || link_id == "-1" {
        return None;
    }

    // A 方：提及我的那条评论。直接 @ 我时就是它。
    let comment_a_id = deep_str(item, &["comment_a_id", "comment_id"])
        .unwrap_or_default();
    // B 方：对方在回复我时才有内容。
    let comment_b_id = deep_str(item, &["comment_b_id"]).unwrap_or_default();
    let root_id = deep_str(item, &["root_comment_id", "root_id", "rootid"])
        .filter(|value| value != "-1")
        .unwrap_or_default();

    let message_id = deep_str(item, &["message_id", "id", "msg_id"])
        .unwrap_or_else(|| format!("{link_id}-{root_id}-{comment_a_id}"));
    let message_type = deep_str(item, &["message_type", "type", "msg_type"])
        .unwrap_or_default();

    // 对方的正文优先用 B 方；没有 B 方时用 A 方（直接 @ 我的情形）。
    //
    // 注意：`text` 字段是服务端生成的摘要（例如「@了我」），
    // 不是对方的原话，只能兜底，不能优先。
    let quote = deep_str(item, &["comment_b_text", "comment_a_text"])
        .map(|value| plain_text(&value))
        .filter(|value| !value.is_empty())
        .or_else(|| {
            deep_str(item, &["comment_text", "content", "text"]).map(|value| plain_text(&value))
        })
        .unwrap_or_default();

    // 昵称：A 方是提我的人（`user_a`），B 方是被 @ 的人。
    // 直接 @ 我时 A 方就是对方；回复场景下 A 方是我、B 方是对方。
    let from_user = item
        .pointer("/user_a/username")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            item.pointer("/user_b/username")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| deep_str(item, &["username", "user_name", "nickname"]))
        .unwrap_or_default();
    let from_user_id = item
        .pointer("/user_a/userid")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            item.pointer("/user_b/userid")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| deep_str(item, &["userid_a", "userid"]))
        .unwrap_or_default();
    let link_title = deep_str(item, &["link_title", "title", "post_title"])
        .map(|value| plain_text(&value))
        .unwrap_or_default();
    let timestamp = deep_str(item, &["timestamp", "time", "create_time", "created_at"])
        .unwrap_or_default();
    let link_url = deep_str(item, &["link_url", "url"]).unwrap_or_else(|| {
        format!("https://www.xiaoheihe.cn/app/post/{link_id}")
    });

    Some(Mention {
        message_id,
        message_type,
        link_id,
        link_title,
        root_id,
        reply_id: comment_b_id,
        quote,
        from_user,
        from_user_id,
        timestamp,
        link_url,
        comment_a_id,
    })
}

/// 判断消息是否属于"对我发起的互动"。
///
/// 官方语义：
/// - `1` 回复了你（楼中楼）
/// - `2` 评论了你的帖子（顶层评论）
/// - `17` 直接 @ 了你（「@我的」列表返回的类型）
pub fn is_reply_to_me(mention: &Mention) -> bool {
    matches!(mention.message_type.as_str(), "1" | "2" | "17")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_root_comment_reply() {
        let response = json!({
            "status": "ok",
            "result": {
                "message_list": [
                    {
                        "message_id": "m1",
                        "message_type": "1",
                        "linkid": "12345",
                        "link_title": "<p>帖子标题</p>",
                        "root_comment_id": "838519258",
                        "comment_a_id": "861663608",
                        "comment_a_text": "我的原评论",
                        "comment_b_id": "849325471",
                        "comment_b_text": "<p>60我给你冲</p>",
                        "timestamp": 1778279017.0
                    }
                ]
            }
        });
        let mentions = parse_mentions(&response);
        assert_eq!(mentions.len(), 1);
        let mention = &mentions[0];
        assert_eq!(mention.link_id, "12345");
        assert_eq!(mention.root_id, "838519258");
        // 回复目标必须是对方那条评论（comment_b_id），不是我自己的。
        assert_eq!(mention.reply_id, "849325471");
        assert_eq!(mention.comment_a_id, "861663608");
        assert_eq!(mention.quote, "60我给你冲");
        assert_eq!(mention.link_title, "帖子标题");
        assert_eq!(mention.reply_targets(), ("849325471", "838519258"));
    }

    #[test]
    fn top_level_comment_uses_minus_one() {
        let response = json!({
            "result": {
                "list": [
                    {"linkid": "77", "message_type": "2", "comment_b_text": "在吗", "root_comment_id": "-1", "comment_b_id": "-1"}
                ]
            }
        });
        let mentions = parse_mentions(&response);
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].reply_targets(), ("-1", "-1"));
    }

    #[test]
    fn skips_entries_without_post() {
        let response = json!({
            "result": {
                "list": [
                    {"message_id": "x1", "message_type": "7", "comment_b_text": "你的帖子被点赞了"},
                    {"message_id": "x2", "linkid": "55", "message_type": "2", "comment_b_text": "在吗"}
                ]
            }
        });
        let mentions = parse_mentions(&response);
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].link_id, "55");
    }

    #[test]
    fn top_level_comment_replies_to_comment_a() {
        // 实测：message_type=2 时没有 comment_b_id，
        // comment_a_id 就是对方的顶层评论，root_comment_id 与它相同。
        let response = json!({
            "result": {
                "list": [
                    {
                        "message_id": "2874415941",
                        "message_type": 2,
                        "linkid": "161551813",
                        "root_comment_id": "695673728",
                        "comment_a_id": "695673728",
                        "comment_a_text": "想分就直说[cube_笑cry]"
                    }
                ]
            }
        });
        let mentions = parse_mentions(&response);
        assert_eq!(mentions.len(), 1);
        let mention = &mentions[0];
        assert_eq!(mention.quote, "想分就直说[cube_笑cry]");
        assert_eq!(mention.root_id, "695673728");
        // 回复目标必须是对方那条，而不是 -1。
        assert_eq!(mention.reply_targets(), ("695673728", "695673728"));
    }

    #[test]
    fn only_reply_and_comment_types_are_replies() {
        let reply = Mention {
            message_id: "m1".into(),
            message_type: "1".into(),
            link_id: "1".into(),
            link_title: String::new(),
            root_id: "9".into(),
            reply_id: "10".into(),
            quote: String::new(),
            from_user: String::new(),
            from_user_id: String::new(),
            timestamp: String::new(),
            link_url: String::new(),
            comment_a_id: String::new(),
        };
        assert!(is_reply_to_me(&reply));
        assert!(reply.is_sub_reply());

        let award = Mention {
            message_type: "7".into(),
            ..reply.clone()
        };
        assert!(!is_reply_to_me(&award), "点赞类不是需要回复的互动");
    }

    #[test]
    fn at_mention_shape_is_parsed_correctly() {
        // 实测的「@我的」消息形状（message_type=16 请求，返回 17）。
        let response = json!({
            "result": {
                "messages": [
                    {
                        "message_id": "4435362982",
                        "message_type": 17,
                        "text": "@了我",
                        "linkid": "192794050",
                        "link_title": "别忘了签到领游戏",
                        "root_comment_id": "980593543",
                        "comment_a_id": "980593543",
                        "comment_b_id": null,
                        "comment_a_text": "<a data-user-id=\"81183017\" href=\"https://x\" target=\"_blank\">@heybot</a>评价一下",
                        "comment_b_text": null,
                        "userid_a": "47235819",
                        "userid_b": "81183017",
                        "user_a": {"username": "玩家AF1008611", "userid": "47235819"},
                        "user_b": {"username": "heybot", "userid": "81183017"},
                        "timestamp": "1792568746.0000"
                    }
                ]
            }
        });
        let mentions = parse_mentions(&response);
        assert_eq!(mentions.len(), 1);
        let mention = &mentions[0];
        // 正文要用 comment_a_text，不能用服务端摘要 `text`（「@了我」）。
        assert_eq!(mention.quote, "@heybot 评价一下");
        // 提我的人是 A 方。
        assert_eq!(mention.from_user, "玩家AF1008611");
        // 直接 @ 我时 B 方为空，回复目标就是 A 方那条。
        assert_eq!(mention.reply_targets(), ("980593543", "980593543"));
        assert!(is_reply_to_me(mention));
    }

    #[test]
    fn at_mention_keeps_the_nickname_inside_the_anchor() {
        // 实测：小黑盒把 @ 渲染成 <a ...>@昵称</a>，
        // 昵称在标签内部，剥标签时必须保住。
        let raw = r#"<a data-user-id="81183017" href="https://x/#heybox://%7B%22a%22%7D" target="_blank">@heybot</a>评价一下"#;
        assert_eq!(plain_text(raw), "@heybot 评价一下");
    }

    #[test]
    fn plain_text_strips_tags_and_entities() {
        assert_eq!(plain_text("<p>a &amp; b</p>"), "a & b");
        // 换行标签被当作分隔符，避免两段文字粘在一起。
        assert_eq!(plain_text("line1<br/>line2"), "line1 line2");
    }
}
