use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

#[derive(Debug, Clone)]
pub struct MCPToolRef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct MCPResult {
    pub content: Vec<MCPContent>,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub enum MCPContent {
    Text(String),
    Image { data: String, mime: String },
    Audio { data: String, mime: String },
    /// MCP `resource` content block (URI reference or embedded text).
    Resource {
        uri: String,
        mime: Option<String>,
        text: Option<String>,
    },
}

/// Server-initiated notification (no `id` field).
#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// Server-to-client request (has `id` and `method`).
#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct JsonRpcServerRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// Parsed MCP notification from server.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum MCPNotification {
    /// Progress notification for a long-running operation.
    Progress {
        progress_token: String,
        progress: f64,
        total: Option<f64>,
    },
    /// A subscribed resource has been updated.
    ResourceUpdated { uri: String },
    /// Server is requesting LLM sampling.
    SamplingRequest {
        id: Value,
        messages: Vec<Value>,
    },
    /// Unknown notification method.
    Unknown { method: String, params: Option<Value> },
}

impl MCPNotification {
    fn from_jsonrpc(n: JsonRpcNotification) -> Self {
        match n.method.as_str() {
            "notifications/progress" => {
                let token = n
                    .params
                    .as_ref()
                    .and_then(|p| p["progressToken"].as_str())
                    .unwrap_or("")
                    .to_string();
                let progress = n
                    .params
                    .as_ref()
                    .and_then(|p| p["progress"].as_f64())
                    .unwrap_or(0.0);
                let total = n.params.as_ref().and_then(|p| p["total"].as_f64());
                MCPNotification::Progress {
                    progress_token: token,
                    progress,
                    total,
                }
            }
            "notifications/resources/updated" => {
                let uri = n
                    .params
                    .as_ref()
                    .and_then(|p| p["uri"].as_str())
                    .unwrap_or("")
                    .to_string();
                MCPNotification::ResourceUpdated { uri }
            }
            _ => MCPNotification::Unknown {
                method: n.method,
                params: n.params,
            },
        }
    }

    fn from_server_request(req: JsonRpcServerRequest) -> Option<Self> {
        match req.method.as_str() {
            "sampling/createMessage" => {
                let messages = req
                    .params
                    .as_ref()
                    .and_then(|p| p["messages"].as_array())
                    .cloned()
                    .unwrap_or_default();
                Some(MCPNotification::SamplingRequest {
                    id: req.id,
                    messages,
                })
            }
            _ => None,
        }
    }
}

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponseSuccess {
    jsonrpc: String,
    id: Value,
    result: Value,
}

#[derive(Deserialize, Debug, Clone)]
struct JsonRpcResponse {
    id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

struct Connection {
    stdin: tokio::io::BufWriter<tokio::process::ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    request_id: AtomicU64,
    _reader: tokio::task::JoinHandle<()>,
    notification_tx: broadcast::Sender<MCPNotification>,
}

pub struct MCPClient {
    name: String,
    command: Vec<String>,
    conn: Arc<Mutex<Option<Connection>>>,
}

impl MCPClient {
    pub fn new(name: String, command: Vec<String>) -> Self {
        Self {
            name,
            command,
            conn: Arc::new(Mutex::new(None)),
        }
    }

