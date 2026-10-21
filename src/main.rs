//! heybot：小黑盒 @ 自动回复。
//!
//! 子命令：
//! - `login`：扫码登录，保存会话。
//! - `whoami`：显示当前登录身份。
//! - `mentions`：拉取一次 @ 消息并打印（调试用，不回复）。
//! - `reply`：拉取并自动回复一次。
//! - `watch`：常驻轮询，持续自动回复。

use heybot::auth;
use heybot::mentions;
use heybot::probe;
use heybot::reply;
use heybot::store;
use heybot::watch;

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;

use heybot::ai::AiClient;
use heybot::api::HeyboxApi;
use heybot::bot::Bot;
use heybot::error::Error;
use heybot::provider::XiaoheihePuppet;
use heybot::puppet::Puppet;
use heybot::reply::ReplyOutcome;

/// 等会话文件时多久检查一次。
const SESSION_WAIT_INTERVAL: Duration = Duration::from_secs(15);

fn usage() -> &'static str {
    "用法：
  heybot login              扫码登录并保存会话
  heybot whoami             显示当前登录身份
  heybot mentions           拉取一次 @ 消息并打印
  heybot reply              拉取并自动回复一次
  heybot preview            渲染 prompt 并预览 AI 回复（不发送）
  heybot reply --latest     只回最新一条（安全验证用）
  heybot watch              常驻监听，持续自动回复

环境变量：
  HEYBOT_AI_BASE_URL     OpenAI 兼容地址，默认 https://api.openai.com/v1
  HEYBOT_AI_API_KEY      AI 接口密钥
  HEYBOT_AI_MODEL        模型名
  HEYBOT_AI_TIMEOUT_SECONDS  AI 超时，默认 60
  HEYBOT_POLL_SECONDS    watch 轮询间隔，默认 20
  HEYBOT_ONLY_USER       只回复指定用户发的消息（默认不过滤）
  HEYBOT_REPLY_HISTORY   设置后 watch 也会回复历史消息（默认只回新消息）
  HEYBOT_SESSION_FILE    会话文件路径
  HEYBOT_DEDUP_FILE      去重记录路径
"
}

#[tokio::main]
async fn main() -> Result<()> {
    // 先加载 .env，再读任何环境变量。
    heybot::config::load();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    if let Some(path) = heybot::config::env_file() {
        heybot::config::warn_if_world_readable(&path);
        tracing::info!("已加载配置：{}", path.display());
    }

    let args: Vec<String> = std::env::args().collect();
    // 无参数时默认进入 watch：没登录就扫码，登录了就常驻。
    let command = args.get(1).map(String::as_str).unwrap_or("watch");

    match command {
        "login" => cmd_login().await,
        "whoami" => cmd_whoami().await,
        "mentions" => cmd_mentions().await,
        "reply" => cmd_reply().await,
        "preview" => cmd_preview().await,
        "watch" => cmd_watch().await,
        "probe" => cmd_probe().await,
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(())
        }
        other => {
            eprintln!("未知子命令：{other}\n");
            println!("{}", usage());
            Ok(())
        }
    }
}

/// 签名连通性自检：不需要登录态。
async fn cmd_probe() -> Result<()> {
    let api = HeyboxApi::new(Default::default())?;
    for path in [
        "/account/restore_login",
        "/bbs/app/api/emojis/list",
        "/bbs/app/user/message",
    ] {
        let result = probe::probe(&api, path).await?;
        let verdict = if result.signature_rejected {
            "签名被拒"
        } else {
            "签名通过"
        };
        println!(
            "{verdict}  {}  status={} msg={}",
            result.path, result.status, result.message
        );
    }
    Ok(())
}

/// 注册默认处理器：调 AI 生成评价，然后回复这条消息。
///
/// 幂等与「结果未知不重发」由 [`heybot::bot::Message::say`] 保证。
fn register_auto_reply(bot: &mut Bot, ai: &AiClient, policy: &reply::ReplyPolicy) {
    let ai = ai.clone();
    let system = policy.prompts.system.clone();
    bot.on_message(move |msg| {
        let ai = ai.clone();
        let system = system.clone();
        async move {
            if msg.is_replied().await {
                return Ok(ReplyOutcome::Skipped);
            }
            let text = match ai.complete(&system, msg.prompt()).await {
                Ok(text) => text,
                Err(error) => {
                    // AI 失败**不写去重表**：模型超时、网络抖动都可能是暂时的，
                    // 写进去等于永久放弃这条 @。留给下一轮重试。
                    return Ok(ReplyOutcome::AiFailed(error.to_string()));
                }
            };
            msg.say(&text).await
        }
    });
}

