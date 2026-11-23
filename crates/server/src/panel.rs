use crate::store::{AppState, CreateLinkInput, UpdateLinkInput, UpdateNodeSettingsInput};
use anyhow::Result;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

const PANEL_HTML: &str = include_str!("panel_ui.html");
const COOKIE_NAME: &str = "gacl_sid";

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct PasswordReq {
    old_password: String,
    new_password: String,
}

#[derive(Deserialize)]
struct LogsQuery {
    link_id: Option<String>,
    proto: Option<String>,
    q: Option<String>,
}

#[derive(Deserialize)]
struct ToggleLogsReq {
    enabled: bool,
}

#[derive(Serialize)]
struct ErrorResp {
    error: String,
}

fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorResp { error: msg.into() })).into_response()
}

fn extract_sid(headers: &HeaderMap) -> Option<String> {
    let cookie_hdr = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie_hdr.split(';') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn require_auth(headers: &HeaderMap, state: &AppState) -> std::result::Result<(), Response> {
    let Some(sid) = extract_sid(headers) else {
        return Err(json_err(StatusCode::UNAUTHORIZED, "未登录"));
    };
    if !state.verify_session(&sid) {
        return Err(json_err(StatusCode::UNAUTHORIZED, "会话已过期"));
    }
    Ok(())
}

pub async fn run_panel_server(listen: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/sub/{token}", get(subscription_handler))
        .route("/api/login", post(login_handler))
        .route("/api/logout", post(logout_handler))
        .route("/api/overview", get(overview_handler))
        .route("/api/links", get(list_links_handler).post(create_link_handler))
        .route(
            "/api/links/{id}",
            put(update_link_handler).delete(delete_link_handler),
        )
        .route("/api/links/{id}/reset-token", post(reset_token_handler))
        .route("/api/links/{id}/reset-traffic", post(reset_traffic_handler))
        .route("/api/logs", get(list_logs_handler))
        .route("/api/logs/clear", post(clear_logs_handler))
        .route("/api/logs/toggle", put(toggle_logs_handler))
        .route("/api/settings/node", put(update_node_settings_handler))
        .route("/api/settings/password", put(update_password_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!("Web admin panel listening on http://{}", listen);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn index_handler() -> Html<&'static str> {
    Html(PANEL_HTML)
}

async fn login_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> Response {
    let ip = addr.ip().to_string();
    if let Err(remaining) = state.check_login_throttle(addr.ip()) {
        state.record_access_log(
            "",
            "管理后台",
            &ip,
            "PANEL",
            "login".to_string(),
            format!(
                "POST /api/login 已被限流 (user={})，剩余 {} 秒",
                req.username,
                remaining.as_secs() + 1
            ),
            0,
            0,
            0,
            "denied",
        );
        return json_err(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "登录失败次数过多，请 {} 秒后重试",
                remaining.as_secs() + 1
            ),
        );
    }
    let is_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let login = state.verify_login(&req.username, &req.password);
    state.record_login_result(addr.ip(), login.is_some());
    if let Some(sid) = login {
        state.record_access_log(
            "",
            "管理后台",
            &ip,
            "PANEL",
            "login".to_string(),
            format!("POST /api/login (user={})", req.username),
            0,
            0,
            0,
            "ok",
        );
        let mut resp = (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response();
        let secure_flag = if is_https { "; Secure" } else { "" };
        let cookie = format!(
            "{COOKIE_NAME}={sid}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400{secure_flag}"
        );
        if let Ok(val) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().insert(header::SET_COOKIE, val);
        }
        resp
    } else {
        state.record_access_log(
            "",
            "管理后台",
            &ip,
            "PANEL",
            "login".to_string(),
            format!("POST /api/login 密码错误 (user={})", req.username),
            0,
            0,
            0,
            "denied",
        );
        json_err(StatusCode::UNAUTHORIZED, "账号或密码错误")
    }
}

async fn logout_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(sid) = extract_sid(&headers) {
        state.revoke_session(&sid);
    }
    let mut resp = (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response();
    if let Ok(val) = HeaderValue::from_str(&format!("{COOKIE_NAME}=; Path=/; HttpOnly; Max-Age=0")) {
        resp.headers_mut().insert(header::SET_COOKIE, val);
    }
    resp
}

async fn overview_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    Json(state.overview()).into_response()
}

async fn list_links_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    Json(state.list_links()).into_response()
}

async fn create_link_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<CreateLinkInput>,
) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    match state.create_link(input) {
        Ok(view) => (StatusCode::CREATED, Json(view)).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn update_link_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<UpdateLinkInput>,
) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    match state.update_link(&id, input) {
        Ok(view) => Json(view).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn delete_link_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    match state.delete_link(&id) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => json_err(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
}

async fn reset_token_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    match state.reset_link_token(&id) {
        Ok(view) => Json(view).into_response(),
        Err(e) => json_err(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
}

async fn reset_traffic_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    match state.reset_link_traffic(&id) {
        Ok(view) => Json(view).into_response(),
        Err(e) => json_err(StatusCode::NOT_FOUND, format!("{e:#}")),
    }
}

async fn update_node_settings_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<UpdateNodeSettingsInput>,
) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    match state.update_node_settings(input) {
        Ok(ov) => Json(ov).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn update_password_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<PasswordReq>,
) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    match state.update_admin_password(&req.old_password, &req.new_password) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn list_logs_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<LogsQuery>,
) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    Json(state.query_access_logs(q.link_id.as_deref(), q.proto.as_deref(), q.q.as_deref()))
        .into_response()
}

async fn clear_logs_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    state.clear_access_logs();
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn toggle_logs_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ToggleLogsReq>,
) -> Response {
    if let Err(resp) = require_auth(&headers, &state) {
        return resp;
    }
    match state.set_access_log_enabled(req.enabled) {
        Ok(enabled) => Json(serde_json::json!({ "enabled": enabled })).into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn subscription_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(token): Path<String>,
) -> Response {
    let ip = addr.ip().to_string();
    match state.build_subscription(&token) {
        Ok((payload, tx, rx, total_limit)) => {
            state.record_access_log(
                "",
                &payload.name,
                &ip,
                "PANEL",
                "subscription".to_string(),
                "GET /sub/***".to_string(),
                0,
                0,
                0,
                "ok",
            );
            let mut resp = Json(payload).into_response();
            let userinfo = format!(
                "upload={tx}; download={rx}; total={}",
                total_limit.unwrap_or(0)
            );
            if let Ok(val) = HeaderValue::from_str(&userinfo) {
                resp.headers_mut().insert("Subscription-Userinfo", val);
            }
            resp
        }
        Err(e) => {
            state.record_access_log(
                "",
                "订阅拉取",
                &ip,
                "PANEL",
                "subscription".to_string(),
                format!("GET /sub/*** ({e:#})"),
                0,
                0,
                0,
                "denied",
            );
            json_err(StatusCode::FORBIDDEN, format!("{e:#}"))
        }
    }
}
