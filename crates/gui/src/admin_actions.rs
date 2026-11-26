use crate::admin::{self, AdminEvent};
use crate::storage::save_config;
use crate::{AsyncEvent, ConnState, GameAccelApp};

impl GameAccelApp {
    pub(crate) fn admin_select_server(&mut self, server_id: String) {
        if self.cfg.active_admin_server_id == server_id {
            return;
        }
        self.cfg.active_admin_server_id = server_id;
        save_config(&self.cfg);
        self.admin.clear_session();
        self.admin.login_error = None;
        self.admin.login_pass.clear();
        if let Some(srv) = self.cfg.active_admin_server() {
            self.admin.login_user = srv.username.clone();
        }
    }

    pub(crate) fn admin_login(&mut self) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            self.show_toast("请先配置远程服务端");
            return;
        };
        if server.via_tunnel && self.state != ConnState::Connected {
            self.admin.login_error =
                Some("当前配置为「经加密隧道访问」，请先开启加速后再登录".to_string());
            return;
        }
        let username = self.admin.login_user.trim().to_string();
        let password = self.admin.login_pass.clone();
        if username.is_empty() || password.is_empty() {
            self.admin.login_error = Some("请输入管理员账号和密码".to_string());
            return;
        }
        // Persist username convenience only (never password)
        if let Some(s) = self
            .cfg
            .admin_servers
            .iter_mut()
            .find(|s| s.id == server.id)
            && s.username != username
        {
            s.username = username.clone();
            save_config(&self.cfg);
        }

        self.admin.busy = true;
        self.admin.login_error = None;
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        self.rt.spawn(async move {
            let result = admin::api_login(&server, &listen_addr, &username, &password)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::LoginResult {
                server_id,
                result,
            }));
        });
    }

    pub(crate) fn admin_logout(&mut self) {
        if let (Some(server), Some(cookie)) = (
            self.cfg.active_admin_server().cloned(),
            self.admin.session_cookie.clone(),
        ) {
            let listen_addr = self.cfg.listen_addr.clone();
            self.rt.spawn(async move {
                admin::api_logout(&server, &listen_addr, &cookie).await;
            });
        }
        self.admin.clear_session();
        self.show_toast("已退出服务端管理登录");
    }

    pub(crate) fn admin_refresh(&mut self) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            return;
        };
        let Some(cookie) = self.admin.session_cookie.clone() else {
            return;
        };
        self.admin.busy = true;
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        self.rt.spawn(async move {
            let result = admin::api_fetch_dashboard(&server, &listen_addr, &cookie).await;
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::DashboardData {
                server_id,
                result,
            }));
        });
    }

    pub(crate) fn admin_save_link(
        &mut self,
        editing_id: Option<String>,
        name: String,
        game_dup: bool,
        traffic_limit_gb: Option<f64>,
        expire_at: Option<String>,
    ) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            return;
        };
        let Some(cookie) = self.admin.session_cookie.clone() else {
            return;
        };
        self.admin.busy = true;
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        let ok_msg = if editing_id.is_some() {
            "分享链接已更新".to_string()
        } else {
            "新分享链接已创建".to_string()
        };
        self.rt.spawn(async move {
            let result = admin::api_save_link(
                &server,
                &listen_addr,
                &cookie,
                editing_id.as_deref(),
                &name,
                game_dup,
                traffic_limit_gb,
                expire_at.as_deref(),
            )
            .await;
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::MutationResult {
                server_id,
                ok_msg,
                result,
            }));
        });
    }

    pub(crate) fn admin_toggle_link(&mut self, id: String, enabled: bool) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            return;
        };
        let Some(cookie) = self.admin.session_cookie.clone() else {
            return;
        };
        self.admin.busy = true;
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        let ok_msg = if enabled {
            "已启用该链接".to_string()
        } else {
            "已禁用并断开该链接活跃会话".to_string()
        };
        self.rt.spawn(async move {
            let result = admin::api_toggle_link(&server, &listen_addr, &cookie, &id, enabled).await;
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::MutationResult {
                server_id,
                ok_msg,
                result,
            }));
        });
    }

    pub(crate) fn admin_post_link_action(
        &mut self,
        id: String,
        action: &'static str,
        ok_msg: &str,
    ) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            return;
        };
        let Some(cookie) = self.admin.session_cookie.clone() else {
            return;
        };
        self.admin.busy = true;
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        let ok_msg = ok_msg.to_string();
        self.rt.spawn(async move {
            let result =
                admin::api_post_link_action(&server, &listen_addr, &cookie, &id, action).await;
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::MutationResult {
                server_id,
                ok_msg,
                result,
            }));
        });
    }

    pub(crate) fn admin_delete_link(&mut self, id: String) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            return;
        };
        let Some(cookie) = self.admin.session_cookie.clone() else {
            return;
        };
        self.admin.busy = true;
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        self.rt.spawn(async move {
            let result = admin::api_delete_link(&server, &listen_addr, &cookie, &id).await;
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::MutationResult {
                server_id,
                ok_msg: "已删除分享链接".to_string(),
                result,
            }));
        });
    }
}