/// 构造带会话的 API 客户端。
///
/// 会话无效时自动扫码登录一次，避免用户手动分步执行。
async fn client() -> Result<HeyboxApi> {
    let path = store::session_path();
    let session = store::load_session(&path)?;
    if session.cookies.is_empty() {
        // 没有会话：直接扫码。
        let session = login_interactive().await?;
        store::save_session(&path, &session)?;
        return Ok(HeyboxApi::new(session)?);
    }
    let api = HeyboxApi::new(session.clone())?;
    match api.restore_login().await {
        Ok(_) => Ok(api),
        Err(Error::Auth) => {
            tracing::info!("会话已失效，重新扫码登录。");
            let session = login_interactive().await?;
            store::save_session(&path, &session)?;
            Ok(HeyboxApi::new(session)?)
        }
        Err(error) => {
            // 网络等临时问题不该触发重新登录。
            tracing::warn!("校验会话失败（{error}），先用现有会话继续。");
            Ok(api)
        }
    }
}

/// 交互式扫码登录，返回可用会话。
///
/// **二维码过期会自动换一张继续显示**——不用守着终端刷新，
/// 什么时候扫什么时候算数。
async fn login_interactive() -> Result<heybot::api::Session> {
    let api = HeyboxApi::new(Default::default())?;
    let session = auth::login_loop(
        || async { auth::fetch_qrcode(&api).await },
        |qr: &auth::QrCode| {
            let qr = qr.clone();
            async move { show_qr(&qr) }
        },
        |qr: &auth::QrCode| {
            let qr = qr.clone();
            let api = &api;
            async move { auth::wait_for_login(api, &qr).await }
        },
    )
    .await?;
    Ok(session)
}

/// 展示一张二维码。
fn show_qr(qr: &auth::QrCode) {
    let svg_path = std::env::temp_dir().join(format!("heybot-qr-{}.svg", std::process::id()));
    println!();
    if let Some(terminal) = render_terminal_qr(&qr.url) {
        println!("{terminal}");
    } else if let Ok(svg) = auth::qr_svg(&qr.url)
        && std::fs::write(&svg_path, svg).is_ok()
    {
        println!("当前终端不支持二维码渲染，请打开这个文件：");
        println!("  {}", svg_path.display());
    }
    println!("请用小黑盒 App 扫描上面的二维码。");
    println!("也可直接打开：{}", qr.url);
    println!("有效期 {} 秒，过期会自动换一张。", qr.expire_seconds);
}

/// 当前是不是交互式环境（有 TTY）。
///
/// 服务跑在 systemd 下时 stdin 不是终端，画不出二维码也没人扫，
/// 这时候应该等运维换好会话文件，而不是反复拉二维码。
fn is_interactive() -> bool {
    std::io::stdin().is_terminal()
}

/// 等到收到退出信号（Ctrl-C / SIGTERM / SIGINT）。
async fn shutdown_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("注册 SIGTERM 处理器");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

