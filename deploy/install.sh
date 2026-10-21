#!/usr/bin/env bash
# 在 Linux 服务器上安装 heybot。
#
# 用法：
#   sudo bash deploy/install.sh <heybot 二进制路径> [安装目录]
#
# 例：
#   sudo bash deploy/install.sh target/x86_64-unknown-linux-musl/release/heybot
#   sudo bash deploy/install.sh ./heybot /srv/heybot
#
# 脚本做的事：
#   1. 建系统用户 heybot（无登录 shell）
#   2. 装二进制与 .env 到安装目录
#   3. 装 systemd unit 并启动
# 不负责登录——登录要人扫码，见脚本结束提示。

set -euo pipefail

BINARY="${1:-}"
INSTALL_DIR="${2:-/opt/heybot}"
SERVICE_NAME="heybot"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ $EUID -ne 0 ]]; then
  echo "需要 root：sudo bash $0 <二进制路径>" >&2
  exit 1
fi
if [[ -z "$BINARY" || ! -f "$BINARY" ]]; then
  echo "用法：sudo bash $0 <heybot 二进制路径> [安装目录]" >&2
  exit 1
fi

# ── 1. 系统用户 ─────────────────────────────────────────
if ! id -u "$SERVICE_NAME" >/dev/null 2>&1; then
  echo "==> 创建系统用户 $SERVICE_NAME"
  useradd --system --no-create-home --shell /usr/sbin/nologin "$SERVICE_NAME"
else
  echo "==> 系统用户 $SERVICE_NAME 已存在"
fi

# ── 2. 目录与文件 ───────────────────────────────────────
echo "==> 安装到 $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$BINARY" "$INSTALL_DIR/heybot"
chown "$SERVICE_NAME:$SERVICE_NAME" "$INSTALL_DIR/heybot"

if [[ -f .env ]]; then
  echo "==> 安装 .env（权限 640，仅属主与组可读）"
  install -m 0640 .env "$INSTALL_DIR/.env"
  chown "$SERVICE_NAME:$SERVICE_NAME" "$INSTALL_DIR/.env"
else
  echo "!! 没找到 .env，跳过。服务需要它才能调 AI 接口。" >&2
  echo "   先建一个：cp .env.example $INSTALL_DIR/.env && chmod 640 $INSTALL_DIR/.env" >&2
  echo "   然后填 HEYBOT_AI_API_KEY 和 HEYBOT_AI_MODEL" >&2
fi

# 状态文件目录必须可写
chown -R "$SERVICE_NAME:$SERVICE_NAME" "$INSTALL_DIR"

# ── 3. systemd ──────────────────────────────────────────
echo "==> 安装 systemd unit"
install -m 0644 "$SCRIPT_DIR/heybot.service" "/etc/systemd/system/$SERVICE_NAME.service"
systemctl daemon-reload
systemctl enable "$SERVICE_NAME" >/dev/null
systemctl restart "$SERVICE_NAME"

sleep 2
if systemctl is-active --quiet "$SERVICE_NAME"; then
  echo "==> $SERVICE_NAME 已启动"
else
  echo "!! $SERVICE_NAME 启动失败，日志：" >&2
  journalctl -u "$SERVICE_NAME" -n 30 --no-pager >&2
fi

cat <<EOF

接下来要人扫码登录（这一步没法自动完成）：

    sudo -u $SERVICE_NAME -H $INSTALL_DIR/heybot login

二维码会直接画在终端里；终端画不出来时会存成 SVG 文件并打印路径。
扫完后会话落在 $INSTALL_DIR/session.json，服务重启即生效。

查看日志：
    journalctl -u $SERVICE_NAME -f
EOF
