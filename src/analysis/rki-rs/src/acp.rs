//! ACP (Agent Communication Protocol) SSE server.
//!
//! Exposes the session wire event stream over HTTP for IDE integrations.
//!
//! **Structured user turns:** `POST /turn` with a UTF-8 body (same formats as
//! [`crate::turn_input::parse_cli_turn_line`]) queues one turn when the binary was started with
//! `--acp` and a worker is running. `GET /turn` returns a small JSON hint. Requires
//! `Content-Length` on POST bodies (max 256 KiB).
//!
//! Optional shared secret: **`RKI_ACP_TOKEN`**. When set, **`POST /turn`** and **`GET /events`**
//! require `Authorization: Bearer <token>`. **`GET /health`** and **`GET /turn`** (JSON hint)
//! stay unauthenticated for local probes.

use crate::wire::RootWireHub;
use anyhow::Context;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

fn sse_idle_timeout() -> Duration {
    if cfg!(test) {
        Duration::from_millis(150)
    } else {
        Duration::from_secs(25)
    }
}

const MAX_REQUEST: usize = 256 * 1024;

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn header_value<'a>(line: &'a str, header_name: &str) -> Option<&'a str> {
    let line = line.trim_start();
    let prefix = format!("{}:", header_name);
    if line.len() >= prefix.len() && line[..prefix.len()].eq_ignore_ascii_case(&prefix) {
        Some(line[prefix.len()..].trim_start())
    } else {
        None
    }
}

fn parse_authorization_bearer(headers: &str) -> Option<String> {
    for raw in headers.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Some(val) = header_value(line, "authorization") else {
            continue;
        };
        let mut it = val.split_whitespace();
        match (it.next(), it.next()) {
            (Some(scheme), Some(tok)) if scheme.eq_ignore_ascii_case("bearer") => {
                return Some(tok.to_string());
            }
            _ => {}
        }
    }
    None
}

fn acp_authorized(headers: &str, token: &Option<String>) -> bool {
    match token {
        None => true,
        Some(expected) => parse_authorization_bearer(headers).as_deref() == Some(expected.as_str()),
    }
}

fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let l = line.trim();
        let lower = l.to_ascii_lowercase();
        if lower.starts_with("content-length:") {
            return l.split(':').nth(1)?.trim().parse().ok();
        }
    }
    None
}