/// 确保有可用会话：已登录就复用，没登录或已失效就扫码。
///
/// 返回会话与账号昵称。
async fn ensure_session() -> Result<(heybot::api::Session, String)> {
    let path = store::session_path();
    let session = store::load_session(&path)?;

    if !session.cookies.is_empty() {
        match HeyboxApi::new(session.clone())?.restore_login().await {
            Ok(identity) => {
                tracing::info!("已登录：{}（{}）", identity.nickname, identity.user_id);
                return Ok((session, identity.nickname));
            }
            Err(Error::Auth) => {
                tracing::info!("会话已失效，重新扫码登录。");
            }
            Err(error) => {
                // 网络之类的临时问题不该触发重新登录，先用现有会话继续。
                tracing::warn!("校验会话失败（{}），先用现有会话继续。", error);
                return Ok((session, String::new()));
            }
        }
    }

    if !is_interactive() {
        // 服务环境：没有终端，画不出二维码也没人扫。
        // 不退出、不反复拉二维码——等运维把会话文件换好就行。
        tracing::error!(
            "会话已失效，且当前不是交互式环境，无法显示二维码。\n\
             请 SSH 登录服务器后执行：sudo -u heybot -H {} login\n\
             换好会话文件后服务会自动恢复，不用重启。",
            path.display()
        );
        let session = tokio::select! {
            result = auth::wait_for_session(&path, SESSION_WAIT_INTERVAL, |session| async move {
                match HeyboxApi::new(session) {
                    Ok(api) => api.restore_login().await.is_ok(),
                    Err(_) => false,
                }
            }) => result?,
            _ = shutdown_signal() => return Err(Error::Auth.into()),
        };
        tracing::info!("会话已更新，继续。");
        let nickname = HeyboxApi::new(session.clone())?
            .restore_login()
            .await
            .map(|value| value.nickname)
            .unwrap_or_default();
        return Ok((session, nickname));
    }

    tracing::info!("尚未登录，开始扫码登录。");
    let session = login_interactive().await?;
    store::save_session(&path, &session)?;
    tracing::info!("登录成功，会话已保存到 {}", path.display());

    let nickname = HeyboxApi::new(session.clone())?
        .restore_login()
        .await
        .map(|value| value.nickname)
        .unwrap_or_default();
    Ok((session, nickname))
}

async fn cmd_login() -> Result<()> {
    let session = login_interactive().await?;
    let path = store::session_path();
    store::save_session(&path, &session)?;
    let user_id = HeyboxApi::new(session.clone())?
        .restore_login()
        .await
        .map(|value| value.user_id)
        .unwrap_or_default();
    println!("登录成功：{user_id}");
    println!("会话已保存到 {}", path.display());
    Ok(())
}

/// 在终端画二维码。终端不支持 UTF-8 半块字符时返回 None。
fn render_terminal_qr(content: &str) -> Option<String> {
    // 终端宽度不够时画出来会被折行，扫不出来。
    if console_width() < 45 {
        return None;
    }
    let rendered = auth::qr_terminal(content).ok()?;
    Some(rendered)
}

/// 读取终端宽度，失败时给一个保守值。
fn console_width() -> usize {
    std::process::Command::new("tput")
        .arg("cols")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse::<usize>().ok())
        .unwrap_or(80)
}

async fn cmd_whoami() -> Result<()> {
    let api = client().await?;
    let identity = api.restore_login().await?;
    println!("用户 ID：{}", identity.user_id);
    if !identity.nickname.is_empty() {
        println!("昵称：{}", identity.nickname);
    }
    Ok(())
}

async fn cmd_mentions() -> Result<()> {
    let api = client().await?;
    let response = api.mentions(0, 50).await?;
    let found = mentions::parse_mentions(&response);
    println!("解析到 {} 条带帖子的消息：", found.len());
    for item in &found {
        let (reply_id, root_id) = item.reply_targets();
        println!(
            "  [type={}] {} @{}：{}\n      帖子 {}（{}）回复 {}/根 {}",
            item.message_type,
            item.from_user,
            item.reply_id,
            item.quote,
            item.link_id,
            item.link_title,
            reply_id,
            root_id
        );
    }
    if found.is_empty() {
        println!(
            "（原始响应片段：{}）",
            response.to_string().chars().take(600).collect::<String>()
        );
    }
    Ok(())
}

