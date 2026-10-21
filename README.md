# heybot

> 小黑盒 @ 自动回复机器人：被 @ 时读取对应消息 → 交给 AI 生成回复 → 用官方网页协议发回评论区。

[![Rust](https://img.shields.io/badge/rust-1.85%2B-000000?logo=rust)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## 快速开始

### 1. 配置

```bash
cp .env.example .env
chmod 600 .env
# 编辑 .env，填上 HEYBOT_AI_API_KEY 和 HEYBOT_AI_MODEL
```

### 2. 启动

```bash
cargo build --release
./target/release/heybot watch
```

**就这一条命令。** 它会自己判断：

| 状态 | 行为 |
|---|---|
| 没登录 | 打印二维码 → 扫码 → 保存会话 → 进入常驻 |
| 已登录 | 直接进入常驻，等待 @ 消息 |
| 会话失效 | 提示重新扫码 → 扫码 → 进入常驻 |
| 会话文件损坏 | 按未登录处理，走扫码 |

无参数运行等价于 `heybot watch`：

```bash
./target/release/heybot        # 等同于 watch
```

常驻期间的行为：

- 启动时建立水位线，**不回历史消息**
- 有人 @ 就自动：判断场景 → 读目标内容 → 调 AI → 回帖
- 幂等，同一条消息不会回两次
- `Ctrl-C` 优雅退出，去重记录落盘，重启接着跑

### 3. 其它命令

```bash
./target/release/heybot preview            # 渲染 prompt 并预览 AI 回复（不发送）
./target/release/heybot mentions           # 看抓到了什么
./target/release/heybot whoami             # 当前登录的是哪个号
./target/release/heybot reply              # 手动回一条
./target/release/heybot reply --latest     # 只回最新一条
./target/release/heybot login              # 强制重新扫码登录
./target/release/heybot probe              # 签名连通性自检（不需要登录）
```

`preview` 有三种输入来源，方便本地调 prompt：

```bash
./target/release/heybot preview                            # 用最新一条真实的 @ 消息
./target/release/heybot preview --text "要评价的评论内容"    # 自带文本
./target/release/heybot preview --post 192794050           # 用某个帖子的正文
```

`--text` 模式不联网、不需要登录态，可以随便试措辞。

## 配置

### 环境变量

所有配置都可以写在 `.env` 里，不用每次 export。

优先级：**进程环境变量 > `.env` > 内置默认值**。
临时覆盖某一项不用改文件：

```bash
HEYBOT_POLL_SECONDS=5 heybot watch
```

| 变量 | 作用 | 默认值 |
|---|---|---|
| `HEYBOT_AI_BASE_URL` | OpenAI 兼容接口地址 | `https://api.openai.com/v1` |
| `HEYBOT_AI_API_KEY` | AI 接口密钥（必填） | — |
| `HEYBOT_AI_MODEL` | 模型名（必填） | — |
| `HEYBOT_AI_TIMEOUT_SECONDS` | AI 请求超时 | `60` |
| `HEYBOT_AI_MAX_TOKENS` | 单次回复上限 | 不限 |
| `HEYBOT_AI_TEMPERATURE` | 采样温度 | 模型默认 |
| `HEYBOT_POLL_SECONDS` | 轮询间隔（5~600） | `20` |
| `HEYBOT_CONCURRENCY` | 并发 AI 生成数（1~16） | `4` |
| `HEYBOT_MAX_PER_ROUND` | 单轮最多处理多少条（1~200） | `20` |
| `HEYBOT_TARGET_MISSING_RETRIES` | 目标查不到的重试次数（1~20） | `5` |
| `HEYBOT_ONLY_USER` | 只回指定用户发的消息 | 不过滤 |
| `HEYBOT_REPLY_HISTORY` | 设置后 watch 也会回历史消息 | 只回新消息 |
| `HEYBOT_SESSION_FILE` | 会话文件路径 | 项目根目录 |
| `HEYBOT_DEDUP_FILE` | 去重记录路径 | 项目根目录 |
| `HEYBOT_WATERMARK_FILE` | 水位线文件路径 | 项目根目录 |
| `HEYBOT_ENV_FILE` | `.env` 路径 | 见下方查找顺序 |
| `HEYBOT_HOME` | 一次性指定项目根目录 | 自动探测 |
| `HEYBOT_PROMPT_DIR` | 一次性指定 prompt 模板目录 | 无（用 `src/prompt.rs` 内置模板） |

`.env` 查找顺序（取第一个存在的）：

1. `HEYBOT_ENV_FILE` 指定的路径
2. 当前工作目录的 `.env`
3. 可执行文件所在目录的 `.env`
4. `~/.config/heybot/.env`

启动时会打印实际加载了哪个文件；文件权限过宽（其他用户可读）会给出警告。

> `.env` 含凭据，已在 `.gitignore` 里。**不要提交**。

### 本地文件

所有本地文件都在**项目根目录**：

| 文件 | 内容 | 权限 |
|---|---|---|
| `session.json` | 登录 Cookie（含 `pkey`） | 0600 |
| `replied.txt` | 已处理的消息 id | 0600 |
| `watermark.txt` | 水位线（已处理到的最大 `message_id`） | 0600 |
| `.env` | AI 接口配置 | 建议 0600 |

路径解析顺序：

1. `HEYBOT_SESSION_FILE` / `HEYBOT_DEDUP_FILE` 显式指定
2. **项目根目录**——从当前目录向上找带 `Cargo.toml` 的目录，所以在子目录里跑也会落在项目根
3. 可执行文件所在目录
4. 兜底 `~/.config/heybot/`

也可以用 `HEYBOT_HOME` 一次性指定根目录。

## 工作原理

### 架构：Puppet + Bot + Message

借鉴 wechaty 的形态，把「平台特有」和「业务逻辑」分开：

| 层 | 模块 | 职责 |
|---|---|---|
| 平台接口 | `src/puppet.rs` | `Puppet` trait：拉消息、判目标、收上下文、发回复 |
| 平台实现 | `src/provider.rs` | `XiaoheihePuppet`：小黑盒的评论树、id 语义都收在这里 |
| 事件派发 | `src/bot.rs` | `Bot` / `Message`：注册处理器、收到消息自动调用 |
| 业务编排 | `src/reply.rs` `src/watch.rs` | 幂等、水位线、限速、失败重试 |

用法是「注册处理器」而不是「调用一堆自由函数」：

```rust
let mut bot = Bot::new(Arc::new(XiaoheihePuppet::new(api)), dedup);
bot.on_message(|msg| async move {
    if msg.is_replied().await {
        return Ok(ReplyOutcome::Skipped);
    }
    let text = ai.complete(&system, msg.prompt()).await?;
    msg.say(&text).await
});
```

`msg.say()` 自带幂等：已回复过的消息直接跳过；发送时网络错误记为「结果未知」并写去重表，避免重发。

因为平台行为收敛在 `Puppet` 后面，测试不需要 mock HTTP server——`tests/integration/` 用一个内存里的假平台跑完整链路。

### 常驻模式

```bash
./target/release/heybot watch
```

## 参考与致谢

本项目的接口契约、签名算法与实现思路大量来自以下开源项目，在此致谢：

| 项目 | 用途 | 许可 |
|---|:--|---|
| [AaronZ345/xiaoheihe-cli](https://github.com/AaronZ345/xiaoheihe-cli) | Python 参考实现。`hkey` 签名算法、评论接口契约、扫码登录流程、Cookie 归一化 | MIT (© 2025 Li Fangyu) |
| [luckylca/xhhBackCrack](https://github.com/luckylca/xhhBackCrack) | `hkey` 签名算法的逆向思路与算法描述，配套文章 [《小黑盒后端参数逆向》](https://luckylca.github.io/2026/02/08/xhhBack-crack/) | 文章 CC BY-NC-SA 4.0 |
| [k1m0206/better-XiaoHeiHe](https://github.com/k1m0206/better-XiaoHeiHe) | 网页版客户端插件。其消息接口调用方式帮助确认了「@我的」对应 `message_type=16` 而非 `list_type` | 公开资料 |

同时也感谢小黑盒社区在公开issue与技术文章中分享的接口信息。

## 许可与合规

- 本项目只与用户**自己的账号**交互，用于个人自动化。
- 签名算法与接口契约来自社区公开资料，参考实现遵循其 MIT 许可。
- 不包含任何真实 Cookie、Token 或可直接复用的生产凭据。
- 使用前请自行确认符合小黑盒用户协议与相关法规。
