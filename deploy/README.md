# 部署到 Linux

## 先想清楚两件事

**1. 服务器在哪？** 小黑盒和常见的 AI 接口（DeepSeek / 通义 / 智谱）都是境内服务。
放到境外 VPS 上大概率是「能连上但很慢」，或者被风控。
**优先选国内的轻量服务器**（腾讯云/阿里云 2C2G 足够）。

**2. 需要哪些权限？** 只需要**出网**：访问 `api.xiaoheihe.cn` 和 AI 接口。
不需要入站端口，不需要域名，不需要证书。

---

## 一、拿到二进制

**在 GitHub 上构建**——仓库自带 CI，推送后自动产出三个 Linux 目标，
服务器上**不需要装 Rust**，本地也不用交叉编译：

| 目标 | 说明 |
|---|---|
| `x86_64-unknown-linux-musl` | 全静态，x86_64 服务器通用 |
| `aarch64-unknown-linux-musl` | 全静态，ARM 服务器（倚天 / Graviton） |
| `x86_64-unknown-linux-gnu` | 动态链接，依赖 glibc 2.35+ |

### 下载

打 tag（`v0.1.0`）会自动发一个
[Release](https://github.com/IQZZ020501/hey-bot/releases)，里面是三个二进制、
`SHA256SUMS` 和 `deploy/` 部署脚本：

```bash
gh release download v0.1.0 -p '*x86_64-unknown-linux-musl*' -D /tmp
scp /tmp/heybot-v0.1.0-x86_64-unknown-linux-musl server:/tmp/heybot
```

不想打 tag 的话，每次推送的构建产物也挂在 Actions 页面：

```bash
gh run download --repo IQZZ020501/hey-bot
```

### 自己触发一次

改完代码想立刻验证能不能编，在 Actions 页面点 **CI → Run workflow**，
或者：

```bash
gh workflow run ci.yml --repo IQZZ020501/hey-bot
```

> musl 目标是**全静态链接**（自带 libc），扔到任何 x86_64 Linux 上都能跑，
> 不挑 glibc 版本。

---

## 二、安装

把二进制传到服务器（`scp` 就行），然后：

```bash
sudo bash deploy/install.sh ./heybot
```

脚本会：

1. 建系统用户 `heybot`（无登录 shell）
2. 把二进制装到 `/opt/heybot/heybot`
3. 有 `.env` 的话一起装上（权限 640）
4. 装 systemd unit，`enable` + `restart`

`.env` 至少要有：

```bash
HEYBOT_AI_BASE_URL=https://api.deepseek.com/v1
HEYBOT_AI_API_KEY=sk-xxxxxxxx
HEYBOT_AI_MODEL=deepseek-chat
```

---

## 三、登录（要人扫码）

登录态必须用小号（bot 账号）扫，一次性完成：

```bash
sudo -u heybot -H /opt/heybot/heybot login
```

- 二维码会**直接画在终端里**，用小黑盒 App 扫
- 终端画不出来（比如某些精简 SSH 客户端）时，会存成 SVG 文件并打印路径
- 同时也会打印二维码的原始 URL

扫完会话落在 `/opt/heybot/session.json`，服务重启即生效。

> 也可以在本地登录后把 `session.json` 拷过去，但小黑盒可能按 IP 校验会话，
> **不保证能用**——还是建议在服务器上直接扫。

---

## 四、跑起来

```bash
systemctl status heybot
journalctl -u heybot -f          # 实时日志
systemctl restart heybot         # 改完 .env 后重启
```

正常启动会看到：

```
INFO 已登录：heybot（81183017）
INFO heybot 启动：账号 heybot，轮询间隔 20 秒，忽略历史消息
INFO 等待 @ 消息中，Ctrl-C 退出。
```

---

## 五、运维

### 会话失效了怎么办

Cookie 过期后服务会进入扫码等待，二维码过期就报错退出，systemd 再拉起——
于是你会看到每两分钟一轮的「取二维码 → 超时 → 重启」。

恢复：

```bash
sudo -u heybot -H /opt/heybot/heybot login   # 重新扫一次
sudo systemctl restart heybot
```

### 改配置

```bash
sudo vi /opt/heybot/.env
sudo systemctl restart heybot
```

### 状态文件

全在 `/opt/heybot/`（由 `HEYBOT_HOME` 指定）：

| 文件 | 作用 | 能不能删 |
|---|---|---|
| `session.json` | 登录态 | 删了要重新扫码 |
| `replied.txt` | 已回复的消息 id | **别删**，删了会重复回复 |
| `watermark.txt` | 水位线 | 删了会重新处理历史 @ |

### 换机器迁移

把这三个文件拷过去就行：

```bash
scp heybot:/opt/heybot/{session.json,replied.txt,watermark.txt} .
```

---

## 六、systemd unit 说明

`deploy/heybot.service` 里做了这些加固：

| 指令 | 作用 |
|---|---|
| `ProtectSystem=strict` | 整个文件系统只读，只有 `/opt/heybot` 可写 |
| `ProtectHome=true` | 读不到 `/home`，防止误读用户数据 |
| `PrivateTmp=true` | 私有 `/tmp`，二维码 SVG 不外泄 |
| `NoNewPrivileges=true` | 不能提权 |
| `Restart=always` | 进程退出自动拉起 |

如果你的环境不支持其中某条（比如老的 systemd 版本），删掉对应行即可，
不影响功能。

---

## 七、常见问题

**启动就退出，日志说「请重新登录」**
没登录或会话失效。跑 `heybot login`。

**一直提示被限流**
`HEYBOT_POLL_SECONDS` 调大（默认 20 秒），或者换 IP。

**回复内容很水 / 不对劲**
调 `prompts/system.txt`（人设）和 `prompts/reply_comment.txt`（评价模板），
改完 `systemctl restart heybot` 生效，不用重新编译。

**回复发不出去，但显示「结果未知」**
网络在请求发出后断了，服务端可能已经收到了。
程序会记进 `replied.txt` 不再重发——去帖子里确认一下。
