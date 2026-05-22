use crate::config::Profile;
use crate::storage::CodeGraph;
use crate::indexer::generate_skeleton_by_regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, AsyncReadExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// MCP 服务器向 UI 线程发送的消息事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpEvent {
    /// 拦截脱水并节省了 Token
    TokenSaved {
        path: String,
        saved_tokens: usize,
    },
    /// 服务器运行日志
    Log(String),
}

/// JSON-RPC 2.0 请求结构
#[derive(Debug, Deserialize, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    pub id: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 错误结构
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 响应结构
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    pub id: Option<serde_json::Value>,
}

/// MCP 工具定义接口
#[derive(Debug, Serialize)]
struct ToolDefinition {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Value,
}

/// MCP 工具调用参数
#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

pub struct McpServer {
    db: Arc<CodeGraph>,
    active_profile: Arc<std::sync::RwLock<Option<Profile>>>,
    ui_tx: UnboundedSender<McpEvent>,
}

impl McpServer {
    pub fn new(
        db: Arc<CodeGraph>,
        active_profile: Arc<std::sync::RwLock<Option<Profile>>>,
        ui_tx: UnboundedSender<McpEvent>,
    ) -> Self {
        Self {
            db,
            active_profile,
            ui_tx,
        }
    }

    /// 发送日志到 UI 线程
    fn log(&self, msg: String) {
        let _ = self.ui_tx.send(McpEvent::Log(msg));
    }

