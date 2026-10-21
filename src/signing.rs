//! 小黑盒网页客户端的请求签名。
//!
//! 这是公开网页协议，不是身份认证：签名只证明"这个请求来自官方网页客户端"，
//! 真正的身份仍然由 Cookie 承担。
//!
//! 参考实现（用于对拍验证）：
//! `xiaoheihe-cli` 的 `xhh_operator/signing.py`。

use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use rand::RngCore;
use sha2::Sha256;

use crate::error::{Error, Result};

/// 签名字符集，共 35 个字符。
///
/// 切片含义（与网页端一致）：
/// - 完整字符集用于路径与 nonce 分量。
/// - 去掉末尾 2 个字符用于时间分量。
/// - 去掉末尾 4 个字符用于前缀映射。
const ALPHABET: &[u8; 35] = b"AB45STUVWZEFGJ6CH01D237IXYPQRKLMN89";

/// Workshop 写接口使用的固定盐。
const WORKSHOP_SALT: &str = "Z7mFG4tQp9Ws2LxB8H";

/// 写接口的 Workshop 附加签名版本。
const WORKSHOP_VERSION: &str = "15";

fn md5_hex(value: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

/// 把字符按 `ord % len` 映射到字符集。
fn map_chars(value: &str, alphabet: &[u8]) -> String {
    value
        .chars()
        .map(|c| alphabet[(c as usize) % alphabet.len()] as char)
        .collect()
}

/// `x * 2`（GF(2^8) 内的 "xtime"）。
fn double(value: u8) -> u8 {
    if value & 0x80 != 0 {
        (value << 1) ^ 27
    } else {
        value << 1
    }
}

fn three(value: u8) -> u8 {
    double(value) ^ value
}

fn nine(value: u8) -> u8 {
    three(double(value))
}

fn eleven(value: u8) -> u8 {
    nine(three(double(value)))
}

fn thirteen(value: u8) -> u8 {
    eleven(value) ^ nine(value) ^ three(value)
}

/// 计算路径签名 `hkey`。
///
/// 流程：路径标准化 → 三个分量分别做字符映射 → 交错混合 → 取前 20 位做 MD5
/// → 前 5 位映射为前缀，后 6 位做 Rijndael 混合并求校验和。
pub fn sign(path: &str, timestamp: i64, nonce: &str) -> String {
    // 路径标准化：去掉空段，首尾各补一个斜杠。
    let normalized = format!(
        "/{}/",
        path.split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    );

    // 时间分量用 +1，与网页端保持一致。
    let time_part = map_chars(&(timestamp + 1).to_string(), &ALPHABET[..ALPHABET.len() - 2]);
    let path_part = map_chars(&normalized, ALPHABET);
    let nonce_part = map_chars(nonce, ALPHABET);

    // 三个分量交错混合。
    let parts = [time_part, path_part, nonce_part];
    let max_len = parts.iter().map(String::len).max().unwrap_or(0);
    let mut interleaved = String::with_capacity(max_len * 3);
    for index in 0..max_len {
        for part in &parts {
            if let Some(ch) = part.chars().nth(index) {
                interleaved.push(ch);
            }
        }
    }

    // 只取前 20 个字符参与哈希。
    let seed: String = interleaved.chars().take(20).collect();
    let digest = md5_hex(&seed);
    let digest_bytes = digest.as_bytes();

    // 后 6 位参与 Rijndael 混合。
    let a = digest_bytes[digest_bytes.len() - 6];
    let b = digest_bytes[digest_bytes.len() - 5];
    let c = digest_bytes[digest_bytes.len() - 4];
    let d = digest_bytes[digest_bytes.len() - 3];
    let mixed = [
        thirteen(a) ^ eleven(b) ^ nine(c) ^ three(d),
        three(a) ^ thirteen(b) ^ eleven(c) ^ nine(d),
        nine(a) ^ three(b) ^ thirteen(c) ^ eleven(d),
        eleven(a) ^ nine(b) ^ three(c) ^ thirteen(d),
    ];

    // JS 实现保留了两个未参与变换的尾字节。
    let tail = digest_bytes[digest_bytes.len() - 2..]
        .iter()
        .map(|byte| u32::from(*byte))
        .sum::<u32>();
    let checksum = (mixed.iter().map(|value| u32::from(*value)).sum::<u32>() + tail) % 100;

    let prefix = map_chars(&digest[..5], &ALPHABET[..ALPHABET.len() - 4]);
    format!("{prefix}{checksum:02}")
}

/// 随机 nonce：MD5(时间戳 + 随机十六进制)，大写。
pub fn random_nonce(timestamp: i64) -> String {
    let mut buffer = [0u8; 16];
    rand::rng().fill_bytes(&mut buffer);
    let seed = format!("{timestamp}{}", hex::encode(buffer));
    md5_hex(&seed).to_uppercase()
}

/// 一条带签名的 GET 请求的查询参数。
#[derive(Debug, Clone)]
pub struct SignedParams {
    pub hkey: String,
    pub time: i64,
    pub nonce: String,
}

impl SignedParams {
    /// 为指定路径生成一组签名参数。
    pub fn for_path(path: &str) -> Self {
        Self::for_path_at(path, now_seconds())
    }

    /// 为指定路径在指定时刻生成一组签名参数（测试用）。
    pub fn for_path_at(path: &str, timestamp: i64) -> Self {
        let nonce = random_nonce(timestamp);
        let hkey = sign(path, timestamp, &nonce);
        Self {
            hkey,
            time: timestamp,
            nonce,
        }
    }

    /// 追加到查询参数里。
    pub fn apply(&self, params: &mut Vec<(String, String)>) {
        params.push(("hkey".into(), self.hkey.clone()));
        params.push(("_time".into(), self.time.to_string()));
        params.push(("nonce".into(), self.nonce.clone()));
        params.push(("time_".into(), self.time.to_string()));
    }
}

/// 网页写接口的 Workshop 附加签名 `_rnd`。
pub fn workshop_params(params: &[(String, String)]) -> Result<Vec<(String, String)>> {
    let nonce = params
        .iter()
        .find(|(key, _)| key == "nonce")
        .map(|(_, value)| value.clone())
        .ok_or_else(|| Error::Protocol("签名参数缺少 nonce。".into()))?;
    let stamp = params
        .iter()
        .find(|(key, _)| key == "_time")
        .map(|(_, value)| value.clone())
        .ok_or_else(|| Error::Protocol("签名参数缺少 _time。".into()))?;

    let message = format!("{WORKSHOP_SALT}{nonce}{stamp}:{nonce}");
    let mut mac = Hmac::<Sha256>::new_from_slice(WORKSHOP_SALT.as_bytes())
        .map_err(|error| Error::Protocol(format!("Workshop 签名失败：{error}")))?;
    mac.update(message.as_bytes());
    let digest = hex::encode(mac.finalize().into_bytes());

    Ok(vec![("_rnd".into(), format!("{WORKSHOP_VERSION}:{digest}"))])
}

/// 当前 Unix 秒。
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 `xiaoheihe-cli` 的 Python 实现对拍。
    ///
    /// 期望值由 Python 参考实现生成（`tools/expected_hkeys.py`，该脚本未入库），
    /// 不是手写的，避免"用 Rust 验证 Rust"的假阳性。
    #[test]
    fn sign_matches_python_reference() {
        let cases = [
            (
                "/bbs/app/api/emojis/list",
                1770287253,
                "2B81EA33B45BA438DB1E4DFC5375B5D6",
                "S30IS57",
            ),
            (
                "/bbs/app/user/message",
                1770529394,
                "3D6891C5FEBD5A0A4C986784D2F60E81",
                "VYZ1T62",
            ),
            (
                "/account/restore_login",
                1770529394,
                "3D6891C5FEBD5A0A4C986784D2F60E81",
                "3XX1P62",
            ),
            (
                "/bbs/app/comment/create",
                1760000000,
                "0123456789ABCDEF0123456789ABCDEF",
                "0WSSX09",
            ),
            (
                "/bbs/app/link/tree",
                1750000000,
                "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
                "X0DWX51",
            ),
            (
                "/bbs/app/comment/sub/comments",
                1740000000,
                "00000000000000000000000000000000",
                "P3Y3I61",
            ),
            (
                "/account/get_qrcode_url/",
                1730000000,
                "ABCDEF0123456789ABCDEF0123456789",
                "YXT0I46",
            ),
            ("/bbs/app/user/message", 1770529394, "ABCDEF", "I2Y0P70"),
            ("/account/restore_login", 1770529394, "ABCDEF", "I0TT172"),
            ("/a/b/c", 1700000000, "xyz", "ST7ZW50"),
        ];
        for (path, timestamp, nonce, expected) in cases {
            assert_eq!(
                sign(path, timestamp, nonce),
                expected,
                "签名与 Python 参考实现不一致：{path}"
            );
        }
    }

    #[test]
    fn sign_is_stable_and_path_dependent() {
        let a = sign("/bbs/app/user/message", 1770529394, "ABCDEF");
        let b = sign("/bbs/app/user/message", 1770529394, "ABCDEF");
        assert_eq!(a, b, "同输入必须得到同签名");

        // 上游算法对路径只有弱绑定：交错串只取前 20 位，
        // 路径分量只贡献前 10 位。实测 `/bbs/app/user/message`
        // 与 `/bbs/app/comment/create` 落在同一前缀上，签名相同；
        // 这是网页端协议的真实行为，必须照搬。
        let same_prefix = sign("/bbs/app/comment/create", 1770529394, "ABCDEF");
        assert_eq!(
            a, same_prefix,
            "同前缀路径的签名相同，这是上游协议的真实行为"
        );
        // 换一个顶级目录，前缀就不同，签名随之改变。
        let different_prefix = sign("/account/restore_login", 1770529394, "ABCDEF");
        assert_ne!(a, different_prefix, "不同前缀必须得到不同签名");

        // nonce 落在 seed 末尾，会影响签名。
        assert_ne!(a, sign("/bbs/app/user/message", 1770529394, "ABCDEG"));
    }

    /// 记录上游协议的一个反直觉行为：相邻秒的时间戳可能得到相同签名。
    ///
    /// 时间分量的末位往往被路径分量挤出 seed 窗口，所以 `hkey` 对时间戳
    /// 并不总是敏感。调用方必须每次请求都重新生成 nonce 与时间戳，
    /// 不能指望签名本身能区分同一秒内的两个请求。
    #[test]
    fn adjacent_timestamps_may_share_a_signature() {
        let a = sign("/bbs/app/user/message", 1770529394, "ABCDEF");
        let b = sign("/bbs/app/user/message", 1770529395, "ABCDEF");
        // 这不是断言"必须相同"，而是把实测行为固定下来：
        // 若上游将来修正了这一点，这条测试会提醒我们更新文档。
        assert_eq!(
            a, b,
            "当前上游实现下相邻秒签名相同；这与 seed 只取前 20 位有关"
        );
    }

    #[test]
    fn workshop_signature_has_expected_shape() {
        let params = vec![
            ("nonce".to_string(), "3D6891C5FEBD5A0A4C986784D2F60E81".to_string()),
            ("_time".to_string(), "1770529394".to_string()),
        ];
        let extra = workshop_params(&params).expect("Workshop 签名");
        assert_eq!(extra.len(), 1);
        assert_eq!(extra[0].0, "_rnd");
        assert!(extra[0].1.starts_with("15:"));
        assert_eq!(extra[0].1.len(), 3 + 64);
    }
}
