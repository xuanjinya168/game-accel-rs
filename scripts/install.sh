#!/usr/bin/env bash
set -euo pipefail

# GameAccel All-in-One Installer
#   - QUIC/UDP Server (dual-protocol gacl:// + hysteria2://)
#   - Built-in tunnel-only Web Admin Panel
#   - Automatic DNS check + Let's Encrypt cert issuance & auto-renewal (Certbot)
#   - Automatic Nginx HTTPS + Hugo (PaperMod) static masquerade blog + HTTP/3 fallback
#
# Usage:
#   GACL_BIN_URL=https://.../server bash <(curl -fsSL https://.../install.sh)

BIN_PATH="/usr/local/bin/game-accel-server"
CLI_PATH="/usr/local/bin/gacl"
DATA_DIR="/etc/game-accel"
DATA_FILE="${DATA_DIR}/data.json"
SERVICE_FILE="/etc/systemd/system/game-accel-server.service"
BLOG_SRC_DIR="/opt/gacl-blog"
SITE_WEB_DIR="/var/www/gacl-site"
CERTBOT_WEBROOT="/var/www/certbot"

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

resolve_domain_ipv4s() {
  local domain="$1"
  if command -v getent >/dev/null 2>&1; then
    getent ahostsv4 "$domain" 2>/dev/null | awk '{print $1}' | sort -u
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c 'import socket,sys; print("\n".join(sorted({i[4][0] for i in socket.getaddrinfo(sys.argv[1], None, socket.AF_INET)})))' "$domain" 2>/dev/null || true
  fi
}

check_domain_dns() {
  local domain="$1"
  local expected_ip="$2"
  info "正在检测域名 ${domain} 的 DNS 解析记录..."
  local resolved
  resolved="$(resolve_domain_ipv4s "$domain" || true)"
  if [[ -z "$resolved" ]]; then
    warn "未查询到 ${domain} 的 IPv4 (A) 记录，请先在域名商处添加 A 记录指向 ${expected_ip}"
    if [[ "${GACL_SKIP_DNS_CHECK:-0}" == "1" ]]; then
      return 0
    fi
    if [[ -t 0 ]]; then
      read -r -p "仍要强制继续申请证书吗? [y/N]: " yn
      [[ "$yn" =~ ^[Yy]$ ]] || err "已取消安装，请配置好 DNS 解析后重试。"
      return 0
    fi
    err "DNS 未解析到本机 IP (${expected_ip})，中止安装。"
  fi

  if echo "$resolved" | grep -Fxq "$expected_ip"; then
    ok "DNS 校验通过: ${domain} -> ${expected_ip}"
    return 0
  fi

  warn "域名 ${domain} 当前解析为: $(echo "$resolved" | tr '\n' ' ')"
  warn "本机公网 IP 为: ${expected_ip}（两者不一致会导致自动申请证书失败）"
  if [[ "${GACL_SKIP_DNS_CHECK:-0}" == "1" ]]; then
    return 0
  fi
  if [[ -t 0 ]]; then
    read -r -p "仍要强制继续吗? [y/N]: " yn
    [[ "$yn" =~ ^[Yy]$ ]] || err "已取消安装，请将域名 A 记录指向 ${expected_ip} 后重试。"
    return 0
  fi
  err "DNS 未指向本机 IP (${expected_ip})，中止安装。"
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
    if [[ "$GACL_BIN_URL" == *.tar.gz || "$GACL_BIN_URL" == *.tgz ]]; then
      mkdir -p /tmp/gacl-dl
      curl -fsSL "$GACL_BIN_URL" -o /tmp/gacl-dl/server.tar.gz
      tar -xzf /tmp/gacl-dl/server.tar.gz -C /tmp/gacl-dl
      local extracted
      extracted="$(find /tmp/gacl-dl -maxdepth 2 -type f \( -name 'server' -o -name 'game-accel-server' \) | head -n 1)"
      [[ -n "$extracted" ]] || err "压缩包中未找到服务端二进制文件"
      install -m 755 "$extracted" "$BIN_PATH"
      rm -rf /tmp/gacl-dl
    else
      curl -fsSL "$GACL_BIN_URL" -o /tmp/game-accel-server
      install -m 755 /tmp/game-accel-server "$BIN_PATH"
      rm -f /tmp/game-accel-server
    fi
  elif [[ -f "./game-accel-server" ]]; then
    info "使用当前目录的 ./game-accel-server"
    install -m 755 ./game-accel-server "$BIN_PATH"
  elif [[ -x "$BIN_PATH" ]]; then
    ok "检测到已安装的二进制: ${BIN_PATH}"
  else
    err "未找到预编译二进制。请先将 game-accel-server 传到 ${BIN_PATH} 或设置 GACL_BIN_URL 环境变量。"
  fi
}

