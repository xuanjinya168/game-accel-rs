#!/usr/bin/env bash
set -euo pipefail

# GameAccel All-in-One Installer (QUIC/UDP Server + Built-in Web Admin Panel)
# Usage:
#   curl -fsSL https://your-host/install.sh | bash
#   GACL_BIN_URL=https://.../game-accel-server-linux-amd64 bash install.sh

BIN_PATH="/usr/local/bin/game-accel-server"
CLI_PATH="/usr/local/bin/gacl"
DATA_DIR="/etc/game-accel"
DATA_FILE="${DATA_DIR}/data.json"
SERVICE_FILE="/etc/systemd/system/game-accel-server.service"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${CYAN}[INFO]${NC} $*"; }
ok()    { echo -e "${GREEN}[OK]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
err()   { echo -e "${RED}[ERR]${NC} $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || err "请使用 root 权限运行本安装脚本（sudo bash install.sh）"

ARCH_RAW="$(uname -m)"
case "$ARCH_RAW" in
  x86_64|amd64) ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) err "暂不支持的架构: ${ARCH_RAW}" ;;
esac

rand_str() {
  local len="${1:-20}"
  (tr -dc 'A-Za-z0-9' </dev/urandom || true) | head -c "$len"
}

detect_public_ip() {
  curl -fsSL4 --max-time 6 https://ifconfig.me 2>/dev/null \
    || curl -fsSL4 --max-time 6 https://api.ipify.org 2>/dev/null \
    || hostname -I | awk '{print $1}'
}

tune_kernel_udp() {
  info "优化 Linux 内核 UDP 缓冲区与 BBR..."
  cat >/etc/sysctl.d/99-game-accel.conf <<'EOF'
net.core.rmem_max = 8388608
net.core.wmem_max = 8388608
net.core.rmem_default = 1048576
net.core.wmem_default = 1048576
net.core.default_qdisc = fq
net.ipv4.tcp_congestion_control = bbr
EOF
  sysctl --system >/dev/null 2>&1 || true
}

install_binary() {
  if [[ -n "${GACL_BIN_URL:-}" ]]; then
    info "正在下载预编译服务端 (${ARCH}): ${GACL_BIN_URL}"
    curl -fsSL "$GACL_BIN_URL" -o /tmp/game-accel-server
    install -m 755 /tmp/game-accel-server "$BIN_PATH"
    rm -f /tmp/game-accel-server
  elif [[ -f "./game-accel-server" ]]; then
    info "使用当前目录的 ./game-accel-server"
    install -m 755 ./game-accel-server "$BIN_PATH"
  elif [[ -x "$BIN_PATH" ]]; then
    ok "检测到已安装的二进制: ${BIN_PATH}"
  else
    err "未找到预编译二进制。请先将 game-accel-server 传到 ${BIN_PATH} 或设置 GACL_BIN_URL 环境变量。"
  fi
}

install_cli_menu() {
  cat >"$CLI_PATH" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
DATA_FILE="/etc/game-accel/data.json"

show_info() {
  echo "================ GameAccel 运行状态 ================"
  systemctl is-active game-accel-server >/dev/null 2>&1 && echo "服务状态: 运行中 (active)" || echo "服务状态: 未运行"
  ss -ulnp 2>/dev/null | grep game-accel-serv || true
  ss -tlnp 2>/dev/null | grep game-accel-serv || true
  if [[ -f "$DATA_FILE" ]]; then
    python3 - "$DATA_FILE" <<'PY' 2>/dev/null || cat "$DATA_FILE"
import json, sys, urllib.parse
d = json.load(open(sys.argv[1]))
pw_info = "(已 PBKDF2 加密存储，忘记请选 2 重置)" if str(d.get("admin_pass","")).startswith("pbkdf2$") else d.get("admin_pass","")
print(f"后台账号: {d['admin_user']}   后台密码: {pw_info}")
print("后台入口(连入隧道后访问，公网端口已隐藏): http://panel.gacl.local  或  http://198.18.0.1 (TUN)")
print(f"公网节点: {d['public_host']}:{d['udp_port']}   SNI: {d['sni']}")
print("------------------ 分享链接列表 ------------------")
for l in d.get("links", []):
    st = "启用" if l.get("enabled") else "禁用"
    dup = "1" if l.get("game_dup", True) else "0"
    ins = "1" if d.get("insecure") else "0"
    name = urllib.parse.quote(l["name"])
    sni = urllib.parse.quote(d["sni"])
    uri = f"gacl://{l['token']}@{d['public_host']}:{d['udp_port']}?sni={sni}&dup={dup}&insecure={ins}#{name}"
    hy2 = f"hysteria2://{l['token']}@{d['public_host']}:{d['udp_port']}/?sni={sni}&insecure={ins}#{name}"
    print(f"[{st}] {l['name']}")
    print(f"  GACL: {uri}")
    print(f"  HY2 : {hy2}")
PY
  fi
}

reset_pass() {
  read -r -p "请输入新的管理后台密码（至少6位）: " NEW_PASS
  [[ ${#NEW_PASS} -ge 6 ]] || { echo "密码长度至少 6 位"; exit 1; }
  python3 - "$DATA_FILE" "$NEW_PASS" <<'PY'
import json, sys
p, new_pass = sys.argv[1], sys.argv[2]
d = json.load(open(p))
d["admin_pass"] = new_pass
open(p, "w").write(json.dumps(d, indent=2, ensure_ascii=False))
PY
  systemctl restart game-accel-server
  echo "密码已重置并重启服务生效！"
}

echo "========================================"
echo "       GameAccel 服务端运维菜单"
echo "========================================"
echo " 1. 查看服务状态、面板密码与全部 gacl:// 链接"
echo " 2. 重置 Web 管理后台密码"
echo " 3. 重启 game-accel-server 服务"
echo " 4. 查看实时日志 (Ctrl+C 退出)"
echo " 5. 停止服务"
echo " 6. 彻底卸载 GameAccel"
echo " 0. 退出"
echo "----------------------------------------"
read -r -p "请选择 [0-6]: " choice
case "${choice:-0}" in
  1) show_info ;;
  2) reset_pass ;;
  3) systemctl restart game-accel-server && echo "已重启" ;;
  4) journalctl -u game-accel-server -f ;;
  5) systemctl stop game-accel-server && echo "已停止" ;;
  6)
    read -r -p "确认彻底卸载并删除 /etc/game-accel 数据? [y/N]: " yn
    if [[ "$yn" =~ ^[Yy]$ ]]; then
      systemctl disable --now game-accel-server || true
      rm -f /etc/systemd/system/game-accel-server.service /usr/local/bin/game-accel-server /usr/local/bin/gacl /etc/sysctl.d/99-game-accel.conf
      rm -rf /etc/game-accel
      systemctl daemon-reload
      echo "已彻底卸载。"
    fi
    ;;
  *) exit 0 ;;
