# heybot

> 小黑盒 @ 自动回复机器人：被 @ 时读取对应消息 → 交给 AI 生成回复 → 用官方网页协议发回评论区。

[![Rust](https://img.shields.io/badge/rust-1.85%2B-000000?logo=rust)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## 特性

| 能力 | 状态 |
|---|---|
| 请求签名（`hkey` + Workshop `_rnd`） | ✅ 与 Python 参考实现逐位一致，服务端接受 |
| @ 消息读取（`/bbs/app/user/message`） | ✅ 端点与参数已确认，字段语义已对齐官方网页端 |
| 评论回复（`/bbs/app/comment/create`） | ✅ 契约已确认（含双签名），等待真实账号验证 |
| 扫码登录 | ✅ 已实现，等待真实扫码验证 |
| AI 生成 + 自动回复 | ✅ 已实现，幂等与"结果未知不重发"已就位 |

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
| `HEYBOT_PROMPT_DIR` | 一次性指定 prompt 模板目录 | 项目根 `prompts/` |

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

### 常驻模式

```bash
./target/release/heybot watch
```

行为：

- 启动时建立**水位线**（记录当前最大 `message_id`），**不回历史消息**
- 之后只处理水位线之后的新消息
- 轮询间隔 `HEYBOT_POLL_SECONDS`（默认 20 秒）
- 限流自动退避 60 秒；连续失败 10 次退出
- 会话失效直接退出并提示重新登录
- `Ctrl-C` / `SIGTERM` 优雅退出，去重记录落盘

实测 `message_id` 按时间严格递减（新的更大），所以它天然适合做水位线。

#### 多人同时 @ 怎么处理

**排队，但队列在小黑盒那边。** 本地没有待处理队列，机制是三个东西：

| 组件 | 位置 | 作用 |
|---|---|---|
| 服务端消息列表 | 小黑盒 | 事实来源，待处理消息一直躺在那儿 |
| 水位线 | `watermark.txt` | 已处理到的最大 `message_id` |
| 去重表 | `replied.txt` | 哪些 id 已经回复过 |

每轮从服务端拉「水位线之后」的消息处理掉。一轮最多处理
`HEYBOT_MAX_PER_ROUND`（默认 20）条，超出的留到下一轮。

每条消息分三个阶段：

| 阶段 | 并发 | 原因 |
|---|---|---|
| 准备（确认目标 + 读上下文） | 串行 | 走小黑盒接口，受 600ms 限速保护 |
| AI 生成 | **并发**（`HEYBOT_CONCURRENCY`，默认 4） | 唯一不受接口限速约束的 IO |
| 发送评论 | 串行 | 避免同时打接口触发风控 |

所以 5 条 @ 大约 20-40 秒发完；100 条 @ 分几轮慢慢消化，不会打爆接口。

#### 不会丢消息

四条规则保证「有人 @ 了但没被回复」不会发生：

1. **水位线只在处理成功后推进**。一条消息处理失败（网络抖动、AI 超时），
   水位线停在它前面，下一轮重新处理。
2. **失败的消息不写去重表**。去重表只记录确实回复过的消息。
3. **分页追平**。一次来超过一页的 @ 时，递增 `offset` 一直拉到追上水位线。
4. **水位线落盘**。bot 停机（重启、崩溃、换机）期间到达的 @，
   下次启动会补上，不会被当成历史消息跳过。

另外，「目标评论查不到」会重试 `HEYBOT_TARGET_MISSING_RETRIES` 次（默认 5）
才认定评论真被删——因为消息中心可能比评论树索引先到，这时查不到不代表被删除。

> 删掉 `watermark.txt` 会让 bot 重新忽略全部历史消息（等同首次启动）。

相关环境变量见[配置](#配置)。

> **常驻需要 7×24 在线**。笔记本合盖休眠、关机、换网络都会中断。
> 真要长跑就放到 VPS，或用 `launchd` 把本机当服务。

### 两种场景

| 场景 | @ 触发评论挂在哪 | bot 评价什么 |
|---|---|---|
| 在帖子下 @ bot | 顶层评论 | **整篇帖子**（标题 + 正文） |
| 回复某条评论并 @ bot | 楼中楼 | **被回复的那条评论** |

### 目录结构

```
heybot/
├── src/            Rust 源码
├── prompts/        Prompt 模板（system / reply_comment / reply_post）
└── .env.example    配置模板
```

`tools/`（签名对拍脚本）与 `vendor/`（第三方 Python 参考实现）均不入库：
签名算法的期望值已固化为 `src/signing.rs` 里的测试向量，
主程序与 `cargo test` 都不依赖它们。

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
