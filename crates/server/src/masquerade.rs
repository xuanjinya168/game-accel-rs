use anyhow::Result;
use protocol::hy2::{header_val, read_h3_headers, write_h3_response_with_body};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_404_HTML: &[u8] = b"<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n<center><h1>404 Not Found</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n";
const MAX_MASQUERADE_FILE_BYTES: u64 = 4 * 1024 * 1024;

pub(crate) fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("xml") => "application/xml; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

pub(crate) fn resolve_static_file(root: &Path, raw_path: &str) -> Option<PathBuf> {
    let root_canon = std::fs::canonicalize(root).ok()?;
    let path_only = raw_path.split('?').next().unwrap_or("/");
    let rel = path_only.trim_start_matches('/');
    let mut candidate = root_canon.clone();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains('\\') || seg.contains('\0') {
            return None;
        }
        candidate.push(seg);
    }
    if candidate.is_dir() {
        candidate.push("index.html");
    }
    let canon = std::fs::canonicalize(&candidate).ok()?;
    if !canon.starts_with(&root_canon) || !canon.is_file() {
        return None;
    }
    let meta = std::fs::metadata(&canon).ok()?;
    if meta.len() > MAX_MASQUERADE_FILE_BYTES {
        return None;
    }
    Some(canon)
}

pub(crate) async fn serve_masquerade_h3(
    send: &mut quinn::SendStream,
    method: &str,
    path: &str,
    masquerade_dir: Option<&Path>,
) -> Result<()> {
    let is_head = method.eq_ignore_ascii_case("HEAD");
    let allow_file = method.eq_ignore_ascii_case("GET") || is_head;

    if allow_file
        && let Some(root) = masquerade_dir
        && let Some(file_path) = resolve_static_file(root, path)
        && let Ok(bytes) = tokio::fs::read(&file_path).await
    {
        let ct = content_type_for(&file_path);
        let len_str = bytes.len().to_string();
        let body = if is_head { &[][..] } else { &bytes[..] };
        return write_h3_response_with_body(
            send,
            "200",
            &[
                ("server", "nginx"),
                ("content-type", ct),
                ("content-length", &len_str),
                ("alt-svc", "h3=\":443\"; ma=2592000"),
            ],
            body,
        )
        .await;
    }

    let custom_404 = masquerade_dir
        .and_then(|root| resolve_static_file(root, "/404.html"))
        .and_then(|p| std::fs::read(p).ok());
    let body_slice: &[u8] = custom_404.as_deref().unwrap_or(DEFAULT_404_HTML);
    let len_str = body_slice.len().to_string();
    let body = if is_head { &[][..] } else { body_slice };
    write_h3_response_with_body(
        send,
        "404",
        &[
            ("server", "nginx"),
            ("content-type", "text/html; charset=utf-8"),
            ("content-length", &len_str),
            ("alt-svc", "h3=\":443\"; ma=2592000"),
        ],
        body,
    )
    .await
}

pub(crate) async fn serve_masquerade_followup(conn: &quinn::Connection, root: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    for _ in 0..32 {
        let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let Ok(Ok((mut send, mut recv))) = tokio::time::timeout(rem, conn.accept_bi()).await else {
            break;
        };
        let root_clone = root.to_path_buf();
        tokio::spawn(async move {
            if let Ok(headers) = read_h3_headers(&mut recv).await {
                let method = header_val(&headers, ":method").unwrap_or("GET");
                let path = header_val(&headers, ":path").unwrap_or("/");
                let _ = serve_masquerade_h3(&mut send, method, path, Some(root_clone.as_path())).await;
            }
            let _ = send.finish();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masquerade_static_file_resolves_index_and_blocks_traversal() {
        let dir = std::env::temp_dir().join(format!(
            "gacl-masq-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("posts")).unwrap();
        std::fs::write(dir.join("index.html"), b"<h1>GoldPeak</h1>").unwrap();
        std::fs::write(dir.join("posts/index.html"), b"<h1>Posts</h1>").unwrap();
        std::fs::write(dir.join("style.css"), b"body{}").unwrap();

        let root_idx = resolve_static_file(&dir, "/").unwrap();
        assert!(root_idx.ends_with("index.html"));
        let posts_idx = resolve_static_file(&dir, "/posts/?v=1").unwrap();
        assert!(posts_idx.ends_with("posts\\index.html") || posts_idx.ends_with("posts/index.html"));
        assert_eq!(
            content_type_for(&resolve_static_file(&dir, "/style.css").unwrap()),
            "text/css; charset=utf-8"
        );
        assert!(resolve_static_file(&dir, "/../etc/passwd").is_none());
        assert!(resolve_static_file(&dir, "/posts/../../secret").is_none());
        assert!(resolve_static_file(&dir, "/nonexistent").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
