//! Prompt 模板。
//!
//! Prompt 放在独立文件里，改风格不用重新编译。
//!
//! 目录结构（默认 `prompts/`，可用 `HEYBOT_PROMPT_DIR` 覆盖）：
//!
//! - `system.txt`        —— 人设与语气，发给模型的 system 消息
//! - `reply_comment.txt` —— 评价**评论**时用的 user 消息
//! - `reply_post.txt`    —— 评价**帖子**时用的 user 消息
//!
//! 占位符用 `{名字}` 形式，缺失的占位符会替换成空串，
//! 模板里没出现的占位符不影响。

use std::collections::HashMap;
use std::path::Path;

use crate::error::{Error, Result};

/// 一套 prompt 模板。
#[derive(Debug, Clone)]
pub struct Prompts {
    /// system 提示：人设与语气。
    pub system: String,
    /// 评价评论时的 user 消息。
    pub reply_comment: String,
    /// 评价帖子时的 user 消息。
    pub reply_post: String,
}

/// 内置默认模板：文件缺失时兜底，保证功能不依赖外部文件。
pub const DEFAULT_SYSTEM: &str = "\
你是小黑盒社区里的一个用户，正在回复别人对你的@。
用简短自然的中文回复，1到3句，不要复述对方原话，不要使用markdown格式，
不要自称AI或机器人。";

pub const DEFAULT_REPLY_COMMENT: &str = "\
帖子标题：{post_title}
帖子作者：{post_author}

【下面这条评论，就是需要你评价的内容】
评论者：{target_author}
评论内容：{target_comment}

{from_user}说：{trigger_text}

请直接对上面那条评论给出你的评价。";

pub const DEFAULT_REPLY_POST: &str = "\
帖子标题：{post_title}
帖子作者：{post_author}

【下面这个帖子，就是需要你评价的内容】
帖子正文：{target_text}

{from_user}说：{trigger_text}

请直接对上面这个帖子给出你的评价。";

impl Default for Prompts {
    fn default() -> Self {
        Self {
            system: DEFAULT_SYSTEM.to_string(),
            reply_comment: DEFAULT_REPLY_COMMENT.to_string(),
            reply_post: DEFAULT_REPLY_POST.to_string(),
        }
    }
}

impl Prompts {
    /// 加载模板。
    ///
    /// 目录里缺哪个文件就用内置默认补哪个，不会因为文件缺失而失败。
    pub fn load(dir: &Path) -> Self {
        let mut prompts = Self::default();
        if let Some(text) = read_optional(&dir.join("system.txt")) {
            prompts.system = text;
        }
        if let Some(text) = read_optional(&dir.join("reply_comment.txt")) {
            prompts.reply_comment = text;
        }
        if let Some(text) = read_optional(&dir.join("reply_post.txt")) {
            prompts.reply_post = text;
        }
        prompts
    }

    /// 从配置加载：`HEYBOT_PROMPT_DIR` 优先，否则用项目根的 `prompts/`。
    pub fn from_config() -> Result<Self> {
        if let Some(dir) = crate::config::optional("HEYBOT_PROMPT_DIR") {
            return Ok(Self::load(Path::new(&dir)));
        }
        Ok(Self::load(&crate::store::project_root().join("prompts")))
    }
}

/// 读一个可选的模板文件。
fn read_optional(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.trim_end().to_string())
}

/// 填充占位符。
///
/// `{name}` 替换为对应值；未提供的占位符替换成空串。
/// 渲染后会清掉「标签：」后面为空的整行，避免出现「帖子标题：」这种空壳。
pub fn render(template: &str, values: &HashMap<&str, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            // 没有配对的 `}`，原样输出。
            out.push_str(&rest[start..]);
            return clean_empty_labels(&out);
        };
        let name = &after[..end];
        if name.is_empty() {
            out.push_str(&rest[start..=start + end + 1]);
        } else {
            out.push_str(values.get(name).map(String::as_str).unwrap_or(""));
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    clean_empty_labels(&out)
}

