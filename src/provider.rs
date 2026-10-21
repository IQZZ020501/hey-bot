//! 小黑盒平台的 Puppet 实现。
//!
//! 这里是小黑盒的协议细节唯一该出现的地方：评论树怎么翻、
//! 新评论 id 藏在响应的哪一层、回复要带哪些 id。

use async_trait::async_trait;
use serde_json::Value;

use crate::api::HeyboxApi;
use crate::context::{self, Context};
use crate::error::Result;
use crate::mentions::Mention;
use crate::puppet::Puppet;

/// 小黑盒平台。
pub struct XiaoheihePuppet {
    api: HeyboxApi,
}

impl XiaoheihePuppet {
    /// 用一个已登录的 API 客户端构造。
    pub fn new(api: HeyboxApi) -> Self {
        Self { api }
    }

    /// 底层客户端，供需要直接访问接口的场景使用。
    pub fn api(&self) -> &HeyboxApi {
        &self.api
    }
}

#[async_trait]
impl Puppet for XiaoheihePuppet {
    async fn mentions(&self, offset: i64, limit: i64) -> Result<Vec<Mention>> {
        let response = self.api.mentions(offset, limit).await?;
        Ok(crate::mentions::parse_mentions(&response))
    }

    async fn target_exists(&self, mention: &Mention) -> Result<bool> {
        let (reply_id, root_id) = mention.reply_targets();
        if reply_id == "-1" {
            // 顶层评论：目标就是主楼。
            return Ok(true);
        }
        // 用楼层收集统一判断：主楼在 link/tree 里，楼中楼在 sub_comments 里。
        // 注意 commentid 是数字，不能用 as_str() 比较。
        let floor = context::collect_floor(&self.api, &mention.link_id, root_id).await?;
        Ok(floor.iter().any(|item| item.id == reply_id))
    }

    async fn context(&self, mention: &Mention) -> Result<Context> {
        context::collect(&self.api, mention).await
    }

    async fn say(&self, mention: &Mention, text: &str) -> Result<String> {
        let (reply_id, root_id) = mention.reply_targets();
        let data = self
            .api
            .create_comment(&mention.link_id, text, reply_id, root_id)
            .await?;
        Ok(extract_comment_id(&data))
    }
}

/// 从创建评论的响应里取新评论 id。
///
/// 服务端把 id 放在**响应顶层**的 `commentid`，不同版本偶尔挪到 `result` 里。
/// 注意它是 JSON 数字，所以只能按字符串比对。
pub fn extract_comment_id(data: &Value) -> String {
    context::numeric_id(data.get("commentid"))
        .or_else(|| context::numeric_id(data.pointer("/result/commentid")))
        .or_else(|| context::numeric_id(data.pointer("/result/comment/comment/0/commentid")))
        .unwrap_or_default()
}
