use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const SHARE_SCHEME: &str = "gacl://";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeProfile {
    pub id: String,
    pub name: String,
    pub server: String,
    pub sni: String,
    pub token: String,
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
    /// Encode node into a shareable `gacl://` URI:
    /// `gacl://<token>@<server>?sni=<sni>&dup=1&insecure=0#<encoded_name>`
    pub fn to_share_uri(&self) -> String {
        let dup = if self.game_dup { "1" } else { "0" };
        let insecure = if self.insecure { "1" } else { "0" };
        let name_enc = percent_encode(&self.name);
        let sni_enc = percent_encode(&self.sni);
        let token_enc = percent_encode(&self.token);
        format!(
            "{SHARE_SCHEME}{token_enc}@{}?sni={sni_enc}&dup={dup}&insecure={insecure}#{name_enc}",
            self.server.trim()
        )
    }

    /// Parse a `gacl://` share URI into a `NodeProfile`.
    pub fn from_share_uri(raw: &str) -> Result<Self> {
        let s = raw.trim();
        let rest = s
            .strip_prefix(SHARE_SCHEME)
            .context("链接必须以 gacl:// 开头")?;

        let (before_hash, fragment) = match rest.split_once('#') {
            Some((a, b)) => (a, Some(b)),
            None => (rest, None),
        };

        let (authority, query) = match before_hash.split_once('?') {
            Some((a, b)) => (a, Some(b)),
            None => (before_hash, None),
        };

        let (token_raw, server_raw) = authority
            .split_once('@')
            .context("链接格式错误：缺少 token@host:port")?;

        let token = percent_decode(token_raw)?;
        let server = server_raw.trim().to_string();
        if token.is_empty() || server.is_empty() || !server.contains(':') {
            bail!("链接中的 token 或服务器地址(IP:端口)无效");
        }

        let mut sni = server
            .rsplit_once(':')
            .map(|(h, _)| h.trim_matches(['[', ']']).to_string())
            .unwrap_or_else(|| "localhost".to_string());
        let mut game_dup = true;
        let mut insecure = false;

        if let Some(q) = query {
            for pair in q.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    match k {
                        "sni" => {
                            let decoded = percent_decode(v)?;
                            if !decoded.is_empty() {
                                sni = decoded;
                            }
                        }
                        "dup" | "game_dup" => {
                            game_dup = matches!(v, "1" | "true" | "yes");
                        }
                        "insecure" => {
                            insecure = matches!(v, "1" | "true" | "yes");
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

        let id = format!(
            "node-{:08x}",
            fnv1a_32(format!("{server}|{sni}|{token}").as_bytes())
        );

        Ok(Self {
            id,
            name,
            server,
            sni,
            token,
            game_dup,
            insecure,
            source: None,
        })
    }
}

/// Parse one or more `gacl://` links (newline-separated) OR a JSON `SubscriptionPayload` / `Vec<NodeProfile>`.
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
        if line.starts_with(SHARE_SCHEME) {
            nodes.push(NodeProfile::from_share_uri(line)?);
        }
    }
    if nodes.is_empty() {
        bail!("未找到有效的 gacl:// 链接或 JSON 配置");
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
            game_dup: true,
            insecure: false,
            source: None,
        };
        let uri = node.to_share_uri();
        assert!(uri.starts_with("gacl://"));
        let parsed = NodeProfile::from_share_uri(&uri).unwrap();
        assert_eq!(parsed.name, node.name);
        assert_eq!(parsed.server, node.server);
        assert_eq!(parsed.sni, node.sni);
        assert_eq!(parsed.token, node.token);
        assert_eq!(parsed.game_dup, node.game_dup);
        assert_eq!(parsed.insecure, node.insecure);
    }
}