/// 去掉「标签：」后面为空的整行。
///
/// 模板里常写成「帖子标题：{post_title}」，字段缺失时不该留下
/// 一个「帖子标题：」的空行。
fn clean_empty_labels(text: &str) -> String {
    const LABEL: char = '：';
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_end();
            match trimmed.rfind(LABEL) {
                Some(index) => {
                    let label = trimmed[..index].trim();
                    // 「：」是多字节字符，切片要用它的字节长度。
                    let value = trimmed[index + LABEL.len_utf8()..].trim();
                    // 「标签：」后面没内容 → 整行丢掉。
                    !value.is_empty() && !label.is_empty()
                }
                None => true,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 模板里用到的占位符清单（写文档用）。
pub fn placeholder_names() -> Vec<&'static str> {
    vec![
        "post_title",
        "post_author",
        "post_body",
        "target_text",
        "target_comment",
        "target_author",
        "trigger_text",
        "from_user",
    ]
}

/// 校验模板：用一组样例值渲染，确保没有未闭合的占位符。
pub fn validate(template: &str) -> Result<()> {
    let mut values = HashMap::new();
    for name in placeholder_names() {
        values.insert(name, "样例".to_string());
    }
    let rendered = render(template, &values);
    if rendered.contains('{') {
        return Err(Error::Config(format!(
            "prompt 模板里有未闭合的占位符：{rendered}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> HashMap<&'static str, String> {
        let mut values = HashMap::new();
        values.insert("post_title", "标题".to_string());
        values.insert("target_comment", "评论".to_string());
        values.insert("from_user", "张三".to_string());
        values
    }

    #[test]
    fn fills_placeholders() {
        let rendered = render("帖子：{post_title}\n评论：{target_comment}", &values());
        assert_eq!(rendered, "帖子：标题\n评论：评论");
    }

    #[test]
    fn missing_placeholder_becomes_empty() {
        let rendered = render("a{post_title}b{not_provided}c", &values());
        assert_eq!(rendered, "a标题bc");
    }

    #[test]
    fn keeps_text_without_placeholders() {
        let rendered = render("纯文本，没有占位符", &values());
        assert_eq!(rendered, "纯文本，没有占位符");
    }

    #[test]
    fn empty_labels_are_dropped() {
        let mut values = HashMap::new();
        values.insert("post_title", "标题".to_string());
        // 只有 post_title 有值，另外两行的标签后面是空的 → 整行丢掉。
        let rendered = render(
            "帖子标题：{post_title}\n帖子作者：{post_author}\n评论：{target_comment}",
            &values,
        );
        assert_eq!(rendered, "帖子标题：标题");
    }

    #[test]
    fn lines_without_colon_are_kept() {
        let rendered = render("就是一行普通文字", &values());
        assert_eq!(rendered, "就是一行普通文字");
    }

    #[test]
    fn default_templates_are_valid() {
        for template in [DEFAULT_SYSTEM, DEFAULT_REPLY_COMMENT, DEFAULT_REPLY_POST] {
            validate(template).expect("默认模板应当有效");
        }
    }

    #[test]
    fn unclosed_placeholder_is_detected() {
        assert!(validate("未闭合 {post_title").is_err());
    }

    #[test]
    fn loads_templates_from_directory() {
        let dir = std::env::temp_dir().join(format!("heybot-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        std::fs::write(dir.join("system.txt"), "你是{post_title}的老婆").expect("写文件");
        std::fs::write(dir.join("reply_comment.txt"), "评：{target_comment}").expect("写文件");
        // 缺 reply_post.txt，应该回落到内置默认。

        let prompts = Prompts::load(&dir);
        assert_eq!(prompts.system, "你是{post_title}的老婆");
        assert_eq!(prompts.reply_comment, "评：{target_comment}");
        assert_eq!(prompts.reply_post, DEFAULT_REPLY_POST);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_templates_are_valid() {
        // 仓库里的 prompts/ 目录必须始终可用。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
        let prompts = Prompts::load(&dir);
        for template in [
            &prompts.system,
            &prompts.reply_comment,
            &prompts.reply_post,
        ] {
            validate(template).expect("仓库模板应当有效");
        }
    }
}