/// 预演：渲染 prompt 并让 AI 生成回复，但不发送。
///
/// 调 prompt 时用这个看效果，不用每次都真发评论。
///
/// 三种输入来源：
/// - 默认：最新一条真实的 @ 消息
/// - `--text "<内容>"`：把任意文本当作待评价的评论
/// - `--post <帖子 id>`：把某个帖子当作待评价的对象
async fn cmd_preview() -> Result<()> {
    let ai = AiClient::new(heybot::ai::AiConfig::from_env()?)?;
    let policy = reply::policy_with_prompts();

    let args: Vec<String> = std::env::args().collect();
    let text_arg = flag_value(&args, "--text");
    let post_arg = flag_value(&args, "--post");

    let context = if let Some(text) = text_arg {
        // 自带内容：不需要真实 @ 消息，也不联网。
        heybot::context::Context {
            target_comment: text,
            target_author: "某人".into(),
            trigger_text: "@heybot 锐评一下".into(),
            target_kind: heybot::context::TargetKind::Comment,
            ..Default::default()
        }
    } else {
        // 需要读接口的场景才建客户端。
        let puppet = XiaoheihePuppet::new(client().await?);
        if let Some(link_id) = post_arg {
            let mention = mentions::Mention {
                message_id: "preview".into(),
                message_type: "17".into(),
                link_id: link_id.clone(),
                link_title: String::new(),
                root_id: String::new(),
                reply_id: String::new(),
                quote: "@heybot 锐评一下".into(),
                from_user: "某人".into(),
                from_user_id: String::new(),
                timestamp: String::new(),
                link_url: String::new(),
                comment_a_id: String::new(),
            };
            puppet.context(&mention).await?
        } else {
            let mut found = puppet.mentions(0, 20).await?;
            found.sort_by(|a, b| {
                b.timestamp
                    .parse::<f64>()
                    .unwrap_or(0.0)
                    .total_cmp(&a.timestamp.parse::<f64>().unwrap_or(0.0))
            });
            let Some(mention) = found.into_iter().find(mentions::is_at_mention) else {
                println!("没有待回复的 @ 消息。");
                println!("可以自带内容来预览：");
                println!("  heybot preview --text \"要评价的评论内容\"");
                println!("  heybot preview --post <帖子 id>");
                return Ok(());
            };
            puppet.context(&mention).await?
        }
    };

    let mention = mentions::Mention {
        message_id: "preview".into(),
        message_type: "17".into(),
        link_id: String::new(),
        link_title: context.post_title.clone(),
        root_id: String::new(),
        reply_id: String::new(),
        quote: context.trigger_text.clone(),
        from_user: String::new(),
        from_user_id: String::new(),
        timestamp: String::new(),
        link_url: String::new(),
        comment_a_id: String::new(),
    };
    let prompt = reply::build_prompt(&mention, &context, &policy.prompts);

    println!("=== system prompt ===");
    println!("{}", policy.prompts.system);
    println!("\n=== user prompt（已渲染）===");
    println!("{prompt}");
    println!("=== AI 回复预览（未发送）===");
    match ai.complete(&policy.prompts.system, &prompt).await {
        Ok(text) => println!("{text}"),
        Err(error) => println!("（生成失败：{error}）"),
    }
    Ok(())
}

/// 读 `--flag value` 形式的参数值。
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == flag)?;
    args.get(index + 1).cloned()
}

async fn cmd_reply() -> Result<()> {
    let puppet = XiaoheihePuppet::new(client().await?);
    let ai = AiClient::new(heybot::ai::AiConfig::from_env()?)?;
    let store = Arc::new(Mutex::new(reply::DedupStore::open(store::dedup_path())?));
    let policy = reply::policy_with_prompts();
    let mut bot = Bot::new(Arc::new(puppet), store.clone());
    register_auto_reply(&mut bot, &ai, &policy);

    // --latest：只回最新一条，用于安全验证。
    if std::env::args().any(|arg| arg == "--latest") {
        // 显式按时间倒序，不依赖接口返回顺序。
        let mut mentions = bot.puppet().mentions(0, 20).await?;
        mentions.sort_by(|a, b| {
            b.timestamp
                .parse::<f64>()
                .unwrap_or(0.0)
                .total_cmp(&a.timestamp.parse::<f64>().unwrap_or(0.0))
        });
        let Some(mention) = mentions.into_iter().find(mentions::is_at_mention) else {
            println!("没有需要回复的消息。");
            return Ok(());
        };
        println!(
            "准备回复：{} 在帖子 {} 说「{}」",
            mention.from_user, mention.link_id, mention.quote
        );
        let (reply_id, root_id) = mention.reply_targets();
        println!("  reply_id={reply_id} root_id={root_id}");
        match reply::handle_mention(&bot, &policy, &mention).await? {
            reply::ReplyOutcome::Replied { comment_id } => {
                println!("已回复，comment_id={comment_id}");
            }
            reply::ReplyOutcome::Skipped => println!("已跳过（之前处理过）。"),
            reply::ReplyOutcome::TargetMissing => println!("目标评论已被删除，无法回复。"),
            reply::ReplyOutcome::AiFailed(message) => println!("AI 生成失败：{message}"),
            reply::ReplyOutcome::Uncertain(message) => {
                println!("结果未知，未重发：{message}")
            }
        }
        return Ok(());
    }

    let handled = reply::run_once(&bot, &policy).await?;
    report(&handled);
    Ok(())
}