async fn read_until_double_crlf(stream: &mut TcpStream, buf: &mut Vec<u8>) -> anyhow::Result<usize> {
    let mut tmp = [0u8; 4096];
    while buf.len() < MAX_REQUEST {
        if let Some(pos) = find_double_crlf(buf) {
            return Ok(pos);
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("connection closed before end of headers");
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    anyhow::bail!("request headers exceed limit")
}

async fn ensure_body(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    header_end: usize,
    content_len: usize,
) -> anyhow::Result<()> {
    let need = header_end + 4 + content_len;
    let mut tmp = [0u8; 4096];
    while buf.len() < need && buf.len() < MAX_REQUEST {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("connection closed before full body");
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    if buf.len() < need {
        anyhow::bail!("short body: have {} need {}", buf.len(), need);
    }
    Ok(())
}

async fn write_http_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

/// ACP (Agent Communication Protocol) server.
/// Exposes the session wire event stream over HTTP as Server-Sent Events.
#[allow(dead_code)]
pub struct AcpServer {
    hub: RootWireHub,
    port: u16,
    /// When set, `POST /turn` accepts a body and forwards it here (UTF-8 text / JSON line).
    turn_tx: Option<mpsc::Sender<String>>,
    /// When set, `POST /turn` and `GET /events` require `Authorization: Bearer <token>`.
    auth_token: Option<String>,
}

#[allow(dead_code)]
impl AcpServer {
    pub fn new(
        hub: RootWireHub,
        port: u16,
        turn_tx: Option<mpsc::Sender<String>>,
        auth_token: Option<String>,
    ) -> Self {
        Self {
            hub,
            port,
            turn_tx,
            auth_token,
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", self.port)).await?;
        tracing::info!("ACP server listening on http://127.0.0.1:{}", self.port);

        let hub = self.hub.clone();
        let turn_tx = self.turn_tx.clone();
        let auth_token = self.auth_token.clone();

        loop {
            let (stream, addr) = listener.accept().await?;
            let hub = hub.clone();
            let turn_tx = turn_tx.clone();
            let auth_token = auth_token.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, hub, turn_tx, auth_token).await {
                    tracing::debug!("ACP connection from {} error: {}", addr, e);
                }
            });
        }
    }
}

async fn write_chunk_frame(stream: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    let header = format!("{:x}\r\n", body.len());
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.write_all(b"\r\n").await?;
    Ok(())
}

async fn finalize_chunked_stream(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await
}

async fn handle_connection(
    mut stream: TcpStream,
    hub: RootWireHub,
    turn_tx: Option<mpsc::Sender<String>>,
    auth_token: Option<String>,
) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    let header_end = read_until_double_crlf(&mut stream, &mut buf).await?;
    let head = std::str::from_utf8(&buf[..header_end]).context("request headers not utf-8")?;
    let first = head.lines().next().unwrap_or("").trim();
    let parts: Vec<&str> = first.split_whitespace().collect();
    let method = parts.first().copied().unwrap_or("");
    let path = parts.get(1).copied().unwrap_or("");
    let path_base = path.split('?').next().unwrap_or(path);

    if method == "GET" && path.starts_with("/health") {
        let body = br#"{"status":"ok"}"#;
        write_http_response(&mut stream, "200 OK", "application/json", body).await?;
        return Ok(());
    }

    if method == "GET" && path_base == "/turn" {
        let help = if turn_tx.is_some() {
            serde_json::json!({
                "post": "/turn",
                "content_type": "text/plain; charset=utf-8 or application/json",
                "body": "Formats accepted by turn_input::parse_cli_turn_line (plain text or JSON on one line)",
                "content_length_required": true,
            })
            .to_string()
        } else {
            serde_json::json!({"post": "/turn", "enabled": false}).to_string()
        };
        write_http_response(&mut stream, "200 OK", "application/json", help.as_bytes()).await?;
        return Ok(());
    }

    if method == "POST" && path_base == "/turn" {
        if !acp_authorized(head, &auth_token) {
            write_http_response(
                &mut stream,
                "401 Unauthorized",
                "application/json",
                br#"{"error":"unauthorized"}"#,
            )
            .await?;
            return Ok(());
        }
        let cl = parse_content_length(head).context("POST /turn requires Content-Length header")?;
        if cl > MAX_REQUEST.saturating_sub(header_end + 4) {
            write_http_response(
                &mut stream,
                "413 Payload Too Large",
                "application/json",
                br#"{"error":"body too large"}"#,
            )
            .await?;
            return Ok(());
        }
        ensure_body(&mut stream, &mut buf, header_end, cl).await?;
        let body_bytes = &buf[header_end + 4..header_end + 4 + cl];
        let body_str = String::from_utf8_lossy(body_bytes).trim().to_string();

        match &turn_tx {
            Some(tx) => match tx.try_send(body_str) {
                Ok(()) => {
                    write_http_response(
                        &mut stream,
                        "202 Accepted",
                        "application/json",
                        br#"{"accepted":true}"#,
                    )
                    .await?;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    write_http_response(
                        &mut stream,
                        "503 Service Unavailable",
                        "application/json",
                        br#"{"error":"turn queue full"}"#,
                    )
                    .await?;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    write_http_response(
                        &mut stream,
                        "503 Service Unavailable",
                        "application/json",
                        br#"{"error":"turn queue closed"}"#,
                    )
                    .await?;
                }
            },
            None => {
                write_http_response(
                    &mut stream,
                    "404 Not Found",
                    "application/json",
                    br#"{"error":"turn ingestion disabled"}"#,
                )
                .await?;
            }
        }
        return Ok(());
    }

    if method == "GET" && path.starts_with("/events") {
        if !acp_authorized(head, &auth_token) {
            write_http_response(
                &mut stream,
                "401 Unauthorized",
                "application/json",
                br#"{"error":"unauthorized"}"#,
            )
            .await?;
            return Ok(());
        }
        const HDR: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\
Transfer-Encoding: chunked\r\n\
\r\n";
        stream.write_all(HDR).await?;

        let mut rx = hub.subscribe();
        loop {
            match tokio::time::timeout(sse_idle_timeout(), rx.recv()).await {
                Ok(Ok(envelope)) => {
                    if let Ok(json) = serde_json::to_string(&envelope) {
                        let sse = format!("data: {}\n\n", json);
                        if write_chunk_frame(&mut stream, sse.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                    }
                }
                Ok(Err(RecvError::Lagged(_))) => continue,
                Ok(Err(RecvError::Closed)) => {
                    let _ = finalize_chunked_stream(&mut stream).await;
                    break;
                }
                Err(_) => {
                    if write_chunk_frame(&mut stream, b": keepalive\n\n").await.is_err() {
                        break;
                    }
                    let _ = stream.flush().await;
                }
            }
        }
        return Ok(());
    }

    stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::WireEvent;

    #[tokio::test]
    async fn test_acp_server_creation() {
        let hub = RootWireHub::new();
        let server = AcpServer::new(hub, 0, None, None);
        assert_eq!(server.port, 0);
    }

    #[tokio::test]
    async fn test_acp_server_health_response_format() {
        let hub = RootWireHub::new();
        let server = AcpServer::new(hub, 0, None, None);
        assert_eq!(server.port, 0);
    }

    #[tokio::test]
    async fn test_acp_server_non_zero_port() {
        let hub = RootWireHub::new();
        let server = AcpServer::new(hub, 8080, None, None);
        assert_eq!(server.port, 8080);
    }

    #[tokio::test]
    async fn test_acp_server_clone_hub() {
        let hub = RootWireHub::new();
        let server = AcpServer::new(hub.clone(), 3000, None, None);
        assert_eq!(server.port, 3000);
    }

    #[tokio::test]
    async fn test_acp_health_via_handle_connection() {
        let hub = RootWireHub::new();
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(e) => panic!("bind failed: {e}"),
        };
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, hub, None, None).await.unwrap();
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 512];
        let n = client.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("200 OK"));
        assert!(text.contains("{\"status\":\"ok\"}"));
        drop(client);
        let _ = server.await;
    }

    #[tokio::test]
    async fn test_acp_get_turn_help() {
        let hub = RootWireHub::new();
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(e) => panic!("bind failed: {e}"),
        };
        let addr = listener.local_addr().unwrap();
        let (tx, _rx) = mpsc::channel::<String>(1);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, hub, Some(tx), None).await.unwrap();
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /turn HTTP/1.1\r\nHost: t\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 1024];
        let n = client.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("200 OK"), "{text}");
        assert!(text.contains("parse_cli_turn_line"), "{text}");
        drop(client);
        let _ = server.await;
    }

    #[tokio::test]
    async fn test_acp_post_turn_queues_body() {
        let hub = RootWireHub::new();
        let (tx, mut rx) = mpsc::channel::<String>(4);
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(e) => panic!("bind failed: {e}"),
        };
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, hub, Some(tx), None).await.unwrap();
        });
        let body = r#"{"text":"hello-acp"}"#;
        let req = format!(
            "POST /turn HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 512];
        let n = client.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("202"), "{text}");
        let got = rx.recv().await.expect("queued turn");
        assert!(got.contains("hello-acp"), "{got}");
        drop(client);
        let _ = server.await;
    }

    #[tokio::test]
    async fn test_acp_chunked_sse_streams_wire_event() {
        let hub = RootWireHub::new();
        let hub_send = hub.clone();
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(e) => panic!("bind failed: {e}"),
        };
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = handle_connection(stream, hub, None, None).await;
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /events HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .unwrap();

        // Wait for the handler to install its broadcast receiver before emitting.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        hub_send.broadcast(WireEvent::TurnEnd);

        let mut accumulated = String::new();
        let mut buf = vec![0u8; 8192];
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while accumulated.len() < 64 * 1024 && tokio::time::Instant::now() < deadline {
            let n = match tokio::time::timeout(std::time::Duration::from_millis(400), client.read(&mut buf)).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => panic!("read: {e}"),
                Err(_) => continue,
            };
            if n == 0 {
                break;
            }
            accumulated.push_str(&String::from_utf8_lossy(&buf[..n]));
            if accumulated.contains("data: ") {
                break;
            }
        }
        let text = &accumulated;
        assert!(text.contains("200 OK"), "expected 200, got {:?}", text);
        assert!(text.contains("Transfer-Encoding: chunked"), "{}", text);
        assert!(text.contains("text/event-stream"), "{}", text);
        assert!(text.contains("data: "), "{}", text);
        assert!(text.contains("turn_end") || text.contains("TurnEnd"), "{}", text);

        drop(client);
        let _ = server.await;
    }

    #[test]
    fn test_parse_authorization_bearer() {
        assert_eq!(
            parse_authorization_bearer("POST /t HTTP/1.1\r\nAuthorization: Bearer my-secret\r\n"),
            Some("my-secret".to_string())
        );
        assert_eq!(
            parse_authorization_bearer("authorization: BeArEr tok\r\n"),
            Some("tok".to_string())
        );
        assert_eq!(parse_authorization_bearer("GET /x HTTP/1.1\r\nHost: a\r\n"), None);
    }

    #[tokio::test]
    async fn test_acp_post_turn_401_when_auth_required() {
        let hub = RootWireHub::new();
        let (tx, _rx) = mpsc::channel::<String>(2);
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(e) => panic!("bind failed: {e}"),
        };
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, hub, Some(tx), Some("sekrit".into()))
                .await
                .unwrap();
        });
        let body = r#"{"text":"x"}"#;
        let req = format!(
            "POST /turn HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 512];
        let n = client.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("401"), "{text}");
        drop(client);
        let _ = server.await;
    }

    #[tokio::test]
    async fn test_acp_post_turn_accepts_bearer_when_auth_required() {
        let hub = RootWireHub::new();
        let (tx, mut rx) = mpsc::channel::<String>(2);
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(e) => panic!("bind failed: {e}"),
        };
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, hub, Some(tx), Some("good".into()))
                .await
                .unwrap();
        });
        let body = r#"{"text":"authed"}"#;
        let req = format!(
            "POST /turn HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer good\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 512];
        let n = client.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("202"), "{text}");
        let got = rx.recv().await.expect("queued");
        assert!(got.contains("authed"), "{got}");
        drop(client);
        let _ = server.await;
    }

    #[tokio::test]
    async fn test_acp_events_401_without_bearer_when_auth_required() {
        let hub = RootWireHub::new();
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(e) => panic!("bind failed: {e}"),
        };
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, hub, None, Some("tok".into())).await.unwrap();
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /events HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 512];
        let n = client.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("401"), "{text}");
        drop(client);
        let _ = server.await;
    }
}
