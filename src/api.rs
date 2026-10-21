//! 小黑盒 API 客户端。
//!
//! 关键约束：
//! - 串行限速，避免触发风控。
//! - 写请求永不自动重试（可能重复提交）。
//! - 每次请求按路径重新计算签名，不能复用。

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, REFERER, USER_AGENT};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::{Error, Result};
use crate::signing;

pub const API_BASE: &str = "https://api.xiaoheihe.cn";
pub const WEB_BASE: &str = "https://www.xiaoheihe.cn";

/// 请求间隔。官方客户端大约 0.5~1 秒一次。
const REQUEST_INTERVAL: Duration = Duration::from_millis(600);

/// 客户端持有的会话 Cookie。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub cookies: Vec<Cookie>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
}

/// 登录态与账号信息。
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub user_id: String,
    pub nickname: String,
}

pub struct HeyboxApi {
    client: Client,
    /// 自己持有 cookie jar，才能在响应后读回服务端下发的会话。
    jar: Arc<crate::jar::RecordingJar>,
    session: Arc<Mutex<Session>>,
    identity: Arc<Mutex<Option<Identity>>>,
    last_request: Arc<Mutex<Instant>>,
    interval: Duration,
}

impl HeyboxApi {
    pub fn new(session: Session) -> Result<Self> {
        // 启用 cookie store：登录接口通过 Set-Cookie 下发会话，
        // 必须由客户端自己收集，否则拿不到登录态。
        let jar = Arc::new(crate::jar::RecordingJar::default());
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .cookie_provider(Arc::clone(&jar))
            .build()
            .map_err(|error| Error::Network(error.to_string()))?;

        let api = Self {
            client,
            jar,
            session: Arc::new(Mutex::new(session.clone())),
            identity: Arc::new(Mutex::new(None)),
            last_request: Arc::new(Mutex::new(Instant::now() - REQUEST_INTERVAL)),
            interval: REQUEST_INTERVAL,
        };
        api.seed_cookies(&session);
        Ok(api)
    }

    /// 把已知 Cookie 写进 cookie jar。
    fn seed_cookies(&self, session: &Session) {
        for cookie in &session.cookies {
            self.jar.insert(&cookie.name, &cookie.value, &cookie.domain, &cookie.path);
        }
    }

