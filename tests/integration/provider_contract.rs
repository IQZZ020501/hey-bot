//! 小黑盒实现的纯函数契约。
//!
//! `XiaoheihePuppet` 内部包着具体的 `HeyboxApi`，没法直接造假客户端；
//! 但「从响应里取评论 id」这段纯逻辑可以单独测。

use heybot::provider::extract_comment_id;
use serde_json::json;

#[test]
fn 从响应顶层取评论id() {
    let data = json!({
        "status": "ok",
        "commentid": 980772669,
        "result": { "comment": { "comment": [{ "commentid": 111 }] } }
    });
    assert_eq!(extract_comment_id(&data), "980772669");
}

#[test]
fn 顶层没有时退回_result_里() {
    let data = json!({
        "status": "ok",
        "result": { "commentid": 980772670 }
    });
    assert_eq!(extract_comment_id(&data), "980772670");
}

#[test]
fn 再退回嵌套的评论数组() {
    let data = json!({
        "status": "ok",
        "result": { "comment": { "comment": [{ "commentid": 980772671 }] } }
    });
    assert_eq!(extract_comment_id(&data), "980772671");
}

#[test]
fn 评论id是数字也要按字符串取() {
    let data = json!({ "commentid": 980772672 });
    assert_eq!(extract_comment_id(&data), "980772672");
}

#[test]
fn 响应里没有评论id时返回空串() {
    let data = json!({ "status": "ok", "msg": "没有评论" });
    assert_eq!(extract_comment_id(&data), "");
}