esac
EOF
  chmod +x "$CLI_PATH"
}

main() {
  install_binary
  tune_kernel_udp
  mkdir -p "$DATA_DIR"

  DEFAULT_IP="$(detect_public_ip)"
  DEFAULT_UDP="8443"
  if ! ss -ulnp 2>/dev/null | grep -q ":443 "; then
    DEFAULT_UDP="443"
  fi
  DEFAULT_PANEL_PORT="9090"

  if [[ -t 0 ]]; then
    read -r -p "公网 IP 或域名 [${DEFAULT_IP}]: " PUBLIC_HOST
    PUBLIC_HOST="${PUBLIC_HOST:-$DEFAULT_IP}"

    read -r -p "QUIC UDP 监听端口 [${DEFAULT_UDP}]: " UDP_PORT
    UDP_PORT="${UDP_PORT:-$DEFAULT_UDP}"

    read -r -p "Web 管理后台端口 [${DEFAULT_PANEL_PORT}]: " PANEL_PORT
    PANEL_PORT="${PANEL_PORT:-$DEFAULT_PANEL_PORT}"

    read -r -p "TLS 证书域名 SNI（留空则使用自签测试证书）: " SNI_DOMAIN
    SNI_DOMAIN="${SNI_DOMAIN:-localhost}"
  else
    PUBLIC_HOST="${GACL_PUBLIC_HOST:-$DEFAULT_IP}"
    UDP_PORT="${GACL_UDP_PORT:-$DEFAULT_UDP}"
    PANEL_PORT="${GACL_PANEL_PORT:-$DEFAULT_PANEL_PORT}"
    SNI_DOMAIN="${GACL_SNI:-localhost}"
  fi

  CERT_ARGS=""
  if [[ "$SNI_DOMAIN" != "localhost" && -f "/root/cert/${SNI_DOMAIN}/fullchain.pem" && -f "/root/cert/${SNI_DOMAIN}/privkey.pem" ]]; then
    ok "自动发现正规证书: /root/cert/${SNI_DOMAIN}/"
    CERT_ARGS="--cert /root/cert/${SNI_DOMAIN}/fullchain.pem --key /root/cert/${SNI_DOMAIN}/privkey.pem"
  elif [[ -n "${GACL_CERT:-}" && -n "${GACL_KEY:-}" ]]; then
    CERT_ARGS="--cert ${GACL_CERT} --key ${GACL_KEY}"
  else
    warn "未发现 ${SNI_DOMAIN} 的 PEM 证书，将使用自签测试证书（后续可在 systemd 配置 --cert/--key）"
  fi

  SEED_TOKEN="${GACL_TOKEN:-$(rand_str 24)}"
  PANEL_PASS_ARG=""
  if [[ -n "${GACL_PANEL_PASS:-}" ]]; then
    PANEL_PASS_ARG="--panel-pass ${GACL_PANEL_PASS}"
  fi

  cat >"$SERVICE_FILE" <<EOF
[Unit]
Description=GameAccel All-in-One QUIC/UDP Server & Web Panel
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${BIN_PATH} --listen 0.0.0.0:${UDP_PORT} --panel-listen 127.0.0.1:${PANEL_PORT} --data-file ${DATA_FILE} --public-host ${PUBLIC_HOST} --sni ${SNI_DOMAIN} --token ${SEED_TOKEN} ${PANEL_PASS_ARG} ${CERT_ARGS}
Restart=always
RestartSec=3
LimitNOFILE=65535
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

  systemctl daemon-reload
  systemctl enable game-accel-server
  systemctl restart game-accel-server
  install_cli_menu
  sleep 1

  echo ""
  ok "GameAccel 安装并启动成功！"
  echo -e "  Web 管理后台 (仅监听 127.0.0.1:${PANEL_PORT}，公网不可扫描):"
  echo -e "    连入加速器后浏览器访问: ${GREEN}http://panel.gacl.local${NC}  (TUN 模式: ${GREEN}http://198.18.0.1${NC})"
  echo -e "  随时运行 ${CYAN}gacl${NC} 打开运维菜单查看状态与全部 gacl:// 分享链接"
  echo ""
  "$CLI_PATH" <<< "1" || true
}

main "$@"
