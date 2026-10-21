//! 会话 Cookie 存储。
//!
//! reqwest 自带的 Jar 没有"列出全部 Cookie"的接口，而登录态可能下在
//! `.xiaoheihe.cn` 这样的父域上，只按单个 API 域名取值会漏掉。
//! 这里自己实现 `CookieStore`：记录所有下发的 Cookie，并按请求 URL 正确筛选。

use std::sync::RwLock;

use reqwest::cookie::CookieStore;
use reqwest::header::HeaderValue;

/// 一条记录下来的 Cookie。
#[derive(Debug, Clone)]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
}

/// 记录全部 Cookie 的 jar。
#[derive(Debug, Default)]
pub struct RecordingJar {
    cookies: RwLock<Vec<StoredCookie>>,
}

impl RecordingJar {
    /// 全部记录下来的 Cookie。
    pub fn all(&self) -> Vec<StoredCookie> {
        self.cookies
            .read()
            .map(|items| items.clone())
            .unwrap_or_default()
    }

    /// 手动写入一条 Cookie。
    pub fn insert(&self, name: &str, value: &str, domain: &str, path: &str) {
        let mut guard = match self.cookies.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.retain(|item| !(item.name == name && item.domain == domain));
        guard.push(StoredCookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: domain.to_string(),
            path: path.to_string(),
            secure: true,
        });
    }

    /// Cookie 的域是否覆盖请求主机。
    fn domain_matches(cookie_domain: &str, host: &str) -> bool {
        let cookie_domain = cookie_domain.trim_start_matches('.').to_ascii_lowercase();
        let host = host.to_ascii_lowercase();
        if cookie_domain.is_empty() {
            return true;
        }
        host == cookie_domain || host.ends_with(&format!(".{cookie_domain}"))
    }
}

impl CookieStore for RecordingJar {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &url::Url) {
        let host = url.host_str().unwrap_or_default().to_string();
        let mut guard = match self.cookies.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        for header in headers {
            let Ok(text) = header.to_str() else {
                continue;
            };
            let mut parts = text.split(';');
            let Some(pair) = parts.next() else {
                continue;
            };
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            if name.is_empty() {
                continue;
            }

            let mut domain = String::new();
            let mut path = "/".to_string();
            let mut secure = false;
            for attribute in parts {
                let attribute = attribute.trim();
                let (key, val) = attribute.split_once('=').unwrap_or((attribute, ""));
                match key.to_ascii_lowercase().as_str() {
                    "domain" => domain = val.trim().to_string(),
                    "path" => path = val.trim().to_string(),
                    "secure" => secure = true,
                    _ => {}
                }
            }
            if domain.is_empty() {
                // 没有 Domain 属性的 Cookie 属于当前主机。
                domain = host.clone();
            }

            // 同名同域覆盖。
            guard.retain(|item| !(item.name == name && item.domain == domain && item.path == path));
            guard.push(StoredCookie {
                name,
                value,
                domain,
                path,
                secure,
            });
        }
    }

    fn cookies(&self, url: &url::Url) -> Option<HeaderValue> {
        let host = url.host_str().unwrap_or_default();
        let path = url.path();
        let guard = self.cookies.read().ok()?;
        let mut text = String::new();
        for cookie in guard.iter() {
            if !Self::domain_matches(&cookie.domain, host) {
                continue;
            }
            if !path.starts_with(cookie.path.as_str()) {
                continue;
            }
            if cookie.secure && url.scheme() != "https" {
                continue;
            }
            if !text.is_empty() {
                text.push_str("; ");
            }
            text.push_str(&cookie.name);
            text.push('=');
            text.push_str(&cookie.value);
        }
        if text.is_empty() {
            None
        } else {
            HeaderValue::from_str(&text).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> url::Url {
        url::Url::parse(value).expect("URL 合法")
    }

    #[test]
    fn records_cookies_from_any_domain() {
        let jar = RecordingJar::default();
        let headers = [
            HeaderValue::from_static("user_heybox_id=42; Domain=.xiaoheihe.cn; Path=/"),
            HeaderValue::from_static("pkey=pk; Domain=.xiaoheihe.cn; Path=/"),
        ];
        let mut iter = headers.iter();
        jar.set_cookies(
            &mut iter,
            &url("https://api.xiaoheihe.cn/account/qr_state/"),
        );

        let all = jar.all();
        assert_eq!(all.len(), 2);
        assert!(
            all.iter()
                .any(|item| item.name == "user_heybox_id" && item.value == "42")
        );
    }

    #[test]
    fn parent_domain_cookies_are_sent_to_subdomain() {
        let jar = RecordingJar::default();
        let headers = [HeaderValue::from_static(
            "session=abc; Domain=.xiaoheihe.cn; Path=/",
        )];
        let mut iter = headers.iter();
        jar.set_cookies(&mut iter, &url("https://api.xiaoheihe.cn/account/"));

        let value = jar.cookies(&url("https://api.xiaoheihe.cn/account/restore_login"));
        assert_eq!(
            value.and_then(|item| item.to_str().ok().map(str::to_string)),
            Some("session=abc".to_string())
        );
    }

    #[test]
    fn unrelated_domain_cookies_are_not_sent() {
        let jar = RecordingJar::default();
        let headers = [HeaderValue::from_static(
            "other=1; Domain=.example.com; Path=/",
        )];
        let mut iter = headers.iter();
        jar.set_cookies(&mut iter, &url("https://api.example.com/"));
        assert!(jar.cookies(&url("https://api.xiaoheihe.cn/")).is_none());
    }

    #[test]
    fn same_name_on_same_domain_is_replaced() {
        let jar = RecordingJar::default();
        for value in ["old", "new"] {
            let headers = [HeaderValue::from_str(&format!(
                "heybox_id={value}; Domain=.xiaoheihe.cn; Path=/"
            ))
            .unwrap()];
            let mut iter = headers.iter();
            jar.set_cookies(&mut iter, &url("https://api.xiaoheihe.cn/"));
        }
        let all = jar.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].value, "new");
    }
}
