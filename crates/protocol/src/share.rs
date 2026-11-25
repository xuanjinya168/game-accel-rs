use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const SHARE_SCHEME: &str = "gacl://";
pub const HY2_SCHEME: &str = "hysteria2://";
pub const HY2_SHORT_SCHEME: &str = "hy2://";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeProtocol {
    #[default]
    Gacl,
    Hysteria2,
}

impl NodeProtocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Gacl => "GACL",
            Self::Hysteria2 => "HY2",
        }
    }

    pub fn scheme(self) -> &'static str {
        match self {
            Self::Gacl => SHARE_SCHEME,
            Self::Hysteria2 => HY2_SCHEME,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeProfile {
    pub id: String,
    pub name: String,
    pub server: String,
    pub sni: String,
    pub token: String,
    #[serde(default)]
    pub protocol: NodeProtocol,
    #[serde(default = "default_true")]
    pub game_dup: bool,
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub source: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionPayload {
    pub name: String,
    #[serde(default)]
    pub expire_at: Option<String>,
    #[serde(default)]
    pub traffic_used_gb: Option<f64>,
    #[serde(default)]
    pub traffic_limit_gb: Option<f64>,
    pub nodes: Vec<NodeProfile>,
}

impl NodeProfile {
    /// Encode node into a shareable `gacl://` or `hysteria2://` URI.
    pub fn to_share_uri(&self) -> String {
        let insecure = if self.insecure { "1" } else { "0" };
        let name_enc = percent_encode(&self.name);
        let sni_enc = percent_encode(&self.sni);
        let token_enc = percent_encode(&self.token);
        match self.protocol {
            NodeProtocol::Gacl => {
                let dup = if self.game_dup { "1" } else { "0" };
                format!(
                    "{SHARE_SCHEME}{token_enc}@{}?sni={sni_enc}&dup={dup}&insecure={insecure}#{name_enc}",
                    self.server.trim()
                )
            }
            NodeProtocol::Hysteria2 => {
                format!(
                    "{HY2_SCHEME}{token_enc}@{}/?sni={sni_enc}&insecure={insecure}#{name_enc}",
                    self.server.trim()
                )
            }
        }
    }

    /// Parse a `gacl://`, `hysteria2://`, or `hy2://` share URI into a `NodeProfile`.
    pub fn from_share_uri(raw: &str) -> Result<Self> {
        let s = raw.trim();
        let (protocol, rest) = if let Some(r) = strip_scheme_ci(s, SHARE_SCHEME) {
            (NodeProtocol::Gacl, r)
        } else if let Some(r) = strip_scheme_ci(s, HY2_SCHEME) {
            (NodeProtocol::Hysteria2, r)
        } else if let Some(r) = strip_scheme_ci(s, HY2_SHORT_SCHEME) {
            (NodeProtocol::Hysteria2, r)
        } else {
            bail!("链接必须以 gacl://、hysteria2:// 或 hy2:// 开头");
        };

        let (before_hash, fragment) = match rest.split_once('#') {
            Some((a, b)) => (a, Some(b)),
            None => (rest, None),
        };

        let (authority_raw, query) = match before_hash.split_once('?') {
            Some((a, b)) => (a, Some(b)),
            None => (before_hash, None),
        };
        // Standard Hysteria2 URIs often include a trailing '/' before '?':
        // `hysteria2://pass@host:443/?sni=example.com&insecure=0#Name`
        let authority = authority_raw.trim_end_matches('/');

        let (token_raw, server_raw) = authority
            .split_once('@')
            .context("链接格式错误：缺少 token@host:port")?;

        let token = percent_decode(token_raw)?;
        let mut server = server_raw.trim().to_string();
        if token.is_empty() || server.is_empty() {
            bail!("链接中的 token 或服务器地址无效");
        }
        // Default Hysteria2 port to 443 when omitted (e.g. `hysteria2://pw@example.com?sni=...`)
        let has_port = if server.starts_with('[') {
            server.contains("]:")
        } else {
            server.contains(':')
        };
        if !has_port {
            if protocol == NodeProtocol::Hysteria2 {
                server.push_str(":443");
            } else {
                bail!("链接中的服务器地址缺少端口号 (host:port)");
            }
        }

        let mut sni = server
            .rsplit_once(':')
            .map(|(h, _)| h.trim_matches(['[', ']']).to_string())
            .unwrap_or_else(|| "localhost".to_string());
        let mut game_dup = protocol == NodeProtocol::Gacl;
        let mut insecure = false;

        if let Some(q) = query {
            for pair in q.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    match k.to_ascii_lowercase().as_str() {
                        "sni" => {
                            let decoded = percent_decode(v)?;
                            if !decoded.is_empty() {
                                sni = decoded;
                            }
                        }
                        "dup" | "game_dup" => {
                            if protocol == NodeProtocol::Gacl {
                                game_dup = matches!(v, "1" | "true" | "yes");
                            }
                        }
                        "insecure" => {
                            insecure = matches!(v, "1" | "true" | "yes");
                        }
                        "obfs" if protocol == NodeProtocol::Hysteria2 => {
                            let decoded = percent_decode(v)?;
                            if !decoded.is_empty() && !decoded.eq_ignore_ascii_case("none") {
                                bail!("暂不支持带 obfs={decoded} 混淆参数的 Hysteria2 链接");
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        let name = match fragment {
            Some(f) if !f.is_empty() => percent_decode(f)?,
            _ => format!("节点 {server}"),
        };

        let proto_tag = protocol.label();
        let id = format!(
            "node-{:08x}",
            fnv1a_32(format!("{proto_tag}|{server}|{sni}|{token}").as_bytes())
        );

        Ok(Self {
            id,
            name,
            server,
            sni,
            token,
            protocol,
            game_dup,
            insecure,
            source: None,
        })
    }
}

fn strip_scheme_ci<'a>(s: &'a str, scheme: &str) -> Option<&'a str> {
    let prefix = s.get(..scheme.len())?;
    if prefix.eq_ignore_ascii_case(scheme) {
        Some(&s[scheme.len()..])
    } else {
        None
    }
}

fn is_supported_share_line(line: &str) -> bool {
    strip_scheme_ci(line, SHARE_SCHEME).is_some()
        || strip_scheme_ci(line, HY2_SCHEME).is_some()
        || strip_scheme_ci(line, HY2_SHORT_SCHEME).is_some()
}

/// Parse one or more `gacl://` / `hysteria2://` / `hy2://` links (newline-separated)
/// OR a JSON `SubscriptionPayload` / `Vec<NodeProfile>`.
pub fn parse_import_text(text: &str) -> Result<Vec<NodeProfile>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("导入内容为空");
    }

    if trimmed.starts_with('{') {
        let sub: SubscriptionPayload =
            serde_json::from_str(trimmed).context("解析订阅 JSON 失败")?;
        return Ok(sub.nodes);
    }
    if trimmed.starts_with('[') {
        let nodes: Vec<NodeProfile> =
            serde_json::from_str(trimmed).context("解析节点数组 JSON 失败")?;
        return Ok(nodes);
    }

    let mut nodes = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if is_supported_share_line(line) {
            nodes.push(NodeProfile::from_share_uri(line)?);
        }
    }
    if nodes.is_empty() {
        bail!("未找到有效的 gacl:// / hysteria2:// 链接或 JSON 配置");
    }
    Ok(nodes)
}

fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])?;
            let val = u8::from_str_radix(hex, 16).context("invalid percent-encoding")?;
            out.push(val);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).context("decoded string is not valid utf-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_uri_roundtrip() {
        let node = NodeProfile {
            id: "us-1".into(),
            name: "美西-游戏专线".into(),
            server: "203.0.113.10:8443".into(),
            sni: "node.example.com".into(),
            token: "example-test-token-12345".into(),
            protocol: NodeProtocol::Gacl,
            game_dup: true,
            insecure: false,
            source: None,
        };
        let uri = node.to_share_uri();
        assert!(uri.starts_with("gacl://"));
        let parsed = NodeProfile::from_share_uri(&uri).unwrap();
        assert_eq!(parsed.protocol, NodeProtocol::Gacl);
        assert_eq!(parsed.name, node.name);
        assert_eq!(parsed.server, node.server);
        assert_eq!(parsed.sni, node.sni);
        assert_eq!(parsed.token, node.token);
        assert_eq!(parsed.game_dup, node.game_dup);
        assert_eq!(parsed.insecure, node.insecure);
    }

    #[test]
    fn hy2_uri_roundtrip_and_mixed_import() {
        let hy2_uri = "hysteria2://pass-123@www.goldpeak.top:443/?sni=www.goldpeak.top&insecure=0#%E9%A6%99%E6%B8%AFHy2";
        let parsed = NodeProfile::from_share_uri(hy2_uri).unwrap();
        assert_eq!(parsed.protocol, NodeProtocol::Hysteria2);
        assert_eq!(parsed.server, "www.goldpeak.top:443");
        assert_eq!(parsed.sni, "www.goldpeak.top");
        assert_eq!(parsed.token, "pass-123");
        assert_eq!(parsed.name, "香港Hy2");
        assert!(!parsed.game_dup);
        assert!(!parsed.insecure);

        let exported = parsed.to_share_uri();
        assert!(exported.starts_with("hysteria2://"));
        let reparsed = NodeProfile::from_share_uri(&exported).unwrap();
        assert_eq!(reparsed, parsed);

        // Short hy2:// scheme with default port 443
        let short = NodeProfile::from_share_uri("hy2://pw@example.com?insecure=1#Short").unwrap();
        assert_eq!(short.protocol, NodeProtocol::Hysteria2);
        assert_eq!(short.server, "example.com:443");
        assert_eq!(short.sni, "example.com");
        assert!(short.insecure);

        // Mixed import text
        let mixed = format!("{hy2_uri}\ngacl://tok@1.2.3.4:443?sni=a.com&dup=1#GACL\n");
        let list = parse_import_text(&mixed).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].protocol, NodeProtocol::Hysteria2);
        assert_eq!(list[1].protocol, NodeProtocol::Gacl);
        assert_ne!(list[0].id, list[1].id);

        // Unsupported obfs parameter returns a clear error
        assert!(
            NodeProfile::from_share_uri("hysteria2://pw@example.com:443/?obfs=salamander&obfs-password=abc")
                .is_err()
        );
    }
}