    /// 构造请求头。
    ///
    /// Cookie 交给 cookie store 自动处理：手动拼 `Cookie` 头会覆盖
    /// 服务端新下发的会话。
    fn headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
            ),
        );
        headers.insert(
            REFERER,
            HeaderValue::from_static("https://www.xiaoheihe.cn/"),
        );
        headers.insert("x-requested-with", HeaderValue::from_static("XMLHttpRequest"));
        Ok(headers)
    }

    /// 串行限速。
    async fn throttle(&self) {
        let mut last = self.last_request.lock().await;
        let elapsed = last.elapsed();
        if elapsed < self.interval {
            tokio::time::sleep(self.interval - elapsed).await;
        }
        *last = Instant::now();
    }

    /// 公共查询参数。
    fn base_params(&self, session: &Session) -> Vec<(String, String)> {
        let heybox_id = session
            .cookies
            .iter()
            .find(|cookie| cookie.name == "heybox_id" || cookie.name == "user_heybox_id")
            .map(|cookie| cookie.value.clone())
            .unwrap_or_default();

        vec![
            ("os_type".into(), "web".into()),
            ("app".into(), "heybox".into()),
            ("client_type".into(), "web".into()),
            ("version".into(), "999.0.4".into()),
            ("web_version".into(), "3.0".into()),
            ("x_app".into(), "heybox_website".into()),
            ("x_client_type".into(), "web".into()),
            ("x_os_type".into(), "Mac".into()),
            ("device_info".into(), "Chrome".into()),
            ("heybox_id".into(), heybox_id),
        ]
    }

    /// 发起一次带签名的 GET 请求。
    pub async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        self.get_with_extra(path, params, &[]).await
    }

    /// 发起一次带签名的 GET 请求，并追加额外参数（例如 Workshop 签名）。
    pub async fn get_with_extra(
        &self,
        path: &str,
        params: &[(&str, String)],
        extra: &[(String, String)],
    ) -> Result<Value> {
        let session = self.session.lock().await.clone();
        let mut query = self.base_params(&session);
        for (key, value) in params {
            query.push((key.to_string(), value.clone()));
        }
        signing::SignedParams::for_path(path).apply(&mut query);
        query.extend(extra.iter().cloned());

        let headers = self.headers()?;
        self.throttle().await;
        let response = self
            .client
            .get(format!("{API_BASE}{path}"))
            .headers(headers)
            .query(&query)
            .send()
            .await
            .map_err(|error| Error::Network(error.to_string()))?;

        self.handle(response).await
    }

    /// 发起一次带签名的 POST 表单请求。
    ///
    /// 写请求不自动重试：响应丢失时结果属于未知。
    pub async fn post_form(
        &self,
        path: &str,
        form: &[(String, String)],
        workshop: bool,
    ) -> Result<Value> {
        let session = self.session.lock().await.clone();
        let mut query = self.base_params(&session);
        signing::SignedParams::for_path(path).apply(&mut query);
        if workshop {
            query.extend(signing::workshop_params(&query)?);
        }

        let headers = self.headers()?;
        self.throttle().await;
        let response = self
            .client
            .post(format!("{API_BASE}{path}"))
            .headers(headers)
            .query(&query)
            .form(form)
            .send()
            .await
            .map_err(|error| Error::Network(error.to_string()))?;

        self.handle(response).await
    }

    /// 统一处理响应：小黑盒用 `status` 字段表达业务结果。
    async fn handle(&self, response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(Error::Auth);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(Error::RateLimit);
        }
        let text = response.text().await.map_err(|error| Error::Network(error.to_string()))?;
        let data: Value = serde_json::from_str(&text)
            .map_err(|_| Error::Protocol(format!("响应不是合法 JSON：{text}")))?;

        // 每次响应后同步一次会话：登录接口会在这里下发 Cookie。
        self.refresh_session().await;

        match data.get("status").and_then(Value::as_str) {
            Some("ok") => Ok(data),
            Some("relogin") | Some("not_login") | Some("need_login") | Some("login_required")
            | Some("need_google_check") => Err(Error::Auth),
            Some("show_captcha") | Some("need_captcha") => Err(Error::Captcha),
            Some("failed") | Some("error") => {
                // 不回显上游原始信息，可能包含会话材料。
                let message = data
                    .get("msg")
                    .and_then(Value::as_str)
                    .map(|value| value.chars().take(200).collect::<String>())
                    .unwrap_or_else(|| "小黑盒拒绝了该操作。".into());
                Err(Error::Rejected(message))
            }
            _ => Ok(data),
        }
    }

    /// 从 cookie jar 同步会话 Cookie。
    ///
    /// 记录 jar 里所有域的 Cookie：登录态可能下在 `.xiaoheihe.cn` 父域上，
    /// 只按单个 API 域名取值会漏掉。
    async fn refresh_session(&self) {
        let collected: Vec<Cookie> = self
            .jar
            .all()
            .into_iter()
            .map(|item| Cookie {
                name: item.name,
                value: item.value,
                domain: item.domain,
                path: item.path,
            })
            .collect();
        if collected.is_empty() {
            return;
        }
        let mut session = self.session.lock().await;
        // 合并：新值覆盖同名旧值，保留 jar 里没有的历史项。
        for item in collected {
            match session
                .cookies
                .iter_mut()
                .find(|existing| existing.name == item.name)
            {
                Some(existing) => existing.value = item.value,
                None => session.cookies.push(item),
            }
        }
    }

    /// 读取当前登录身份。
    pub async fn restore_login(&self) -> Result<Identity> {
        let data = self.get("/account/restore_login", &[]).await?;
        // 身份在 result.profile.heybox_id。
        let user_id = crate::auth::read_user_id(&data).ok_or_else(|| {
            Error::Protocol(format!(
                "响应里没有账号标识，原始响应：{}",
                &data.to_string().chars().take(300).collect::<String>()
            ))
        })?;
        let nickname = data
            .pointer("/result/profile/nickname")
            .or_else(|| data.pointer("/profile/nickname"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let identity = Identity { user_id, nickname };
        *self.identity.lock().await = Some(identity.clone());
        Ok(identity)
    }

    /// 读取 @ 我的消息。
    ///
    /// 官方网页端的标签映射（实测自官网 JS）：
    /// - 所有评论 → `list_type=0`
    /// - 点赞 → `list_type=1`
    /// - 关注 → `message_type=4`
    /// - **@我的 → `message_type=16`**
    ///
    /// 注意「@我的」不是 `list_type`，用错参数会拿到「所有评论」或空列表。
    pub async fn mentions(&self, offset: i64, limit: i64) -> Result<Value> {
        self.get(
            "/bbs/app/user/message",
            &[
                ("message_type", "16".into()),
                ("offset", offset.to_string()),
                ("limit", limit.to_string()),
                ("no_more", "false".into()),
            ],
        )
        .await
    }

    /// 读取所有评论类消息（回复与评论）。
    pub async fn messages(&self, list_type: i64, offset: i64, limit: i64) -> Result<Value> {
        self.get(
            "/bbs/app/user/message",
            &[
                ("list_type", list_type.to_string()),
                ("offset", offset.to_string()),
                ("limit", limit.to_string()),
                ("no_more", "false".into()),
            ],
        )
        .await
    }

    /// 读取帖子根评论。
    pub async fn post_comments(&self, link_id: &str, offset: i64, limit: i64) -> Result<Value> {
        self.get(
            "/bbs/app/link/tree",
            &[
                ("link_id", link_id.to_string()),
                ("offset", offset.to_string()),
                ("limit", limit.to_string()),
            ],
        )
        .await
    }

    /// 读取楼中楼回复。
    pub async fn sub_comments(
        &self,
        link_id: &str,
        root_comment_id: &str,
        lastval: &str,
        limit: i64,
    ) -> Result<Value> {
        self.get(
            "/bbs/app/comment/sub/comments",
            &[
                ("link_id", link_id.to_string()),
                ("root_comment_id", root_comment_id.to_string()),
                ("lastval", lastval.to_string()),
                ("limit", limit.to_string()),
            ],
        )
        .await
    }

    /// 发布评论或回复。
    ///
    /// `reply_id` / `root_id` 传 `-1` 表示顶层评论。
    pub async fn create_comment(
        &self,
        link_id: &str,
        text: &str,
        reply_id: &str,
        root_id: &str,
    ) -> Result<Value> {
        self.post_form(
            "/bbs/app/comment/create",
            &[
                ("link_id".into(), link_id.to_string()),
                ("text".into(), text.to_string()),
                ("is_cy".into(), "0".into()),
                ("reply_id".into(), reply_id.to_string()),
                ("root_id".into(), root_id.to_string()),
            ],
            true,
        )
        .await
    }

    /// 当前身份。
    pub async fn identity(&self) -> Option<Identity> {
        self.identity.lock().await.clone()
    }

    /// 会话快照。
    pub async fn session(&self) -> Session {
        self.session.lock().await.clone()
    }
}
