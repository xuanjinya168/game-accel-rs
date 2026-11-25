# game-accel-rs

基于 Rust 与 QUIC（TLS 1.3 + BBR）的极简低延迟游戏加速与加密代理套件，包含协议库、Linux All-in-One 服务端（内置 Web 管理后台）、跨平台 CLI 客户端与 Windows 原生桌面端。

## 架构与特性

### 1. 传输与协议 (`crates/protocol`)
- **QUIC 双协议传输**：基于 `quinn` + `rustls` (TLS 1.3)，同一 UDP 端口原生同时支持 `gacl://` 与标准 `hysteria2://` / `hy2://` 客户端，默认启用 BBR 拥塞控制与 `initial_mtu = 1200` 防止路径分片。
- **可选 TCP 通道 Brutal 拥塞控制**：默认关闭（`0` = BBR），开启后仅作用于 TCP 通道（游戏 UDP 通道始终保持 BBR），内置 5 秒滑窗丢包补偿（2.5× 封顶）、基于 `min_rtt` 的排队保护及有界低窗口 ProbeRTT。
- **游戏 UDP 通道**：通过 QUIC Datagram（RFC 9221）转发 UDP，单连接共享有界队列（`UdpDupPacer`）实时监测 QUIC 丢包率，链路健康时零冗余发包，检测到丢包（≥ 0.5%）时自动对 `< 200B` 游戏小包开启 2–5ms 抖动零拷贝双发，配合 256-bit 滑动窗口去重（支持乱序与 `u16` 回绕）。
- **节点分享与订阅**：内置 `gacl://` 与 `hysteria2://` / `hy2://` 分享链接（支持可选 `&brutal=<mbps>`）及订阅 JSON 编解码。

### 2. All-in-One 服务端 (`crates/server`)
- **Full Cone NAT (NAT1)**：按 `flow_id` 同步创建独立 UDP Socket（并发首包无竞态），支持 60s 空闲回收。
- **零延迟 TCP 转发与主域名日志**：反向管道 `s2c` 在连接建立后立即启动（SSH / 服务端先发包协议零阻塞），前向首包异步嗅探 TLS SNI / HTTP Host 提取主域名，内存环形缓冲 + 10MB 轮转落盘 `access.log`。
- **内置 Web 管理面板**：Axum 驱动的单二进制面板，默认仅绑定 `127.0.0.1:9090`（公网不暴露明文管理端口），连入已鉴权隧道后直接访问 `http://panel.gacl.local`（代理模式）或 `http://198.18.0.1`（TUN 模式）即可管理多链路、配额/到期、访问日志与 `/sub/{token}` 订阅。
- **鉴权与存储安全**：管理员密码采用加盐 PBKDF2-HMAC-SHA256 哈希落盘（自动迁移旧明文配置），Token 与密码比对使用定长 SHA-256 常量时间比较；HTTPS 反代下自动附带 `Secure` Cookie 属性，配置文件在 Linux 下设为 `0600` 权限。

### 3. 客户端内核与 Windows 桌面端 (`crates/client`, `crates/gui`)
- **三种代理模式**：
  - **TUN 全局虚拟网卡**：基于 `Wintun` + `netstack-smoltcp`，安装 `0.0.0.0/1` + `128.0.0.0/1` 半默认路由与服务器直连防回环路由（`RouteGuard` RAII 退出自动还原）；SOCKS5 (`1..=32767`) 与 TUN (`32768..=65534`) 严格分区分配 `flow_id`，TUN UDP 会话具备 60s 空闲回收。
  - **Windows 系统代理**：一键接管并在断开/退出时自动清理。
  - **本地混合端口**：单端口自动识别 HTTP / HTTPS `CONNECT` / SOCKS5 TCP & UDP。
- **原生桌面 GUI (`game-accel.exe`)**：无边框圆角窗口、暖米白 / 暖炭黑双主题、自定义节点弹出选择器、实时 RTT 曲线与批量 QUIC 测速、系统托盘最小化与退出清理、持久化落盘日志（`%APPDATA%\game-accel\logs\app.log`）。

## 构建与测试

### 运行全量单元测试与回环集成测试
```powershell
cargo test --workspace
```

### 编译 Windows 桌面端与 CLI
```powershell
cargo build --release -p game-accel-gui -p client
```
> 使用 TUN 模式时需将 `wintun.dll` 置于 `game-accel.exe` 同目录并以管理员身份运行（桌面端已内置 UAC Manifest）。

### Windows 本地交叉编译 Linux 服务端（`zig` + `cargo-zigbuild`）
```powershell
cargo zigbuild --release -p server --target x86_64-unknown-linux-gnu.2.36
```

### Linux VPS 一键部署
先将你的域名 A 记录解析到 VPS 公网 IP，然后以 `root` 执行：

```bash
GACL_BIN_URL="https://github.com/xuanjinya168/game-accel-rs/releases/download/v0.2.1/game-accel-server-v0.2.1-linux-x86_64.tar.gz" \
bash <(curl -fsSL https://raw.githubusercontent.com/xuanjinya168/game-accel-rs/main/scripts/install.sh)
```

脚本 [`scripts/install.sh`](scripts/install.sh) 会自动完成：
- DNS 解析校验、Let's Encrypt 证书自动申请与续签（Certbot）
- Nginx HTTPS + Hugo (PaperMod) 静态伪装博客 + HTTP/3 (`--masquerade-dir`) 伪装回落
- `systemd` 服务配置与 `/usr/local/bin/gacl` 交互式运维菜单（随时运行 `gacl` 查看状态和分享链接）