install_web_packages() {
  if command -v apt-get >/dev/null 2>&1; then
    info "安装 Nginx、Certbot、Git 与依赖..."
    DEBIAN_FRONTEND=noninteractive apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq nginx certbot git curl ca-certificates tar
  else
    err "自动申请证书与建站目前仅支持 Debian / Ubuntu (apt-get) 系统"
  fi
}

provision_letsencrypt_cert() {
  local domain="$1"
  mkdir -p "$CERTBOT_WEBROOT"

  if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
    ufw allow 80/tcp >/dev/null 2>&1 || true
    ufw allow 443/tcp >/dev/null 2>&1 || true
  fi

  # Temporary HTTP-only vhost so certbot webroot challenge succeeds cleanly
  rm -f /etc/nginx/sites-enabled/default
  cat >/etc/nginx/sites-available/gacl-site.conf <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name ${domain};

    location ^~ /.well-known/acme-challenge/ {
        root ${CERTBOT_WEBROOT};
        default_type "text/plain";
    }

    location / {
        return 200 "ok\n";
    }
}
EOF
  ln -sf /etc/nginx/sites-available/gacl-site.conf /etc/nginx/sites-enabled/gacl-site.conf
  nginx -t
  systemctl enable --now nginx
  systemctl reload nginx

  info "正在通过 Let's Encrypt (Certbot) 为 ${domain} 自动申请证书..."
  certbot certonly \
    --webroot -w "$CERTBOT_WEBROOT" \
    -d "$domain" \
    --non-interactive \
    --agree-tos \
    --register-unsafely-without-email \
    --keep-until-expiring

  # Auto-renewal deploy hook: reload Nginx + restart game-accel-server when cert renews
  mkdir -p /etc/letsencrypt/renewal-hooks/deploy
  cat >/etc/letsencrypt/renewal-hooks/deploy/reload-gacl.sh <<'EOF'
#!/usr/bin/env bash
systemctl reload nginx >/dev/null 2>&1 || true
systemctl restart game-accel-server >/dev/null 2>&1 || true
EOF
  chmod +x /etc/letsencrypt/renewal-hooks/deploy/reload-gacl.sh
  systemctl enable --now certbot.timer >/dev/null 2>&1 || true

  ok "证书申请成功，已配置 certbot.timer 自动续签与热重载！"
}