    /// 处理 JSON-RPC 请求并生成可能的响应
    pub fn handle_request(&self, raw_request: &str, agent_type: &str) -> Option<JsonRpcResponse> {
        let req: JsonRpcRequest = match serde_json::from_str(raw_request) {
            Ok(r) => r,
            Err(e) => {
                return Some(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                        data: None,
                    }),
                    id: None,
                });
            }
        };

        let response = match req.method.as_str() {
            "initialize" => self.handle_initialize(req.id),
            "notifications/initialized" => {
                // 初始化通知，不需要响应
                self.log("[MCP] Client initialized notifications received".to_string());
                None
            }
            "tools/list" => Some(self.handle_tools_list(req.id)),
            "tools/call" => Some(self.handle_tools_call(req.params, req.id, agent_type)),
            _ => Some(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {}", req.method),
                    data: None,
                }),
                id: req.id,
            }),
        };

        response
    }

    fn handle_initialize(&self, id: Option<serde_json::Value>) -> Option<JsonRpcResponse> {
        self.log("[MCP] Client handshake 'initialize'".to_string());
        Some(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            result: Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "Dehydrator4Win",
                    "version": "0.1.0"
                }
            })),
            error: None,
            id,
        })
    }

    fn handle_tools_list(&self, id: Option<serde_json::Value>) -> JsonRpcResponse {
        self.log("[MCP] Query tools list".to_string());
        let tools = vec![
            ToolDefinition {
                name: "list_files".to_string(),
                description: "List all files indexed in the active workspace profile.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "read_file".to_string(),
                description: "Read the content of a file. Large files are automatically dehydrated (implementation hidden) to save tokens.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "The absolute path of the file to read."
                        }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "search_symbols".to_string(),
                description: "Search for code symbols (functions, structs, classes) matching the query.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The symbol name or keyword to search for."
                        }
                    },
                    "required": ["query"]
                }),
            },
        ];

        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            result: Some(json!({ "tools": tools })),
            error: None,
            id,
        }
    }

    fn handle_tools_call(&self, params: serde_json::Value, id: Option<serde_json::Value>, agent_type: &str) -> JsonRpcResponse {
        let call_params: ToolCallParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32602,
                        message: format!("Invalid params: {}", e),
                        data: None,
                    }),
                    id,
                };
            }
        };

        self.log(format!("[MCP] Tool execution request: {}", call_params.name));

        match call_params.name.as_str() {
            "list_files" => self.execute_list_files(id),
            "read_file" => self.execute_read_file(call_params.arguments, id, agent_type),
            "search_symbols" => self.execute_search_symbols(call_params.arguments, id),
            _ => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Tool not found: {}", call_params.name),
                    data: None,
                }),
                id,
            },
        }
    }

    fn execute_list_files(&self, id: Option<serde_json::Value>) -> JsonRpcResponse {
        let profile_opt = self.active_profile.read().unwrap().clone();
        let profile = match profile_opt {
            Some(p) => p,
            None => {
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: Some(json!({
                        "content": [{
                            "type": "text",
                            "text": "Error: No active profile is selected. Please select a profile in the Dehydrator4Win UI first."
                        }],
                        "isError": true
                    })),
                    error: None,
                    id,
                };
            }
        };

        match self.db.get_files_by_profile(&profile.name) {
            Ok(files) => {
                let mut text = format!("Indexed files in active profile '{}':\n", profile.name);
                if files.is_empty() {
                    text.push_str("(No files indexed yet. Run a workspace scan.)\n");
                } else {
                    for f in files {
                        text.push_str(&format!("- {}\n", f.absolute_path));
                    }
                }
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: Some(json!({
                        "content": [{
                            "type": "text",
                            "text": text
                        }]
                    })),
                    error: None,
                    id,
                }
            }
            Err(e) => {
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: Some(json!({
                        "content": [{
                            "type": "text",
                            "text": format!("Error querying indexed files: {}", e)
                        }],
                        "isError": true
                    })),
                    error: None,
                    id,
                }
            }
        }
    }

    fn execute_read_file(&self, arguments: serde_json::Value, id: Option<serde_json::Value>, agent_type: &str) -> JsonRpcResponse {
        let path_str = match arguments.get("path").or_else(|| arguments.get("absolute_path")).and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: Some(json!({
                        "content": [{
                            "type": "text",
                            "text": "Error: Missing 'path' argument in read_file request."
                        }],
                        "isError": true
                    })),
                    error: None,
                    id,
                };
            }
        };

        let path = Path::new(path_str);
        if !path.exists() {
            return JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                result: Some(json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Error: File not found: {}", path_str)
                    }],
                    "isError": true
                })),
                error: None,
                id,
            };
        }

        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: Some(json!({
                        "content": [{
                            "type": "text",
                            "text": format!("Error reading file: {}", e)
                        }],
                        "isError": true
                    })),
                    error: None,
                    id,
                };
            }
        };

        let max_lines = {
            let guard = self.active_profile.read().unwrap();
            guard.as_ref().map(|p| p.max_file_read_lines).unwrap_or(500)
        };

        let profile_opt = self.active_profile.read().unwrap().clone();
        let profile_name = profile_opt.as_ref().map(|p| p.name.as_str()).unwrap_or("default");

        let raw_tokens = (content.len() / 3) as i64;
        let line_count = content.lines().count();

        if line_count > max_lines {
            // 大文件触发正则脱水算法
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            let (dehydrated_content, _) = generate_skeleton_by_regex(&content, ext);

            // 估计节省的 Token 数量 (代码字符数减少 / 3)
            let saved_tokens = content.len().saturating_sub(dehydrated_content.len()) / 3;
            let optimized_tokens = (dehydrated_content.len() / 3) as i64;

            let _ = self.db.insert_token_analytics(profile_name, agent_type, raw_tokens, optimized_tokens);

            self.log(format!(
                "[MCP] Intercepted oversized read: {} ({} lines). Dehydrating to save estimated {} tokens.",
                path_str, line_count, saved_tokens
            ));

            if saved_tokens > 0 {
                let _ = self.ui_tx.send(McpEvent::TokenSaved {
                    path: path_str.to_string(),
                    saved_tokens,
                });
            }

            let response_text = format!(
                "// [Dehydrated by Dehydrator4Win: {} lines hidden to save estimated {} tokens]\n{}",
                line_count.saturating_sub(dehydrated_content.lines().count()),
                saved_tokens,
                dehydrated_content
            );

            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                result: Some(json!({
                    "content": [{
                        "type": "text",
                        "text": response_text
                    }]
                })),
                error: None,
                id,
            }
        } else {
            let _ = self.db.insert_token_analytics(profile_name, agent_type, raw_tokens, raw_tokens);
            self.log(format!("[MCP] Read file directly: {} ({} lines)", path_str, line_count));
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                result: Some(json!({
                    "content": [{
                        "type": "text",
                        "text": content
                    }]
                })),
                error: None,
                id,
            }
        }
    }

    fn execute_search_symbols(&self, arguments: serde_json::Value, id: Option<serde_json::Value>) -> JsonRpcResponse {
        let query = match arguments.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => {
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: Some(json!({
                        "content": [{
                            "type": "text",
                            "text": "Error: Missing 'query' argument in search_symbols request."
                        }],
                        "isError": true
                    })),
                    error: None,
                    id,
                };
            }
        };

        match self.db.find_symbol_definitions(query) {
            Ok(defs) => {
                let mut text = format!("Found {} symbol definition(s) for '{}':\n", defs.len(), query);
                if defs.is_empty() {
                    text.push_str("No matching symbols found. Make sure the symbol is defined and workspace is scanned.\n");
                } else {
                    for (file_rec, sym_rec) in defs {
                        text.push_str(&format!(
                            "- Symbol: {} ({}) defined in {} [Line {}-{}]\n  Signature: {}\n",
                            sym_rec.name,
                            sym_rec.kind,
                            file_rec.absolute_path,
                            sym_rec.start_line,
                            sym_rec.end_line,
                            sym_rec.signature
                        ));
                    }
                }
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: Some(json!({
                        "content": [{
                            "type": "text",
                            "text": text
                        }]
                    })),
                    error: None,
                    id,
                }
            }
            Err(e) => {
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: Some(json!({
                        "content": [{
                            "type": "text",
                            "text": format!("Error searching symbols: {}", e)
                        }],
                        "isError": true
                    })),
                    error: None,
                    id,
                }
            }
        }
    }
}

