#!/usr/bin/env bash
# 把中继装到服务器上（systemd 服务，开机自启、挂了自动重启）。见 docs/12-relay.md。
#
#   scripts/deploy-relay.sh vihv@129.211.71.14   # 或直接 `just relay-deploy`（默认就是这台）
#   RELAY_PORT=9000 scripts/deploy-relay.sh ubuntu@1.2.3.4
#
# 要求：ssh 登得上去；不是 root 的话要能 sudo（自己那台：vihv@ + 公钥 + sudo 免密）。
# 服务器上不需要装 Rust。
# 装完打印家里那台电脑要设的 AGENT_RELAY / AGENT_RELAY_KEY。
# 再跑一遍就是升级：换二进制、重启服务，密钥不变（手机不用重新配）。
set -euo pipefail

target="${1:?用法：deploy-relay.sh <user@服务器IP>}"
port="${RELAY_PORT:-8790}"
bin="relay/target/x86_64-unknown-linux-musl/release/agent-relay"
[ -x "$bin" ] || { echo "先编：just relay-build" >&2; exit 1; }
host="${target#*@}"

echo "→ 传二进制到 $target"
scp -q "$bin" "$target:/tmp/agent-relay"

echo "→ 装成 systemd 服务（端口 $port）"
# 服务以 DynamicUser 跑（不是 root，也不建用户）；密钥在 StateDirectory 里，
# 由 systemd 管权限（/var/lib/agent-relay → /var/lib/private/agent-relay）
out="$(ssh "$target" "PORT=$port bash -s" <<'REMOTE'
set -euo pipefail
if [ "$(id -u)" = 0 ]; then S=; else S=sudo; fi
$S install -m 0755 /tmp/agent-relay /usr/local/bin/agent-relay
rm -f /tmp/agent-relay
$S tee /etc/systemd/system/agent-relay.service >/dev/null <<UNIT
[Unit]
Description=android-agent relay
After=network-online.target
Wants=network-online.target

[Service]
Environment=AGENT_RELAY_LISTEN=0.0.0.0:$PORT
Environment=AGENT_RELAY_KEY_FILE=/var/lib/agent-relay/key
ExecStart=/usr/local/bin/agent-relay
Restart=always
RestartSec=2
DynamicUser=yes
StateDirectory=agent-relay
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes

[Install]
WantedBy=multi-user.target
UNIT
$S systemctl daemon-reload
$S systemctl enable agent-relay >/dev/null 2>&1
$S systemctl restart agent-relay
# 第一次启动会生成密钥：等它写出来
for _ in $(seq 1 20); do $S test -s /var/lib/agent-relay/key && break; sleep 0.5; done
$S systemctl is-active agent-relay >/dev/null || { $S journalctl -u agent-relay -n 20 --no-pager; exit 1; }
# 防火墙：装了 ufw 且开着的话顺手放行（云厂商的安全组还得自己去控制台放）
if command -v ufw >/dev/null && $S ufw status 2>/dev/null | grep -q "Status: active"; then
  $S ufw allow "$PORT/tcp" >/dev/null && echo "  已在 ufw 里放行 TCP $PORT"
fi
echo "KEY=$($S cat /var/lib/agent-relay/key)"
REMOTE
)"
printf '%s\n' "$out" | grep -v '^KEY=' || true
key="$(printf '%s\n' "$out" | sed -n 's/^KEY=//p')"
[ -n "$key" ] || { echo "没拿到密钥（看上面的输出）" >&2; exit 1; }

echo
echo "  中继在 $host:$port 跑起来了。家里那台电脑启动核心前设上："
echo "    export AGENT_RELAY=$host:$port"
echo "    export AGENT_RELAY_KEY=$key"
echo "  （写进仓库根目录的 .env.local，just dev-host 会自动读）"
echo "  云服务器的安全组 / 防火墙要放行 TCP $port。"
