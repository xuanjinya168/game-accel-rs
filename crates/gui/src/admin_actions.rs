use crate::admin::{self, AdminEvent, AdminSubTab, LogsQueryInput, NodeSettingsPayload};
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
        if self.admin.sub_tab == AdminSubTab::Logs {
            self.admin_refresh_logs();
        }
    }

    pub(crate) fn admin_refresh_logs(&mut self) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            return;
        };
        let Some(cookie) = self.admin.session_cookie.clone() else {
            return;
        };
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        let query = LogsQueryInput {
            link_id: self.admin.log_link_filter.clone(),
            proto: self.admin.log_proto_filter.clone(),
            keyword: self.admin.log_search.clone(),
        };
        self.rt.spawn(async move {
            let result = admin::api_fetch_logs(&server, &listen_addr, &cookie, query).await;
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::LogsData {
                server_id,
                result,
            }));
        });
    }

    pub(crate) fn admin_toggle_logs(&mut self) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            return;
        };
        let Some(cookie) = self.admin.session_cookie.clone() else {
            return;
        };
        self.admin.busy = true;
        let target_enabled = !self.admin.logs_enabled;
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        self.rt.spawn(async move {
            let result =
                admin::api_toggle_logs(&server, &listen_addr, &cookie, target_enabled).await;
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::LogsToggled {
                server_id,
                result,
            }));
        });
    }

    pub(crate) fn admin_clear_logs(&mut self) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            return;
        };
        let Some(cookie) = self.admin.session_cookie.clone() else {
            return;
        };
        self.admin.busy = true;
        self.admin.confirm_clear_logs = false;
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        self.rt.spawn(async move {
            let result = admin::api_clear_logs(&server, &listen_addr, &cookie).await;
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::MutationResult {
                server_id,
                ok_msg: "访问日志已清空".to_string(),
                result,
            }));
        });
    }

    pub(crate) fn admin_save_node_settings(&mut self) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            return;
        };
        let Some(cookie) = self.admin.session_cookie.clone() else {
            return;
        };
        let public_host = self.admin.set_public_host.trim().to_string();
        if public_host.is_empty() {
            self.show_toast("公网 IP 或域名不能为空");
            return;
        }
        let Ok(udp_port) = self.admin.set_udp_port.trim().parse::<u16>() else {
            self.show_toast("QUIC UDP 端口格式无效");
            return;
        };
        if udp_port == 0 {
            self.show_toast("QUIC UDP 端口不能为 0");
            return;
        }
        let sni = self.admin.set_sni.trim().to_string();
        let insecure = self.admin.set_insecure;

        self.admin.busy = true;
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        self.rt.spawn(async move {
            let result = admin::api_update_node_settings(
                &server,
                &listen_addr,
                &cookie,
                NodeSettingsPayload {
                    public_host,
                    udp_port,
                    sni,
                    insecure,
                },
            )
            .await;
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::NodeSettingsSaved {
                server_id,
                result,
            }));
        });
    }

    pub(crate) fn admin_change_password(&mut self) {
        let Some(server) = self.cfg.active_admin_server().cloned() else {
            return;
        };
        let Some(cookie) = self.admin.session_cookie.clone() else {
            return;
        };
        let old_pass = self.admin.old_password.clone();
        let new_pass = self.admin.new_password.clone();
        if old_pass.is_empty() || new_pass.len() < 6 {
            self.show_toast("请输入原密码，且新密码长度至少 6 位");
            return;
        }
        self.admin.busy = true;
        let listen_addr = self.cfg.listen_addr.clone();
        let ev_tx = self.event_tx.clone();
        let server_id = server.id.clone();
        self.rt.spawn(async move {
            let result =
                admin::api_update_password(&server, &listen_addr, &cookie, old_pass, new_pass)
                    .await;
            let _ = ev_tx.send(AsyncEvent::Admin(AdminEvent::PasswordUpdated {
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
                admin::SaveLinkRequest {
                    editing_id,
                    name,
                    game_dup,
                    traffic_limit_gb,
                    expire_at,
                },
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