/// 运行 Stdio MCP 服务端。由客户端通过标准输入输出传输单行 JSON。
pub async fn run_mcp_stdio_server(
    db: Arc<CodeGraph>,
    active_profile: Arc<std::sync::RwLock<Option<Profile>>>,
    ui_tx: UnboundedSender<McpEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let server = McpServer::new(db, active_profile, ui_tx);
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    let mut writer = tokio::io::stdout();

    while let Ok(n) = reader.read_line(&mut line).await {
        if n == 0 {
            break;
        }

        if let Some(resp) = server.handle_request(&line, "Claude") {
            if let Ok(resp_str) = serde_json::to_string(&resp) {
                let mut resp_line = resp_str;
                resp_line.push('\n');
                writer.write_all(resp_line.as_bytes()).await?;
                writer.flush().await?;
            }
        }
        line.clear();
    }

    Ok(())
}

static CONNECTION_ID_COUNTER: AtomicUsize = AtomicUsize::new(1);

async fn write_http_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    status: &str,
    headers: &[&str],
    body: &str,
) -> std::io::Result<()> {
    let mut resp = format!("HTTP/1.1 {}\r\n", status);
    for h in headers {
        resp.push_str(h);
        resp.push_str("\r\n");
    }
    resp.push_str("Access-Control-Allow-Origin: *\r\n");
    resp.push_str("Access-Control-Allow-Headers: *\r\n");
    resp.push_str("Access-Control-Allow-Methods: *\r\n");
    resp.push_str(&format!("Content-Length: {}\r\n", body.len()));
    resp.push_str("\r\n");
    resp.push_str(body);
    writer.write_all(resp.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_sse_headers<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
) -> std::io::Result<()> {
    let resp = "HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\
Access-Control-Allow-Origin: *\r\n\
Access-Control-Allow-Headers: *\r\n\
Access-Control-Allow-Methods: *\r\n\
\r\n";
    writer.write_all(resp.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// 运行异步 TCP 和 HTTP SSE 双通道 MCP 服务端。可用于外部任务或后台线程启动。
pub async fn run_mcp_server(
    addr: &str,
    db: Arc<CodeGraph>,
    active_profile: Arc<std::sync::RwLock<Option<Profile>>>,
    ui_tx: UnboundedSender<McpEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;
    let server = Arc::new(McpServer::new(db, active_profile, ui_tx));
    let active_connections = Arc::new(tokio::sync::Mutex::new(HashMap::<String, tokio::sync::mpsc::UnboundedSender<String>>::new()));

    let _ = server.ui_tx.send(McpEvent::Log(format!("[MCP] Server listening on {}", addr)));

    loop {
        let (socket, _) = match listener.accept().await {
            Ok(res) => res,
            Err(e) => {
                let _ = server.ui_tx.send(McpEvent::Log(format!("[MCP] Failed to accept connection: {}", e)));
                continue;
            }
        };

        let server_clone = server.clone();
        let active_connections_clone = active_connections.clone();
        tokio::spawn(async move {
            let _ = server_clone.ui_tx.send(McpEvent::Log("[MCP] Client connected".to_string()));
            let (reader, mut writer) = tokio::io::split(socket);
            let mut buf_reader = BufReader::new(reader);

            loop {
                let mut line = String::new();
                match buf_reader.read_line(&mut line).await {
                    Ok(n) if n > 0 => {
                        let is_http = line.starts_with("GET ")
                            || line.starts_with("POST ")
                            || line.starts_with("OPTIONS ")
                            || line.starts_with("PUT ")
                            || line.starts_with("DELETE ");

                        if is_http {
                            let request_line = line;
                            let mut headers = Vec::new();
                            loop {
                                let mut header_line = String::new();
                                if buf_reader.read_line(&mut header_line).await.is_err() {
                                    break;
                                }
                                if header_line == "\r\n" || header_line == "\n" || header_line.is_empty() {
                                    break;
                                }
                                headers.push(header_line);
                            }

                            let parts: Vec<&str> = request_line.split_whitespace().collect();
                            if parts.len() < 2 {
                                break;
                            }
                            let method = parts[0];
                            let uri = parts[1];

                            let mut host = "127.0.0.1:3001".to_string();
                            let mut is_sse_accept = false;
                            for h in &headers {
                                let h_lower = h.to_lowercase();
                                if h_lower.starts_with("host:") {
                                    if let Some(pos) = h.find(':') {
                                        host = h[pos + 1..].trim().to_string();
                                    }
                                }
                                if h_lower.starts_with("accept:") && h_lower.contains("text/event-stream") {
                                    is_sse_accept = true;
                                }
                            }

                            if method == "OPTIONS" {
                                if write_http_response(&mut writer, "200 OK", &[], "").await.is_err() {
                                    break;
                                }
                                continue;
                            }

                            let is_sse_request = method == "GET" && (is_sse_accept || uri.starts_with("/sse") || uri == "/");

                            if is_sse_request {
                                let conn_id = CONNECTION_ID_COUNTER.fetch_add(1, Ordering::SeqCst).to_string();
                                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                
                                {
                                    let mut conns = active_connections_clone.lock().await;
                                    conns.insert(conn_id.clone(), tx);
                                }

                                if write_sse_headers(&mut writer).await.is_ok() {
                                    let endpoint_msg = format!("event: endpoint\ndata: http://{}/message?connection_id={}\n\n", host, conn_id);
                                    if writer.write_all(endpoint_msg.as_bytes()).await.is_ok() {
                                        let _ = writer.flush().await;
                                    }

                                    while let Some(msg) = rx.recv().await {
                                        let event_msg = format!("event: message\ndata: {}\n\n", msg);
                                        if writer.write_all(event_msg.as_bytes()).await.is_err() {
                                            break;
                                        }
                                        let _ = writer.flush().await;
                                    }
                                }

                                {
                                    let mut conns = active_connections_clone.lock().await;
                                    conns.remove(&conn_id);
                                }
                                break;
                            } else if method == "POST" {
                                let conn_id = if let Some(pos) = uri.find("connection_id=") {
                                    uri[pos + 14..].split('&').next().map(|s| s.to_string())
                                } else {
                                    None
                                };

                                let mut content_length = 0;
                                for h in &headers {
                                    let h_lower = h.to_lowercase();
                                    if h_lower.starts_with("content-length:") {
                                        if let Some(val_str) = h.split(':').nth(1) {
                                            content_length = val_str.trim().parse::<usize>().unwrap_or(0);
                                        }
                                    }
                                }

                                let mut body = vec![0u8; content_length];
                                if buf_reader.read_exact(&mut body).await.is_ok() {
                                    let body_str = String::from_utf8_lossy(&body);
                                    let mut user_agent = String::new();
                                    for h in &headers {
                                        let h_lower = h.to_lowercase();
                                        if h_lower.starts_with("user-agent:") {
                                            if let Some(pos) = h.find(':') {
                                                user_agent = h[pos + 1..].trim().to_string();
                                            }
                                        }
                                    }
                                    let agent_type = if user_agent.is_empty() {
                                        "OpenCode"
                                    } else {
                                        map_user_agent(&user_agent)
                                    };

                                    if let Some(resp) = server_clone.handle_request(&body_str, agent_type) {
                                        if let Ok(resp_str) = serde_json::to_string(&resp) {
                                            if uri.starts_with("/message") {
                                                let mut sent = false;
                                                if let Some(ref cid) = conn_id {
                                                    let conns = active_connections_clone.lock().await;
                                                    if let Some(tx) = conns.get(cid) {
                                                        let _ = tx.send(resp_str.clone());
                                                        sent = true;
                                                    }
                                                }
                                                if !sent {
                                                    let conns = active_connections_clone.lock().await;
                                                    if let Some(tx) = conns.values().next() {
                                                        let _ = tx.send(resp_str);
                                                    }
                                                }
                                                if write_http_response(&mut writer, "200 OK", &[], "OK").await.is_err() {
                                                    break;
                                                }
                                            } else {
                                                // Streamable HTTP: return JSON response directly in POST response body
                                                if write_http_response(
                                                    &mut writer,
                                                    "200 OK",
                                                    &["Content-Type: application/json"],
                                                    &resp_str,
                                                ).await.is_err() {
                                                    break;
                                                }
                                            }
                                        } else {
                                            if write_http_response(&mut writer, "500 Internal Error", &[], "Internal Error").await.is_err() {
                                                break;
                                            }
                                        }
                                    } else {
                                        // Notification request (e.g. notifications/initialized), return 202 Accepted/200 OK
                                        if write_http_response(&mut writer, "202 Accepted", &[], "").await.is_err() {
                                            break;
                                        }
                                    }
                                } else {
                                    if write_http_response(&mut writer, "400 Bad Request", &[], "Bad Request").await.is_err() {
                                        break;
                                    }
                                }
                                continue;
                            } else {
                                if write_http_response(&mut writer, "404 Not Found", &[], "Not Found").await.is_err() {
                                    break;
                                }
                                continue;
                            }
                        } else {
                            if let Some(resp) = server_clone.handle_request(&line, "Claude") {
                                if let Ok(resp_str) = serde_json::to_string(&resp) {
                                    let mut resp_line = resp_str;
                                    resp_line.push('\n');
                                    if writer.write_all(resp_line.as_bytes()).await.is_err() {
                                        break;
                                    }
                                    let _ = writer.flush().await;
                                }
                            }
                            line.clear();

                            while let Ok(n) = buf_reader.read_line(&mut line).await {
                                if n == 0 {
                                    break;
                                }
                                if let Some(resp) = server_clone.handle_request(&line, "Claude") {
                                    if let Ok(resp_str) = serde_json::to_string(&resp) {
                                        let mut resp_line = resp_str;
                                        resp_line.push('\n');
                                        if writer.write_all(resp_line.as_bytes()).await.is_err() {
                                            break;
                                        }
                                        let _ = writer.flush().await;
                                    }
                                }
                                line.clear();
                            }
                            break;
                        }
                    }
                    _ => break,
                }
            }
            let _ = server_clone.ui_tx.send(McpEvent::Log("[MCP] Client disconnected".to_string()));
        });
    }
}

fn map_user_agent(ua: &str) -> &'static str {
    let ua_lower = ua.to_lowercase();
    if ua_lower.contains("codex") || ua_lower.contains("vscode") || ua_lower.contains("copilot") {
        "Codex"
    } else if ua_lower.contains("claude") || ua_lower.contains("cline") || ua_lower.contains("anthropic") {
        "Claude"
    } else if ua_lower.contains("gemini") || ua_lower.contains("google") {
        "Gemini"
    } else {
        "OpenCode"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceFolder;
    use std::io::Write;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_json_rpc_initialize() {
        let db = Arc::new(CodeGraph::open_in_memory().unwrap());
        let active_profile = Arc::new(std::sync::RwLock::new(None));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let server = McpServer::new(db, active_profile, ui_tx);

        let req = json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {},
            "id": 1
        });
        let raw_req = serde_json::to_string(&req).unwrap();
        let raw_resp = server.handle_request(&raw_req, "Claude").unwrap();

        assert_eq!(raw_resp.jsonrpc, "2.0");
        assert!(raw_resp.result.is_some());
        assert_eq!(raw_resp.id.unwrap(), json!(1));

        let event = ui_rx.recv().await.unwrap();
        assert!(matches!(event, McpEvent::Log(_)));
    }

    #[tokio::test]
    async fn test_mcp_tool_list_files_empty_and_success() {
        let db = Arc::new(CodeGraph::open_in_memory().unwrap());
        let active_profile = Arc::new(std::sync::RwLock::new(None));
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let server = McpServer::new(db.clone(), active_profile.clone(), ui_tx);

        // 1. 无活动 profile 错误测试
        let req = json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": "list_files",
                "arguments": {}
            },
            "id": 2
        });
        let raw_req = serde_json::to_string(&req).unwrap();
        let resp = server.handle_request(&raw_req, "Claude").unwrap();
        let result = resp.result.unwrap();
        assert!(result.get("isError").unwrap().as_bool().unwrap());
        assert!(result.get("content").unwrap().as_array().unwrap()[0]
            .get("text").unwrap().as_str().unwrap()
            .contains("No active profile"));

        // 2. 有活动 profile，但无数据测试
        let profile = Profile {
            name: "test-profile-mcp".to_string(),
            description: "Desc".to_string(),
            workspaces: vec![],
            exclude: vec![],
            max_file_read_lines: 10,
        };
        *active_profile.write().unwrap() = Some(profile);

        let resp2 = server.handle_request(&raw_req, "Claude").unwrap();
        let result2 = resp2.result.unwrap();
        assert!(result2.get("isError").is_none());
        assert!(result2.get("content").unwrap().as_array().unwrap()[0]
            .get("text").unwrap().as_str().unwrap()
            .contains("No files indexed"));

        // 3. 有数据测试
        db.upsert_file("test-profile-mcp", "C:/my_project/main.rs", 12345).unwrap();
        let resp3 = server.handle_request(&raw_req, "Claude").unwrap();
        let result3 = resp3.result.unwrap();
        let text3 = result3.get("content").unwrap().as_array().unwrap()[0]
            .get("text").unwrap().as_str().unwrap();
        assert!(text3.contains("C:/my_project/main.rs"));
    }

    #[tokio::test]
    async fn test_mcp_tool_read_file_intercept() {
        let db = Arc::new(CodeGraph::open_in_memory().unwrap());
        let active_profile = Arc::new(std::sync::RwLock::new(None));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let server = McpServer::new(db, active_profile.clone(), ui_tx);

        let mut temp_file = std::env::temp_dir();
        temp_file.push("test_mcp_read.rs");
        {
            let mut f = fs::File::create(&temp_file).unwrap();
            for line_idx in 1..=20 {
                writeln!(f, "fn func_{}() {{", line_idx).unwrap();
                for print_idx in 1..=10 {
                    writeln!(f, "    println!(\"line {} - printing message {}\");", line_idx, print_idx).unwrap();
                }
                writeln!(f, "}}").unwrap();
            }
        }

        // 1. 设置 max_file_read_lines = 300 (不触发脱水)
        let profile = Profile {
            name: "test".to_string(),
            description: "Desc".to_string(),
            workspaces: vec![WorkspaceFolder {
                path: std::env::temp_dir(),
                tags: vec![],
            }],
            exclude: vec![],
            max_file_read_lines: 300,
        };
        *active_profile.write().unwrap() = Some(profile);

        let req = json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": "read_file",
                "arguments": {
                    "path": temp_file.to_str().unwrap()
                }
            },
            "id": 3
        });

        let raw_req = serde_json::to_string(&req).unwrap();
        let resp = server.handle_request(&raw_req, "Claude").unwrap();
        let result = resp.result.unwrap();
        let text = result.get("content").unwrap().as_array().unwrap()[0]
            .get("text").unwrap().as_str().unwrap();
        assert!(!text.contains("Dehydrated by Dehydrator4Win"));
        assert!(text.contains("fn func_1()"));

        // 2. 设置 max_file_read_lines = 10 (触发脱水)
        {
            let mut guard = active_profile.write().unwrap();
            guard.as_mut().unwrap().max_file_read_lines = 10;
        }

        let resp2 = server.handle_request(&raw_req, "Claude").unwrap();
        let result2 = resp2.result.unwrap();
        let text2 = result2.get("content").unwrap().as_array().unwrap()[0]
            .get("text").unwrap().as_str().unwrap();
        assert!(text2.contains("Dehydrated by Dehydrator4Win"));
        assert!(text2.contains("fn func_1()"));
        assert!(text2.contains("// [Implementation hidden by Dehydrator4Win to save Token]"));

        // 检查是否有 TokenSaved 事件产生
        let mut has_token_saved = false;
        while let Ok(event) = ui_rx.try_recv() {
            if let McpEvent::TokenSaved { .. } = event {
                has_token_saved = true;
            }
        }
        assert!(has_token_saved);

        let _ = fs::remove_file(temp_file);
    }
}