    async fn ensure_connected(&self) -> anyhow::Result<broadcast::Receiver<MCPNotification>> {
        let mut conn = self.conn.lock().await;
        if let Some(ref c) = *conn {
            return Ok(c.notification_tx.subscribe());
        }
        if self.command.is_empty() {
            anyhow::bail!("Empty MCP command");
        }
        let mut cmd = Command::new(&self.command[0]);
        for arg in &self.command[1..] {
            cmd.arg(arg);
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = pending.clone();
        let name = self.name.clone();
        let (notification_tx, _) = broadcast::channel(64);
        let notif_tx_reader = notification_tx.clone();

        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        // Try response first (has numeric id)
                        if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&line) {
                            let mut map = pending_reader.lock().await;
                            if let Some(tx) = map.remove(&resp.id) {
                                let _ = tx.send(resp);
                            }
                            continue;
                        }
                        // Try server request (has id + method)
                        if let Ok(req) = serde_json::from_str::<JsonRpcServerRequest>(&line) {
                            if let Some(notif) = MCPNotification::from_server_request(req.clone())
                            {
                                let _ = notif_tx_reader.send(notif);
                            }
                            continue;
                        }
                        // Try notification (no id, has method)
                        if let Ok(notif) = serde_json::from_str::<JsonRpcNotification>(&line) {
                            let parsed = MCPNotification::from_jsonrpc(notif);
                            let _ = notif_tx_reader.send(parsed);
                            continue;
                        }
                        tracing::warn!("MCP {} unparseable line: {}", name, line);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("MCP {} stdout read error: {}", name, e);
                        break;
                    }
                }
            }
            // Drop all pending requests on disconnect
            let mut map = pending_reader.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(JsonRpcResponse {
                    id: 0,
                    result: None,
                    error: Some(serde_json::json!({"message": "MCP server disconnected"})),
                });
            }
        });

        *conn = Some(Connection {
            stdin: tokio::io::BufWriter::new(stdin),
            pending,
            request_id: AtomicU64::new(1),
            _reader: reader,
            notification_tx,
        });
        Ok(conn.as_ref().unwrap().notification_tx.subscribe())
    }

    async fn call_method(&self, method: &str, params: Option<Value>) -> anyhow::Result<Value> {
        self.ensure_connected().await?;
        let mut conn = self.conn.lock().await;
        let conn = conn.as_mut().unwrap();

        let id = conn.request_id.fetch_add(1, Ordering::SeqCst);
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };
        let req_line = serde_json::to_string(&req)? + "\n";
        conn.stdin.write_all(req_line.as_bytes()).await?;
        conn.stdin.flush().await?;

        let (tx, rx) = oneshot::channel();
        conn.pending.lock().await.insert(id, tx);

        let resp = tokio::time::timeout(std::time::Duration::from_secs(60), rx).await??;
        if let Some(err) = resp.error {
            anyhow::bail!("MCP error: {}", err);
        }
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }

    /// Send a JSON-RPC response back to the server for server-to-client requests.
    async fn send_response(&self, id: Value, result: Value) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().await;
        let conn = conn.as_mut().ok_or_else(|| anyhow::anyhow!("Not connected"))?;
        let resp = JsonRpcResponseSuccess {
            jsonrpc: "2.0".to_string(),
            id,
            result,
        };
        let line = serde_json::to_string(&resp)? + "\n";
        conn.stdin.write_all(line.as_bytes()).await?;
        conn.stdin.flush().await?;
        Ok(())
    }

    fn tool_record_from_json(t: &Value) -> MCPToolRef {
        let input_schema = t
            .get("inputSchema")
            .cloned()
            .or_else(|| t.get("input_schema").cloned())
            .unwrap_or_else(|| serde_json::json!({"type": "object"}));
        MCPToolRef {
            name: t["name"].as_str().unwrap_or("").to_string(),
            description: t["description"].as_str().unwrap_or("").to_string(),
            input_schema,
        }
    }

    pub async fn list_tools(&self) -> anyhow::Result<Vec<MCPToolRef>> {
        let response = self.call_method("tools/list", None).await?;
        let tools = response["tools"]
            .as_array()
            .map(|arr| arr.iter().map(Self::tool_record_from_json).collect())
            .unwrap_or_default();
        Ok(tools)
    }

    #[allow(dead_code)]
    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<MCPResult> {
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments,
        });
        let response = self.call_method("tools/call", Some(params)).await?;
        let content = response["content"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| match c["type"].as_str() {
                        Some("text") => {
                            Some(MCPContent::Text(c["text"].as_str()?.to_string()))
                        }
                        Some("image") => Some(MCPContent::Image {
                            data: c["data"].as_str()?.to_string(),
                            mime: c["mimeType"]
                                .as_str()
                                .or_else(|| c["mime_type"].as_str())
                                .unwrap_or("image/png")
                                .to_string(),
                        }),
                        Some("audio") => Some(MCPContent::Audio {
                            data: c["data"].as_str().unwrap_or_default().to_string(),
                            mime: c["mimeType"]
                                .as_str()
                                .or_else(|| c["mime_type"].as_str())
                                .unwrap_or("audio/wav")
                                .to_string(),
                        }),
                        Some("resource") => {
                            let res = c.get("resource").cloned().unwrap_or_else(|| c.clone());
                            let uri = res["uri"].as_str().unwrap_or("").to_string();
                            let mime = res["mimeType"]
                                .as_str()
                                .or_else(|| res["mime_type"].as_str())
                                .map(String::from);
                            let text = res["text"].as_str().map(String::from);
                            Some(MCPContent::Resource { uri, mime, text })
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let is_error = response["isError"].as_bool().unwrap_or(false);
        Ok(MCPResult { content, is_error })
    }

    /// Subscribe to resource updates. Returns a receiver that yields updated URIs.
    #[allow(dead_code)]
    pub async fn subscribe_resource(
        &self,
        uri: &str,
    ) -> anyhow::Result<mpsc::Receiver<String>> {
        let params = serde_json::json!({ "uri": uri });
        self.call_method("resources/subscribe", Some(params)).await?;

        // Create a filtered receiver for this URI
        let mut notif_rx = self.subscribe_notifications().await?;
        let (tx, rx) = mpsc::channel(16);
        let uri = uri.to_string();
        tokio::spawn(async move {
            loop {
                match notif_rx.recv().await {
                    Ok(MCPNotification::ResourceUpdated { uri: updated_uri }) => {
                        if updated_uri == uri
                            && tx.send(updated_uri).await.is_err() {
                                break;
                            }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Ok(rx)
    }

    /// Read a resource by URI.
    pub async fn read_resource(&self, uri: &str) -> anyhow::Result<Value> {
        let params = serde_json::json!({ "uri": uri });
        self.call_method("resources/read", Some(params)).await
    }

    /// Subscribe to MCP server notifications (progress, resource updates, sampling, …).
    pub async fn subscribe_notifications(&self) -> anyhow::Result<broadcast::Receiver<MCPNotification>> {
        self.ensure_connected().await
    }

    /// Alias for code that only cares about sampling; receives the same notification stream.
    pub async fn sampling_requests(&self) -> anyhow::Result<broadcast::Receiver<MCPNotification>> {
        self.subscribe_notifications().await
    }

    /// Respond to a sampling request with the LLM-generated content.
    pub async fn respond_sampling(
        &self,
        id: Value,
        content: String,
    ) -> anyhow::Result<()> {
        let result = serde_json::json!({
            "model": "default",
            "role": "assistant",
            "content": {
                "type": "text",
                "text": content,
            },
        });
        self.send_response(id, result).await
    }
}

/// High-level MCP session with resource subscriptions and progress tracking.
pub struct MCPSession {
    client: Arc<MCPClient>,
}

#[allow(dead_code)]
impl MCPSession {
    pub fn new(client: Arc<MCPClient>) -> Self {
        Self { client }
    }

    pub async fn list_tools(&self) -> anyhow::Result<Vec<MCPToolRef>> {
        self.client.list_tools().await
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<MCPResult> {
        self.client.call_tool(name, arguments).await
    }

    pub async fn subscribe_resource(
        &self,
        uri: &str,
    ) -> anyhow::Result<mpsc::Receiver<String>> {
        self.client.subscribe_resource(uri).await
    }

    pub async fn read_resource(&self, uri: &str) -> anyhow::Result<Value> {
        self.client.read_resource(uri).await
    }

    /// Start a background task that listens for progress notifications
    /// and forwards them as StatusUpdate wire events on the RootWireHub.
    pub async fn start_progress_forwarder(
        &self,
        hub: crate::wire::RootWireHub,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let mut rx = self.client.subscribe_notifications().await?;
        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(MCPNotification::Progress {
                        progress_token,
                        progress,
                        total,
                    }) => {
                        hub.broadcast(crate::wire::WireEvent::StatusUpdate {
                            token_count: 0,
                            context_size: 0,
                            plan_mode: false,
                            mcp_status: format!(
                                "MCP progress {}: {}/{}",
                                progress_token,
                                progress,
                                total.map(|t| t.to_string()).unwrap_or_else(|| "?".to_string()),
                            ),
                        });
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Ok(handle)
    }

    /// Start a background task that monitors resource updates and broadcasts
    /// them as wire events. Returns the join handle.
    pub async fn start_resource_monitor(
        &self,
        hub: crate::wire::RootWireHub,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let mut rx = self.client.subscribe_notifications().await?;
        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(MCPNotification::ResourceUpdated { uri }) => {
                        hub.broadcast(crate::wire::WireEvent::TextPart {
                            text: format!("\n[MCP Resource updated: {}]\n", uri),
                        });
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Ok(handle)
    }

    /// Start a background task that routes MCP sampling requests to the LLM (§7.5).
    /// When the MCP server requests sampling (sending messages to the LLM),
    /// this handler forwards them to the provided ChatProvider and sends
    /// the generated response back to the MCP server.
    pub async fn start_sampling_handler(
        &self,
        llm: std::sync::Arc<dyn crate::llm::ChatProvider>,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let client = self.client.clone();
        let mut rx = self.client.subscribe_notifications().await?;
        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(MCPNotification::SamplingRequest { id, messages }) => {
                        // Convert MCP sampling messages to our Message format
                        let history: Vec<crate::message::Message> = messages
                            .iter()
                            .filter_map(|m| {
                                let role = m.get("role")?.as_str()?;
                                let content = m.get("content")?.as_str()?;
                                match role {
                                    "user" => Some(crate::message::Message::User(
                                        crate::message::UserMessage::text(content.to_string()),
                                    )),
                                    "assistant" => Some(crate::message::Message::Assistant {
                                        content: Some(content.to_string()),
                                        tool_calls: None,
                                    }),
                                    _ => None,
                                }
                            })
                            .collect();

                        // Call LLM
                        match llm.generate(None, history, vec![]).await {
                            Ok(mut generation) => {
                                let mut response_text = String::new();
                                while let Some(chunk) = generation.next_chunk().await {
                                    if let crate::message::ContentPart::Text { text } = chunk {
                                        response_text.push_str(&text);
                                    }
                                }
                                if let Err(e) = client.respond_sampling(id, response_text).await {
                                    tracing::warn!("Failed to respond to MCP sampling request: {}", e);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("LLM sampling failed: {}", e);
                                let _ = client.respond_sampling(id, format!("Error: {}", e)).await;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock MCP server that responds to JSON-RPC requests over stdio.
    fn mock_mcp_server_script(_responses: Vec<(String, String)>) -> Vec<String> {
        let script = r#"import sys, json, time, threading

def send(obj):
    print(json.dumps(obj), flush=True)

# For sampling tests: send sampling request then read response
sampling_mode = False

def read_sampling_response():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            resp = json.loads(line)
            if resp.get("result", {}).get("content", {}).get("type") == "text":
                send({"jsonrpc": "2.0", "id": 999, "result": {"sampling_response_received": True}})
                return
        except:
            pass

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
        method = req.get("method", "")
        req_id = req.get("id")
        
        if method == "resources/subscribe":
            send({"jsonrpc": "2.0", "id": req_id, "result": {"subscribed": True}})
            time.sleep(0.1)
            send({"jsonrpc": "2.0", "method": "notifications/resources/updated", "params": {"uri": req["params"]["uri"]}})
        elif method == "resources/read":
            send({"jsonrpc": "2.0", "id": req_id, "result": {"contents": [{"uri": req["params"]["uri"], "text": "resource content"}]}})
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": req_id, "result": {"tools": [{"name": "test_tool", "description": "A test tool", "inputSchema": {"type": "object"}}]}})
        elif method == "tools/call":
            send({"jsonrpc": "2.0", "id": req_id, "result": {"content": [{"type": "text", "text": "done"}], "isError": False}})
        elif method == "notifications/progress":
            pass
        else:
            send({"jsonrpc": "2.0", "id": req_id, "result": {}})
    except Exception as e:
        send({"jsonrpc": "2.0", "id": req.get("id", 0), "error": {"message": str(e)}})
"#;
        vec!["python3".to_string(), "-c".to_string(), script.to_string()]
    }

    /// A mock MCP server that sends a sampling request and expects a response.
    #[allow(dead_code)]
    fn mock_mcp_sampling_server() -> Vec<String> {
        let script = r#"import sys, json, threading, time

def send(obj):
    print(json.dumps(obj), flush=True)

# Start a thread to read the sampling response from stdin
def monitor():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
            # If we receive a JSON-RPC response with a result, it's the sampling response
            if "result" in msg and "model" in msg.get("result", {}):
                send({"jsonrpc": "2.0", "id": 999, "result": {"sampling_handled": True}})
                return
        except:
            pass

threading.Thread(target=monitor, daemon=True).start()

# Wait a bit for client to connect and start handler
time.sleep(0.2)

# Send a sampling request to the client
send({
    "jsonrpc": "2.0",
    "id": "sample-1",
    "method": "sampling/createMessage",
    "params": {
        "messages": [{"role": "user", "content": "Say hello"}]
    }
})

# Keep alive to give client time to respond
time.sleep(1.0)
"#;
        vec!["python3".to_string(), "-c".to_string(), script.to_string()]
    }

    #[test]
    fn test_tool_record_from_json_accepts_snake_case_schema() {
        let t = serde_json::json!({
            "name": "t1",
            "description": "d",
            "input_schema": {"type": "object", "properties": {"x": {"type": "number"}}}
        });
        let tr = MCPClient::tool_record_from_json(&t);
        assert_eq!(tr.name, "t1");
        assert_eq!(tr.description, "d");
        assert!(tr.input_schema.get("properties").is_some());
    }

    #[tokio::test]
    async fn test_mcp_client_list_tools() {
        let cmd = mock_mcp_server_script(vec![]);
        let client = MCPClient::new("test".to_string(), cmd);
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "test_tool");
    }

    #[tokio::test]
    async fn test_mcp_client_call_tool() {
        let cmd = mock_mcp_server_script(vec![]);
        let client = MCPClient::new("test".to_string(), cmd);
        let result = client.call_tool("test_tool", serde_json::json!({})).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            MCPContent::Text(t) => assert_eq!(t, "done"),
            _ => panic!("Expected text content"),
        }
    }

    #[tokio::test]
    async fn test_mcp_subscribe_resource() {
        let cmd = mock_mcp_server_script(vec![]);
        let client = MCPClient::new("test".to_string(), cmd);
        let mut rx = client.subscribe_resource("file:///test.txt").await.unwrap();

        // Wait for the resource update notification
        let updated = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated, "file:///test.txt");
    }

    #[tokio::test]
    async fn test_mcp_read_resource() {
        let cmd = mock_mcp_server_script(vec![]);
        let client = MCPClient::new("test".to_string(), cmd);
        let result = client.read_resource("file:///test.txt").await.unwrap();
        assert!(result["contents"].is_array());
    }

    #[tokio::test]
    async fn test_mcp_session_wrapper() {
        let cmd = mock_mcp_server_script(vec![]);
        let client = Arc::new(MCPClient::new("test".to_string(), cmd));
        let session = MCPSession::new(client);

        let tools = session.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);

        let result = session.read_resource("file:///test.txt").await.unwrap();
        assert!(result["contents"].is_array());
    }

    #[tokio::test]
    async fn test_mcp_notification_parsing() {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/resources/updated".to_string(),
            params: Some(serde_json::json!({"uri": "file:///test.txt"})),
        };
        let parsed = MCPNotification::from_jsonrpc(notif);
        match parsed {
            MCPNotification::ResourceUpdated { uri } => assert_eq!(uri, "file:///test.txt"),
            _ => panic!("Expected ResourceUpdated, got {:?}", parsed),
        }
    }

    #[tokio::test]
    async fn test_mcp_progress_notification() {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(serde_json::json!({
                "progressToken": "token-1",
                "progress": 50.0,
                "total": 100.0,
            })),
        };
        let parsed = MCPNotification::from_jsonrpc(notif);
        match parsed {
            MCPNotification::Progress {
                progress_token,
                progress,
                total,
            } => {
                assert_eq!(progress_token, "token-1");
                assert_eq!(progress, 50.0);
                assert_eq!(total, Some(100.0));
            }
            _ => panic!("Expected Progress, got {:?}", parsed),
        }
    }

    #[tokio::test]
    async fn test_mcp_sampling_handler_smoke() {
        // This test verifies the sampling handler can be started without errors.
        // Full end-to-end testing requires a mock server that sends sampling requests
        // and verifies responses, which is complex over stdio.
        let cmd = mock_mcp_server_script(vec![]);
        let client = Arc::new(MCPClient::new("test".to_string(), cmd));
        let session = MCPSession::new(client);

        let llm: std::sync::Arc<dyn crate::llm::ChatProvider> =
            std::sync::Arc::new(crate::llm::EchoProvider);
        let handle = session.start_sampling_handler(llm).await;
        assert!(handle.is_ok(), "Sampling handler should start without error");

        // Let it run briefly then abort
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        handle.unwrap().abort();
    }
}
