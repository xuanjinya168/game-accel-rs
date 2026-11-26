//! Black-box integration tests for the Web Admin Panel & Subscription HTTP API
//! (*Zero To Production in Rust* style: real TCP listener on ephemeral port `127.0.0.1:0`).

use protocol::domain::LinkStatus;
use protocol::share::SubscriptionPayload;
use reqwest::StatusCode;
use reqwest::header::{COOKIE, SET_COOKIE};
use server::panel::build_panel_router;
use server::store::{AppState, CreateLinkInput, InitParams, LinkView, OverviewView, UpdateLinkInput};
use std::net::SocketAddr;

#[tokio::test]
async fn panel_http_lifecycle_and_subscription_contract() {
    let temp_file = std::env::temp_dir().join(format!(
        "gacl-api-it-{}.json",
        server::store::generate_random_str(8)
    ));
    let (state, _) = AppState::load_or_init(InitParams {
        data_path: temp_file.clone(),
        seed_token: "seed-sub-token-001".to_string(),
        public_host: "203.0.113.88".to_string(),
        udp_port: 8443,
        sni: "edge.example.com".to_string(),
        insecure: false,
        initial_panel_pass: Some("AdminSecret99".to_string()),
    })
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_panel_router(state);
    let srv = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let base = format!("http://{addr}");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    // 1. Unauthenticated overview must return 401
    let unauth = client.get(format!("{base}/api/overview")).send().await.unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // 2. Login and extract session cookie
    let login_res = client
        .post(format!("{base}/api/login"))
        .body(r#"{"username":"admin","password":"AdminSecret99"}"#)
        .header("Content-Type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(login_res.status(), StatusCode::OK);
    let raw_cookie = login_res
        .headers()
        .get(SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // 3. Authenticated overview & create link
    let ov: OverviewView = serde_json::from_str(
        &client
            .get(format!("{base}/api/overview"))
            .header(COOKIE, &raw_cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(ov.public_host, "203.0.113.88");
    assert_eq!(ov.active_links, 1);

    let create_body = serde_json::to_string(&CreateLinkInput {
        name: "电竞好友专线".to_string(),
        game_dup: true,
        traffic_limit_gb: Some(50.0),
        expire_at: Some("2099-12-31".to_string()),
    })
    .unwrap();
    let created_resp = client
        .post(format!("{base}/api/links"))
        .header(COOKIE, &raw_cookie)
        .header("Content-Type", "application/json")
        .body(create_body)
        .send()
        .await
        .unwrap();
    assert_eq!(created_resp.status(), StatusCode::CREATED);
    let created: LinkView = serde_json::from_str(&created_resp.text().await.unwrap()).unwrap();
    assert_eq!(created.status, LinkStatus::Active);

    // 4. Subscription endpoint returns valid SubscriptionPayload & Userinfo header
    let sub_resp = client
        .get(format!("{base}/sub/{}", created.token))
        .send()
        .await
        .unwrap();
    assert_eq!(sub_resp.status(), StatusCode::OK);
    assert!(sub_resp.headers().get("Subscription-Userinfo").is_some());
    let sub: SubscriptionPayload = serde_json::from_str(&sub_resp.text().await.unwrap()).unwrap();
    assert_eq!(sub.nodes.len(), 1);
    assert_eq!(sub.nodes[0].server, "203.0.113.88:8443");

    // 5. Disabling the link immediately blocks subscription access
    let disable_body = serde_json::to_string(&UpdateLinkInput {
        name: None,
        enabled: Some(false),
        game_dup: None,
        traffic_limit_gb: None,
        expire_at: None,
    })
    .unwrap();
    let updated: LinkView = serde_json::from_str(
        &client
            .put(format!("{base}/api/links/{}", created.id))
            .header(COOKIE, &raw_cookie)
            .header("Content-Type", "application/json")
            .body(disable_body)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(updated.status, LinkStatus::Disabled);

    let blocked_sub = client
        .get(format!("{base}/sub/{}", created.token))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked_sub.status(), StatusCode::FORBIDDEN);

    srv.abort();
    let _ = std::fs::remove_file(&temp_file);
    let _ = std::fs::remove_file(temp_file.with_file_name("access.log"));
}