async fn cmd_watch() -> Result<()> {
    let ai = AiClient::new(heybot::ai::AiConfig::from_env()?)?;
    let store = Arc::new(Mutex::new(reply::DedupStore::open(store::dedup_path())?));
    let policy = reply::policy_with_prompts();

    let mut config = watch::default_config();
    if let Some(seconds) = heybot::config::number::<u64>("HEYBOT_POLL_SECONDS") {
        config.interval = Duration::from_secs(seconds.clamp(5, 600));
    }
    if let Some(user) = heybot::config::optional("HEYBOT_ONLY_USER") {
        config.only_user = user;
    }
    if heybot::config::flag("HEYBOT_REPLY_HISTORY") {
        config.skip_history = false;
    }
    if let Some(concurrency) = heybot::config::number::<usize>("HEYBOT_CONCURRENCY") {
        config.concurrency = concurrency.clamp(1, 16);
    }
    if let Some(limit) = heybot::config::number::<usize>("HEYBOT_MAX_PER_ROUND") {
        config.max_per_round = limit.clamp(1, 200);
    }
    if let Some(retries) = heybot::config::number::<u32>("HEYBOT_TARGET_MISSING_RETRIES") {
        config.target_missing_retries = retries.clamp(1, 20);
    }

    // 会话失效时回到循环开头重新拿会话：
    // 交互环境会重新扫码（二维码自动续期，不用守着刷新）；
    // 服务环境会等运维把会话文件换好，然后自动接着跑。
    let result = loop {
        let (session, nickname) = ensure_session().await?;
        let puppet = XiaoheihePuppet::new(HeyboxApi::new(session)?);
        let mut bot = Bot::new(Arc::new(puppet), store.clone());
        register_auto_reply(&mut bot, &ai, &policy);

        tracing::info!(
            "heybot 启动：账号 {nickname}，轮询间隔 {} 秒{}",
            config.interval.as_secs(),
            if config.skip_history {
                "，忽略历史消息"
            } else {
                "，会回复历史消息"
            }
        );
        tracing::info!("等待 @ 消息中，Ctrl-C 退出。");

        // Ctrl-C 时优雅退出：停止轮询，保存去重记录。
        let outcome = tokio::select! {
            _ = shutdown_signal() => break Ok(()),
            result = watch::watch(&bot, &store, &policy, &config) => result,
        };
        match outcome {
            Ok(()) => break Ok(()),
            Err(Error::Auth) => {
                tracing::error!("会话失效，重新获取会话…");
                continue;
            }
            Err(error) => break Err(error),
        }
    };
    result?;

    store.lock().await.flush()?;
    tracing::info!("已退出，去重记录已保存到 {}", store::dedup_path().display());
    Ok(())
}

/// 打印本轮处理结果。
fn report(handled: &[(mentions::Mention, ReplyOutcome)]) {
    for (mention, outcome) in handled {
        match outcome {
            ReplyOutcome::Replied { comment_id } => {
                tracing::info!(
                    "已回复帖子 {} 的评论 {}（comment_id={}）",
                    mention.link_id,
                    mention.reply_id,
                    comment_id
                );
            }
            ReplyOutcome::Skipped => {
                tracing::debug!("跳过已处理的消息 {}", mention.message_id);
            }
            ReplyOutcome::TargetMissing => {
                tracing::warn!("目标评论已被删除，跳过消息 {}", mention.message_id);
            }
            ReplyOutcome::AiFailed(message) => {
                tracing::warn!("AI 生成失败，已跳过：{message}");
            }
            ReplyOutcome::Uncertain(message) => {
                tracing::warn!("结果未知，未重发：{message}");
            }
        }
    }
}
