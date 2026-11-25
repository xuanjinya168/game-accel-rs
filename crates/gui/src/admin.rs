use crate::storage::AdminServerProfile;
use anyhow::{Context, Result, bail};
use reqwest::header::{CONTENT_TYPE, COOKIE, SET_COOKIE};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OverviewView {
    pub uptime_secs: u64,
    pub public_host: String,
    pub udp_port: u16,
    pub sni: String,
    #[allow(dead_code)]
    pub insecure: bool,
    pub total_links: usize,
    pub active_links: usize,
    pub total_online_conns: u64,
    pub total_tx_bytes: u64,
    pub total_rx_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LinkView {
    pub id: String,
    pub name: String,
    pub token: String,
    pub enabled: bool,
    pub game_dup: bool,
    pub traffic_limit_gb: Option<f64>,
    pub expire_at: Option<String>,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub online_conns: u64,
    #[allow(dead_code)]
    pub created_at: String,
    pub share_uri: String,
    #[serde(default)]
    pub hy2_uri: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AdminLinkEditor {
    pub editing_id: Option<String>,
    pub name: String,
    pub traffic_limit_gb: String,
    pub expire_at: String,
    pub game_dup: bool,
}

impl AdminLinkEditor {
    pub(crate) fn new_blank() -> Self {
        Self {
            editing_id: None,
            name: String::new(),
            traffic_limit_gb: String::new(),
            expire_at: String::new(),
            game_dup: true,
        }
    }

    pub(crate) fn from_link(l: &LinkView) -> Self {
        Self {
            editing_id: Some(l.id.clone()),
            name: l.name.clone(),
            traffic_limit_gb: l
                .traffic_limit_gb
                .map(|v| format!("{v}"))
                .unwrap_or_default(),
            expire_at: l.expire_at.clone().unwrap_or_default(),
            game_dup: l.game_dup,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AdminServerEditor {
    pub editing_id: Option<String>,
    pub name: String,
    pub base_url: String,
    pub via_tunnel: bool,
    pub username: String,
}

impl AdminServerEditor {
    pub(crate) fn new_blank() -> Self {
        Self {
            editing_id: None,
            name: String::new(),
            base_url: "https://".to_string(),
            via_tunnel: false,
            username: "admin".to_string(),
        }
    }

    pub(crate) fn from_profile(p: &AdminServerProfile) -> Self {
        Self {
            editing_id: Some(p.id.clone()),
            name: p.name.clone(),
            base_url: p.base_url.clone(),
            via_tunnel: p.via_tunnel,
            username: p.username.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum AdminEvent {
    LoginResult {
        server_id: String,
        result: Result<String, String>,
    },
    DashboardData {
        server_id: String,
        result: Result<(OverviewView, Vec<LinkView>), AdminApiErr>,
    },
    MutationResult {
        server_id: String,
        ok_msg: String,
        result: Result<(), AdminApiErr>,
    },
}

#[derive(Debug)]
pub(crate) enum AdminApiErr {
    Unauthorized,
    Message(String),
}

pub(crate) struct AdminState {
    /// Active server ID that the current `session_cookie` belongs to.
    pub logged_in_server_id: Option<String>,
    pub session_cookie: Option<String>,
    pub login_user: String,
    pub login_pass: String,
    pub login_error: Option<String>,
    pub busy: bool,
    pub overview: Option<OverviewView>,
    pub links: Vec<LinkView>,
    pub last_refresh: Option<Instant>,
    pub link_editor: Option<AdminLinkEditor>,
    pub server_editor: Option<AdminServerEditor>,
    /// Inline confirmation guard for destructive actions (delete / reset token).
    pub confirm_action: Option<(String, String)>, // (action_kind, link_id)
}

impl Default for AdminState {
    fn default() -> Self {
        Self {
            logged_in_server_id: None,
            session_cookie: None,
            login_user: "admin".to_string(),
            login_pass: String::new(),
            login_error: None,
            busy: false,
            overview: None,
            links: Vec::new(),
            last_refresh: None,
            link_editor: None,
            server_editor: None,
            confirm_action: None,
        }
    }
}

impl AdminState {
    pub(crate) fn clear_session(&mut self) {
        self.logged_in_server_id = None;
        self.session_cookie = None;
        self.overview = None;
        self.links.clear();
        self.last_refresh = None;
        self.link_editor = None;
        self.confirm_action = None;
        self.busy = false;
    }

    pub(crate) fn is_logged_in_for(&self, server_id: &str) -> bool {
        self.session_cookie.is_some() && self.logged_in_server_id.as_deref() == Some(server_id)
    }
}

pub(crate) fn format_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}天 {h}小时")
    } else if h > 0 {
        format!("{h}小时 {m}分")
    } else {
        format!("{m}分钟")
    }
}

fn normalized_base(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn build_client(via_tunnel: bool, listen_addr: &str) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
    if via_tunnel {
        let proxy_url = format!("http://{}", listen_addr.trim());
        let proxy = reqwest::Proxy::all(&proxy_url)
            .with_context(|| format!("构造本地代理地址失败: {proxy_url}"))?;
        builder = builder.proxy(proxy);
    } else {
        builder = builder.no_proxy();
    }
    builder.build().context("初始化 HTTP 客户端失败")
}

async fn parse_error_msg(res: reqwest::Response) -> String {
    let status = res.status();
    if let Ok(text) = res.text().await
        && let Ok(body) = serde_json::from_str::<ApiErrorBody>(&text)
        && let Some(err) = body.error
    {
        return err;
    }
    format!("请求失败 (HTTP {})", status.as_u16())
}

pub(crate) async fn api_login(
    server: &AdminServerProfile,
    listen_addr: &str,
    username: &str,
    password: &str,
) -> Result<String> {
    let client = build_client(server.via_tunnel, listen_addr)?;
    let url = format!("{}/api/login", normalized_base(&server.base_url));
    let body = serde_json::json!({
        "username": username,
        "password": password,
    })
    .to_string();
    let res = client
        .post(&url)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .with_context(|| {
            if server.via_tunnel {
                "无法连接隧道内管理接口（请确认加速器已连接）".to_string()
            } else {
                format!("无法连接远程服务端: {}", server.base_url)
            }
        })?;

    if !res.status().is_success() {
        bail!("{}", parse_error_msg(res).await);
    }

    for val in res.headers().get_all(SET_COOKIE) {
        if let Ok(s) = val.to_str()
            && let Some(first) = s.split(';').next()
            && let Some(sid) = first.trim().strip_prefix("gacl_sid=")
            && !sid.is_empty()
        {
            return Ok(format!("gacl_sid={sid}"));
        }
    }
    bail!("登录响应未返回会话 Cookie")
}

pub(crate) async fn api_logout(server: &AdminServerProfile, listen_addr: &str, cookie: &str) {
    if let Ok(client) = build_client(server.via_tunnel, listen_addr) {
        let url = format!("{}/api/logout", normalized_base(&server.base_url));
        let _ = client.post(url).header(COOKIE, cookie).send().await;
    }
}

pub(crate) async fn api_fetch_dashboard(
    server: &AdminServerProfile,
    listen_addr: &str,
    cookie: &str,
) -> std::result::Result<(OverviewView, Vec<LinkView>), AdminApiErr> {
    let client = build_client(server.via_tunnel, listen_addr)
        .map_err(|e| AdminApiErr::Message(format!("{e:#}")))?;
    let base = normalized_base(&server.base_url);

    let ov_res = client
        .get(format!("{base}/api/overview"))
        .header(COOKIE, cookie)
        .send()
        .await
        .map_err(|e| AdminApiErr::Message(format!("拉取节点概览失败: {e}")))?;
    if ov_res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(AdminApiErr::Unauthorized);
    }
    if !ov_res.status().is_success() {
        return Err(AdminApiErr::Message(parse_error_msg(ov_res).await));
    }
    let ov_text = ov_res
        .text()
        .await
        .map_err(|e| AdminApiErr::Message(format!("读取概览响应失败: {e}")))?;
    let ov: OverviewView = serde_json::from_str(&ov_text)
        .map_err(|e| AdminApiErr::Message(format!("解析概览数据失败: {e}")))?;

    let links_res = client
        .get(format!("{base}/api/links"))
        .header(COOKIE, cookie)
        .send()
        .await
        .map_err(|e| AdminApiErr::Message(format!("拉取分享链接失败: {e}")))?;
    if links_res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(AdminApiErr::Unauthorized);
    }
    if !links_res.status().is_success() {
        return Err(AdminApiErr::Message(parse_error_msg(links_res).await));
    }
    let links_text = links_res
        .text()
        .await
        .map_err(|e| AdminApiErr::Message(format!("读取链接响应失败: {e}")))?;
    let links: Vec<LinkView> = serde_json::from_str(&links_text)
        .map_err(|e| AdminApiErr::Message(format!("解析链接列表失败: {e}")))?;

    Ok((ov, links))
}

#[derive(Serialize)]
struct CreateLinkPayload<'a> {
    name: &'a str,
    game_dup: bool,
    traffic_limit_gb: Option<f64>,
    expire_at: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn api_save_link(
    server: &AdminServerProfile,
    listen_addr: &str,
    cookie: &str,
    editing_id: Option<&str>,
    name: &str,
    game_dup: bool,
    traffic_limit_gb: Option<f64>,
    expire_at: Option<&str>,
) -> std::result::Result<(), AdminApiErr> {
    let client = build_client(server.via_tunnel, listen_addr)
        .map_err(|e| AdminApiErr::Message(format!("{e:#}")))?;
    let base = normalized_base(&server.base_url);
    let body = serde_json::to_string(&CreateLinkPayload {
        name,
        game_dup,
        traffic_limit_gb,
        expire_at,
    })
    .map_err(|e| AdminApiErr::Message(format!("序列化请求失败: {e}")))?;

    let req = if let Some(id) = editing_id {
        client
            .put(format!("{base}/api/links/{id}"))
            .header(COOKIE, cookie)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
    } else {
        client
            .post(format!("{base}/api/links"))
            .header(COOKIE, cookie)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
    };

    let res = req
        .send()
        .await
        .map_err(|e| AdminApiErr::Message(format!("保存链接失败: {e}")))?;
    check_mutation_resp(res).await
}

pub(crate) async fn api_toggle_link(
    server: &AdminServerProfile,
    listen_addr: &str,
    cookie: &str,
    id: &str,
    enabled: bool,
) -> std::result::Result<(), AdminApiErr> {
    let client = build_client(server.via_tunnel, listen_addr)
        .map_err(|e| AdminApiErr::Message(format!("{e:#}")))?;
    let base = normalized_base(&server.base_url);
    let body = serde_json::json!({ "enabled": enabled }).to_string();
    let res = client
        .put(format!("{base}/api/links/{id}"))
        .header(COOKIE, cookie)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| AdminApiErr::Message(format!("切换状态失败: {e}")))?;
    check_mutation_resp(res).await
}

pub(crate) async fn api_post_link_action(
    server: &AdminServerProfile,
    listen_addr: &str,
    cookie: &str,
    id: &str,
    action: &str, // "reset-token" | "reset-traffic"
) -> std::result::Result<(), AdminApiErr> {
    let client = build_client(server.via_tunnel, listen_addr)
        .map_err(|e| AdminApiErr::Message(format!("{e:#}")))?;
    let base = normalized_base(&server.base_url);
    let res = client
        .post(format!("{base}/api/links/{id}/{action}"))
        .header(COOKIE, cookie)
        .send()
        .await
        .map_err(|e| AdminApiErr::Message(format!("操作失败: {e}")))?;
    check_mutation_resp(res).await
}

pub(crate) async fn api_delete_link(
    server: &AdminServerProfile,
    listen_addr: &str,
    cookie: &str,
    id: &str,
) -> std::result::Result<(), AdminApiErr> {
    let client = build_client(server.via_tunnel, listen_addr)
        .map_err(|e| AdminApiErr::Message(format!("{e:#}")))?;
    let base = normalized_base(&server.base_url);
    let res = client
        .delete(format!("{base}/api/links/{id}"))
        .header(COOKIE, cookie)
        .send()
        .await
        .map_err(|e| AdminApiErr::Message(format!("删除链接失败: {e}")))?;
    check_mutation_resp(res).await
}

async fn check_mutation_resp(res: reqwest::Response) -> std::result::Result<(), AdminApiErr> {
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(AdminApiErr::Unauthorized);
    }
    if !res.status().is_success() {
        return Err(AdminApiErr::Message(parse_error_msg(res).await));
    }
    Ok(())
}