deploy_hugo_masquerade_site() {
  local domain="$1"
  local site_label
  site_label="$(echo "$domain" | sed -E 's/^www\.//; s/\..*$//' | awk '{print toupper(substr($0,1,1)) substr($0,2)}')"
  [[ -n "$site_label" ]] || site_label="DevNotes"

  info "正在安装最新 Hugo Extended (${ARCH}) 与 PaperMod 主题..."
  if ! command -v hugo >/dev/null 2>&1 || ! hugo version | grep -qE '0\.(1[5-9][0-9]|[2-9][0-9][0-9])'; then
    local hugo_ver
    hugo_ver="$(curl -fsSL https://api.github.com/repos/gohugoio/hugo/releases/latest | grep -Po '"tag_name":\s*"v\K[^"]+' | head -n 1)"
    [[ -n "$hugo_ver" ]] || err "无法获取 Hugo 最新版本号"
    curl -fsSL "https://github.com/gohugoio/hugo/releases/download/v${hugo_ver}/hugo_extended_${hugo_ver}_linux-${ARCH}.tar.gz" -o /tmp/hugo.tar.gz
    tar -xzf /tmp/hugo.tar.gz -C /usr/local/bin hugo
    chmod +x /usr/local/bin/hugo
    rm -f /tmp/hugo.tar.gz
  fi

  mkdir -p "$BLOG_SRC_DIR" "$SITE_WEB_DIR"
  cd "$BLOG_SRC_DIR"
  if [[ ! -f hugo.toml ]]; then
    hugo new site . --force >/dev/null
  fi
  mkdir -p content/posts themes
  if [[ ! -d themes/PaperMod/.git ]]; then
    rm -rf themes/PaperMod
    git clone --depth=1 https://github.com/adityatelange/hugo-PaperMod.git themes/PaperMod >/dev/null 2>&1
  fi

  cat >"${BLOG_SRC_DIR}/hugo.toml" <<EOF
baseURL = 'https://${domain}/'
locale = 'zh-cn'
title = '${site_label} Notes'
theme = 'PaperMod'
hasCJKLanguage = true

[pagination]
  pagerSize = 10

[params]
  env = 'production'
  description = '后端开发、Linux 运维与系统编程学习笔记'
  author = '${site_label}'
  defaultTheme = 'auto'
  ShowReadingTime = true
  ShowPostNavLinks = true
  ShowBreadCrumbs = true
  ShowCodeCopyButtons = true
  ShowToc = true

[params.homeInfoParams]
  Title = '${site_label} Notes'
  Content = '记录日常系统编程、Linux 运维排查与开源工具实践。'

[menu]
  [[menu.main]]
    identifier = 'posts'
    name = '文章'
    url = '/posts/'
    weight = 10
  [[menu.main]]
    identifier = 'archives'
    name = '归档'
    url = '/archives/'
    weight = 20
  [[menu.main]]
    identifier = 'about'
    name = '关于'
    url = '/about/'
    weight = 30
EOF

  if [[ ! -f "${BLOG_SRC_DIR}/content/archives.md" ]]; then
    cat >"${BLOG_SRC_DIR}/content/archives.md" <<'EOF'
---
title: "归档"
layout: "archives"
url: "/archives/"
summary: "文章归档"
---
EOF
  fi

  if [[ ! -f "${BLOG_SRC_DIR}/content/about.md" ]]; then
    cat >"${BLOG_SRC_DIR}/content/about.md" <<EOF
---
title: "关于"
url: "/about/"
summary: "关于 ${site_label} Notes"
---

你好，这里是 **${site_label} Notes**，一个用于记录日常技术折腾的个人静态博客。

### 关注方向

- Rust 与系统级网络编程
- Linux 服务器日常维护与性能排查
- 自动化脚本与效率工具

本站使用 [Hugo](https://gohugo.io/) 与 PaperMod 主题构建，纯静态托管。
EOF
  fi

  if [[ -z "$(ls -A "${BLOG_SRC_DIR}/content/posts" 2>/dev/null)" ]]; then
    cat >"${BLOG_SRC_DIR}/content/posts/linux-troubleshooting-cheatsheet.md" <<'EOF'
---
title: "Linux 服务器日常排查常用命令备忘"
date: 2026-10-18T10:30:00+08:00
tags: ["Linux", "运维"]
summary: "整理日常排查 CPU、内存、磁盘 IO 与端口占用时最常用的几条 Linux 命令。"
---

平时维护几台个人 Linux 主机，遇到服务异常或负载升高时，最常用到的还是下面这几条基础命令。

## 1. 查看端口与连接状态

```bash
ss -tulnp
ss -tan | awk 'NR>1 {print $1}' | sort | uniq -c | sort -rn
```

## 2. 快速定位磁盘空间占用

```bash
df -h
du -h --max-depth=1 | sort -hr | head -n 15
```

## 3. 查看 systemd 服务日志

```bash
journalctl -u nginx -n 100 -f
journalctl --since today -p err
```
EOF

    cat >"${BLOG_SRC_DIR}/content/posts/rust-tokio-graceful-shutdown.md" <<'EOF'
---
title: "Rust Tokio 异步任务优雅退出的常见模式"
date: 2026-11-05T19:15:00+08:00
tags: ["Rust", "Tokio"]
summary: "使用 tokio::sync::watch 与 select! 统一协调多个后台异步任务的关闭流程。"
---

在写 Rust 网络服务或守护进程时，往往会同时拉起多个后台任务。使用 `tokio::sync::watch` 可以非常简洁地实现一发多收的停机广播。

```rust
use tokio::sync::watch;

async fn worker(mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
        }
    }
}
```
EOF

    cat >"${BLOG_SRC_DIR}/content/posts/nginx-static-site-https.md" <<'EOF'
---
title: "Nginx 静态站点 HTTPS 与缓存头基础配置"
date: 2026-11-20T14:00:00+08:00
tags: ["Nginx", "Web"]
summary: "记录一套简洁可靠的 Nginx 静态博客 HTTPS 配置，包括 TLS 1.3、Gzip 压缩与静态资源缓存。"
---

纯静态博客配合 Nginx 托管，几乎不需要维护成本，单核小内存主机也能轻松应对高并发访问。

- **强制 HTTPS**：80 端口除 ACME 证书验证目录外全部 `301` 跳转到 `https://`。
- **开启 TLS 1.3 与 HTTP/2**：减少握手延迟，提升首次加载体验。
- **静态资源缓存**：对 `.css`、`.js`、字体与图片开启长期缓存。
EOF
  fi

  cat >"${BLOG_SRC_DIR}/build.sh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
cd "${BLOG_SRC_DIR}"
hugo --minify --destination "${SITE_WEB_DIR}" --cleanDestinationDir
echo "Build complete -> ${SITE_WEB_DIR}"
EOF
  chmod +x "${BLOG_SRC_DIR}/build.sh"
  "${BLOG_SRC_DIR}/build.sh"

  cat >/etc/nginx/sites-available/gacl-site.conf <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name ${domain};

    location ^~ /.well-known/acme-challenge/ {
        root ${CERTBOT_WEBROOT};
        default_type "text/plain";
    }

    location / {
        return 301 https://${domain}\$request_uri;
    }
}

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name ${domain};

    ssl_certificate /etc/letsencrypt/live/${domain}/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/${domain}/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_prefer_server_ciphers off;
    ssl_session_cache shared:SSL:10m;
    ssl_session_timeout 1d;

    root ${SITE_WEB_DIR};
    index index.html;

    gzip on;
    gzip_types text/plain text/css application/json application/javascript text/xml application/xml text/javascript image/svg+xml;
    gzip_min_length 256;

    add_header Alt-Svc 'h3=":443"; ma=2592000' always;
    add_header X-Content-Type-Options "nosniff" always;
    add_header X-Frame-Options "SAMEORIGIN" always;
    add_header Referrer-Policy "strict-origin-when-cross-origin" always;

    location ~* \.(css|js|png|jpg|jpeg|webp|svg|ico|woff2)$ {
        expires 30d;
        add_header Cache-Control "public, immutable";
        add_header Alt-Svc 'h3=":443"; ma=2592000' always;
        try_files \$uri =404;
    }

    location / {
        try_files \$uri \$uri/ =404;
    }
}
EOF
  ln -sf /etc/nginx/sites-available/gacl-site.conf /etc/nginx/sites-enabled/gacl-site.conf
  nginx -t
  systemctl reload nginx
  ok "Nginx HTTPS 静态博客已上线: https://${domain}/（新增文章后运行 ${BLOG_SRC_DIR}/build.sh 更新）"
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
echo " 1. 查看服务状态、面板密码与全部 gacl:// / hysteria2:// 链接"
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
    read -r -p "请输入已解析到本机 (${DEFAULT_IP}) 的域名（留空则使用自签测试证书）: " SNI_DOMAIN
    SNI_DOMAIN="${SNI_DOMAIN:-localhost}"

    local default_pub="$DEFAULT_IP"
    if [[ "$SNI_DOMAIN" != "localhost" ]]; then
      default_pub="$SNI_DOMAIN"
    fi
    read -r -p "节点对外连接地址 [${default_pub}]: " PUBLIC_HOST
    PUBLIC_HOST="${PUBLIC_HOST:-$default_pub}"

    read -r -p "QUIC UDP 监听端口 [${DEFAULT_UDP}]: " UDP_PORT
    UDP_PORT="${UDP_PORT:-$DEFAULT_UDP}"

    read -r -p "Web 管理后台端口 [${DEFAULT_PANEL_PORT}]: " PANEL_PORT
    PANEL_PORT="${PANEL_PORT:-$DEFAULT_PANEL_PORT}"

    DEPLOY_SITE="1"
    if [[ "$SNI_DOMAIN" != "localhost" ]]; then
      read -r -p "是否自动申请证书并部署 Nginx + Hugo 静态伪装博客? [Y/n]: " yn
      [[ "$yn" =~ ^[Nn]$ ]] && DEPLOY_SITE="0"
    else
      DEPLOY_SITE="0"
    fi
  else
    SNI_DOMAIN="${GACL_DOMAIN:-${GACL_SNI:-localhost}}"
    PUBLIC_HOST="${GACL_PUBLIC_HOST:-$([[ "$SNI_DOMAIN" != "localhost" ]] && echo "$SNI_DOMAIN" || echo "$DEFAULT_IP")}"
    UDP_PORT="${GACL_UDP_PORT:-$DEFAULT_UDP}"
    PANEL_PORT="${GACL_PANEL_PORT:-$DEFAULT_PANEL_PORT}"
    DEPLOY_SITE="${GACL_DEPLOY_SITE:-$([[ "$SNI_DOMAIN" != "localhost" ]] && echo "1" || echo "0")}"
  fi

  CERT_ARGS=""
  MASQ_ARGS=""

  if [[ "$SNI_DOMAIN" != "localhost" ]]; then
    check_domain_dns "$SNI_DOMAIN" "$DEFAULT_IP"
    install_web_packages
    provision_letsencrypt_cert "$SNI_DOMAIN"
    CERT_ARGS="--cert /etc/letsencrypt/live/${SNI_DOMAIN}/fullchain.pem --key /etc/letsencrypt/live/${SNI_DOMAIN}/privkey.pem"

    if [[ "$DEPLOY_SITE" == "1" ]]; then
      deploy_hugo_masquerade_site "$SNI_DOMAIN"
      MASQ_ARGS="--masquerade-dir ${SITE_WEB_DIR}"
    fi
  else
    warn "未提供域名，将使用自签测试证书（不适合公网长期使用）"
  fi

  if [[ -n "${GACL_MASQUERADE_DIR:-}" && -d "${GACL_MASQUERADE_DIR}" ]]; then
    MASQ_ARGS="--masquerade-dir ${GACL_MASQUERADE_DIR}"
  fi

  SEED_TOKEN="${GACL_TOKEN:-$(rand_str 24)}"
  PANEL_PASS_ARG=""
  if [[ -n "${GACL_PANEL_PASS:-}" ]]; then
    PANEL_PASS_ARG="--panel-pass ${GACL_PANEL_PASS}"
  fi

  cat >"$SERVICE_FILE" <<EOF
[Unit]
Description=GameAccel All-in-One QUIC/UDP Server & Web Panel
After=network-online.target nginx.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=${BIN_PATH} --listen 0.0.0.0:${UDP_PORT} --panel-listen 127.0.0.1:${PANEL_PORT} --data-file ${DATA_FILE} --public-host ${PUBLIC_HOST} --sni ${SNI_DOMAIN} --token ${SEED_TOKEN} ${PANEL_PASS_ARG} ${CERT_ARGS} ${MASQ_ARGS}
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
  if [[ "$SNI_DOMAIN" != "localhost" ]]; then
    echo -e "  伪装站点与证书: ${GREEN}https://${SNI_DOMAIN}/${NC} (Certbot 自动续签已开启)"
  fi
  echo -e "  Web 管理后台 (仅监听 127.0.0.1:${PANEL_PORT}，公网不可扫描):"
  echo -e "    连入加速器后浏览器访问: ${GREEN}http://panel.gacl.local${NC}  (TUN 模式: ${GREEN}http://198.18.0.1${NC})"
  echo -e "  随时运行 ${CYAN}gacl${NC} 打开运维菜单查看状态与全部 gacl:// / hysteria2:// 分享链接"
  echo ""
  "$CLI_PATH" <<< "1" || true
}

main "$@"
