//! Localhost HTTP bridge (127.0.0.1:47120) exposing Archive reads and Worker actions for whatsapp-mcp.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

use super::{FileSendOptions, Worker};
use crate::archive::{McpChat, McpContact, McpMessage, McpMessageContext};
use crate::paths::AppDirs;
const DEFAULT_MCP_PORT: u16 = 47_120;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

pub(super) enum McpCommand {
    Status {
        reply: oneshot::Sender<StatusResponse>,
    },
    ListChats {
        req: ListChatsRequest,
        reply: oneshot::Sender<Result<Vec<McpChat>, String>>,
    },
    GetChat {
        req: GetChatRequest,
        reply: oneshot::Sender<Result<Option<McpChat>, String>>,
    },
    GetDirectChat {
        req: GetDirectChatRequest,
        reply: oneshot::Sender<Result<Option<McpChat>, String>>,
    },
    SearchContacts {
        req: SearchContactsRequest,
        reply: oneshot::Sender<Result<Vec<McpContact>, String>>,
    },
    GetContactChats {
        req: GetContactChatsRequest,
        reply: oneshot::Sender<Result<Vec<McpChat>, String>>,
    },
    GetLastInteraction {
        req: GetLastInteractionRequest,
        reply: oneshot::Sender<Result<Option<McpMessage>, String>>,
    },
    ListMessages {
        req: ListMessagesRequest,
        reply: oneshot::Sender<Result<Vec<McpMessage>, String>>,
    },
    GetMessageContext {
        req: GetMessageContextRequest,
        reply: oneshot::Sender<Result<Option<McpMessageContext>, String>>,
    },
    Send {
        req: SendMessageRequest,
        reply: oneshot::Sender<SendMessageResponse>,
    },
    Download {
        req: DownloadMediaRequest,
        reply: oneshot::Sender<DownloadMediaResponse>,
    },
}

#[derive(Clone, Serialize)]
pub(super) struct StatusResponse {
    pub account_id: String,
    pub active: bool,
    pub connected: bool,
    pub status: String,
    pub me_pn: Option<String>,
    pub me_name: Option<String>,
    pub archive_path: String,
}

#[derive(Serialize)]
struct GlobalStatusResponse {
    pub account_id: String,
    pub active: bool,
    pub connected: bool,
    pub status: String,
    pub me_pn: Option<String>,
    pub me_name: Option<String>,
    pub archive_path: String,
    pub accounts: Vec<StatusResponse>,
}

#[derive(Deserialize, Default)]
struct AccountSelector {
    #[serde(default)]
    pub account: Option<String>,
}

#[derive(Deserialize, Default)]
struct AccountsFile {
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub order: Vec<String>,
}
#[derive(Deserialize)]
pub(super) struct ListChatsRequest {
    pub query: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub page: usize,
    #[serde(default = "default_true")]
    pub include_last_message: bool,
    #[serde(default = "default_sort_by")]
    pub sort_by: String,
}

#[derive(Deserialize)]
pub(super) struct GetChatRequest {
    pub chat_jid: String,
    #[serde(default = "default_true")]
    pub include_last_message: bool,
}

#[derive(Deserialize)]
pub(super) struct GetDirectChatRequest {
    pub sender_phone_number: String,
}

#[derive(Deserialize)]
pub(super) struct SearchContactsRequest {
    #[serde(default)]
    pub query: String,
}

#[derive(Deserialize)]
pub(super) struct GetContactChatsRequest {
    pub jid: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub page: usize,
}

#[derive(Deserialize)]
pub(super) struct GetLastInteractionRequest {
    pub jid: String,
}

#[derive(Deserialize)]
pub(super) struct ListMessagesRequest {
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub sender_phone_number: Option<String>,
    pub chat_jid: Option<String>,
    pub query: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub page: usize,
}

#[derive(Deserialize)]
pub(super) struct GetMessageContextRequest {
    pub message_id: String,
    pub chat_jid: Option<String>,
    #[serde(default = "default_context")]
    pub before: usize,
    #[serde(default = "default_context")]
    pub after: usize,
}

#[derive(Deserialize)]
pub(super) struct SendMessageRequest {
    pub recipient: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub media_path: String,
    #[serde(default)]
    pub is_voice: bool,
}

#[derive(Serialize)]
pub(super) struct SendMessageResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Deserialize)]
pub(super) struct DownloadMediaRequest {
    pub message_id: String,
    #[serde(default)]
    pub chat_jid: String,
}

#[derive(Serialize)]
pub(super) struct DownloadMediaResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

fn default_limit() -> usize {
    20
}

fn default_true() -> bool {
    true
}

fn default_sort_by() -> String {
    "last_active".to_owned()
}

fn default_context() -> usize {
    5
}

type WorkerEntry = (String, mpsc::UnboundedSender<McpCommand>);

static REGISTRY: LazyLock<Mutex<Vec<WorkerEntry>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static SERVER_INIT: OnceLock<()> = OnceLock::new();

fn registry() -> &'static Mutex<Vec<WorkerEntry>> {
    &REGISTRY
}

fn read_accounts_roster() -> (String, Vec<String>) {
    let path = AppDirs::discover().accounts_file();
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(parsed) = serde_json::from_slice::<AccountsFile>(&bytes)
    {
        let active = if parsed.active.is_empty() {
            "1".to_owned()
        } else {
            parsed.active
        };
        return (active, parsed.order);
    }
    ("1".to_owned(), vec!["1".to_owned()])
}

fn live_workers() -> Vec<WorkerEntry> {
    let mut guard = registry().lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|(_, tx)| !tx.is_closed());
    guard.clone()
}

async fn collect_all_statuses() -> Vec<(StatusResponse, mpsc::UnboundedSender<McpCommand>)> {
    let (active_id, order) = read_accounts_roster();
    let mut entries = live_workers();
    entries.sort_by_key(|(id, _)| order.iter().position(|o| o == id).unwrap_or(usize::MAX - 1));
    let mut out = Vec::with_capacity(entries.len());
    for (id, tx) in entries {
        let (reply, rx) = oneshot::channel();
        if tx.send(McpCommand::Status { reply }).is_ok()
            && let Ok(mut st) = rx.await
        {
            st.active = id == active_id;
            out.push((st, tx));
        }
    }
    out
}

fn digits_only(s: &str) -> String {
    let head = s.split('@').next().unwrap_or(s);
    let head = head.split(':').next().unwrap_or(head);
    head.chars().filter(|c| c.is_ascii_digit()).collect()
}

async fn resolve_worker(
    selector: Option<&str>,
) -> Option<(StatusResponse, mpsc::UnboundedSender<McpCommand>)> {
    let all = collect_all_statuses().await;
    if all.is_empty() {
        return None;
    }
    if let Some(raw) = selector.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(found) = all.iter().find(|(st, _)| st.account_id == raw) {
            return Some(found.clone());
        }
        let needle_digits = digits_only(raw);
        if !needle_digits.is_empty()
            && let Some(found) = all.iter().find(|(st, _)| {
                st.me_pn.as_deref().map(digits_only).is_some_and(|d| {
                    !d.is_empty() && (d == needle_digits || d.ends_with(&needle_digits))
                })
            })
        {
            return Some(found.clone());
        }
        let lower = raw.to_ascii_lowercase();
        if let Some(found) = all.iter().find(|(st, _)| {
            st.me_name
                .as_deref()
                .is_some_and(|n| n.to_ascii_lowercase().contains(&lower))
        }) {
            return Some(found.clone());
        }
        return None;
    }
    if let Some(active) = all.iter().find(|(st, _)| st.active && st.connected) {
        return Some(active.clone());
    }
    if let Some(active) = all.iter().find(|(st, _)| st.active) {
        return Some(active.clone());
    }
    if let Some(connected) = all.iter().find(|(st, _)| st.connected) {
        return Some(connected.clone());
    }
    all.into_iter().next()
}

pub(super) fn spawn_server(account_id: String) -> mpsc::UnboundedReceiver<McpCommand> {
    let (tx, rx) = mpsc::unbounded_channel();
    {
        let mut guard = registry().lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|(id, sender)| id != &account_id && !sender.is_closed());
        guard.push((account_id, tx));
    }
    #[cfg(test)]
    if std::env::var_os("ZAPFAST_MCP_PORT").is_none() {
        return rx;
    }
    SERVER_INIT.get_or_init(|| {
        let port = std::env::var("ZAPFAST_MCP_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(DEFAULT_MCP_PORT);
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let _ = std::thread::Builder::new()
            .name("zapfast-mcp-http".to_owned())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(async move {
                    let listener = match TcpListener::bind(addr).await {
                        Ok(listener) => {
                            log::info!("MCP localhost bridge listening on http://{addr}");
                            listener
                        }
                        Err(error) => {
                            log::warn!("MCP localhost bridge could not bind {addr}: {error}");
                            return;
                        }
                    };
                    loop {
                        let Ok((stream, _peer)) = listener.accept().await else {
                            continue;
                        };
                        tokio::spawn(async move {
                            let _ = handle_connection(stream).await;
                        });
                    }
                });
            });
    });
    rx
}

async fn handle_connection(stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
            && let Ok(len) = value.trim().parse::<usize>()
        {
            content_length = len.min(MAX_BODY_BYTES);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }
    let mut stream = reader.into_inner();

    let (status, payload) = route_request(method, path, &body).await;
    let status_text = match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        404 => "404 Not Found",
        405 => "405 Method Not Allowed",
        _ => "500 Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

fn json_error(status: u16, msg: impl Into<String>) -> (u16, Vec<u8>) {
    let body = serde_json::json!({
        "success": false,
        "error": msg.into(),
    });
    (status, serde_json::to_vec(&body).unwrap_or_default())
}

fn json_ok<T: Serialize>(value: &T) -> (u16, Vec<u8>) {
    match serde_json::to_vec(value) {
        Ok(bytes) => (200, bytes),
        Err(err) => json_error(500, err.to_string()),
    }
}

fn query_account_param(raw_path: &str) -> (&str, Option<String>) {
    let Some((base, query)) = raw_path.split_once('?') else {
        return (raw_path, None);
    };
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == "account"
            && !v.is_empty()
        {
            return (base, Some(v.to_owned()));
        }
    }
    (base, None)
}

async fn route_request(method: &str, raw_path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let (path, query_acct) = query_account_param(raw_path);
    let body_acct = if !body.is_empty() {
        serde_json::from_slice::<AccountSelector>(body)
            .ok()
            .and_then(|s| s.account)
    } else {
        None
    };
    let selector = body_acct.as_deref().or(query_acct.as_deref());

    if (method == "GET" || method == "POST") && path == "/api/accounts" {
        let all: Vec<StatusResponse> = collect_all_statuses()
            .await
            .into_iter()
            .map(|(st, _)| st)
            .collect();
        return json_ok(&all);
    }

    if (method == "GET" || method == "POST") && (path == "/api/status" || path == "/health") {
        let all: Vec<StatusResponse> = collect_all_statuses()
            .await
            .into_iter()
            .map(|(st, _)| st)
            .collect();
        let Some((primary, _)) = resolve_worker(selector).await else {
            return json_error(404, "No matching ZapFast account found");
        };
        let res = GlobalStatusResponse {
            account_id: primary.account_id,
            active: primary.active,
            connected: primary.connected,
            status: primary.status,
            me_pn: primary.me_pn,
            me_name: primary.me_name,
            archive_path: primary.archive_path,
            accounts: all,
        };
        return json_ok(&res);
    }

    if method != "POST" {
        return json_error(405, "Method not allowed");
    }

    let Some((_, tx)) = resolve_worker(selector).await else {
        return json_error(404, "Requested WhatsApp account not found in ZapFast");
    };
    match path {
        "/api/chats" => {
            let Ok(req) = serde_json::from_slice::<ListChatsRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::ListChats { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(Ok(rows)) => json_ok(&rows),
                Ok(Err(err)) => json_error(500, err),
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/chat" => {
            let Ok(req) = serde_json::from_slice::<GetChatRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::GetChat { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(Ok(row)) => json_ok(&row),
                Ok(Err(err)) => json_error(500, err),
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/direct-chat" => {
            let Ok(req) = serde_json::from_slice::<GetDirectChatRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::GetDirectChat { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(Ok(row)) => json_ok(&row),
                Ok(Err(err)) => json_error(500, err),
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/contacts" => {
            let Ok(req) = serde_json::from_slice::<SearchContactsRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::SearchContacts { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(Ok(rows)) => json_ok(&rows),
                Ok(Err(err)) => json_error(500, err),
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/contact-chats" => {
            let Ok(req) = serde_json::from_slice::<GetContactChatsRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::GetContactChats { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(Ok(rows)) => json_ok(&rows),
                Ok(Err(err)) => json_error(500, err),
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/last-interaction" => {
            let Ok(req) = serde_json::from_slice::<GetLastInteractionRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx
                .send(McpCommand::GetLastInteraction { req, reply })
                .is_err()
            {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(Ok(row)) => json_ok(&row),
                Ok(Err(err)) => json_error(500, err),
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/messages" => {
            let Ok(req) = serde_json::from_slice::<ListMessagesRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::ListMessages { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(Ok(rows)) => json_ok(&rows),
                Ok(Err(err)) => json_error(500, err),
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/message-context" => {
            let Ok(req) = serde_json::from_slice::<GetMessageContextRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx
                .send(McpCommand::GetMessageContext { req, reply })
                .is_err()
            {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(Ok(ctx)) => json_ok(&ctx),
                Ok(Err(err)) => json_error(500, err),
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/send" => {
            let Ok(req) = serde_json::from_slice::<SendMessageRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::Send { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(res) => {
                    let status = if res.success { 200 } else { 500 };
                    (status, serde_json::to_vec(&res).unwrap_or_default())
                }
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/download" => {
            let Ok(req) = serde_json::from_slice::<DownloadMediaRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::Download { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(res) => {
                    let status = if res.success { 200 } else { 500 };
                    (status, serde_json::to_vec(&res).unwrap_or_default())
                }
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        _ => json_error(404, "Not found"),
    }
}

impl Worker {
    fn resolve_recipient_chat(&self, recipient: &str) -> Option<String> {
        let trimmed = recipient.trim().trim_start_matches('+');
        if trimmed.is_empty() {
            return None;
        }
        let jid_str = if trimmed.contains('@') {
            self.canonical_str(trimmed)
        } else {
            let candidates = self.archive.mcp_resolve_jids(trimmed);
            candidates
                .into_iter()
                .find(|c| c.ends_with("@s.whatsapp.net"))
                .unwrap_or_else(|| format!("{trimmed}@s.whatsapp.net"))
        };
        Self::jid_of(&jid_str).map(|_| jid_str)
    }

    pub(super) fn handle_mcp(&mut self, cmd: McpCommand) {
        match cmd {
            McpCommand::Status { reply } => {
                let _ = reply.send(StatusResponse {
                    account_id: self.dirs.id.as_str().to_owned(),
                    active: false,
                    connected: self.status.is_connected(),
                    status: self.status.log_label().to_owned(),
                    me_pn: self.me_pn.clone(),
                    me_name: self.me_name.clone(),
                    archive_path: self.dirs.archive_db().to_string_lossy().into_owned(),
                });
            }
            McpCommand::ListChats { req, reply } => {
                let res = self
                    .archive
                    .mcp_list_chats(
                        req.query.as_deref(),
                        req.limit,
                        req.page,
                        req.include_last_message,
                        &req.sort_by,
                    )
                    .map_err(|e| e.to_string());
                let _ = reply.send(res);
            }
            McpCommand::GetChat { req, reply } => {
                let res = self
                    .archive
                    .mcp_get_chat(&req.chat_jid, req.include_last_message)
                    .map_err(|e| e.to_string());
                let _ = reply.send(res);
            }
            McpCommand::GetDirectChat { req, reply } => {
                let res = self
                    .archive
                    .mcp_get_direct_chat_by_contact(&req.sender_phone_number)
                    .map_err(|e| e.to_string());
                let _ = reply.send(res);
            }
            McpCommand::SearchContacts { req, reply } => {
                let res = self
                    .archive
                    .mcp_search_contacts(&req.query)
                    .map_err(|e| e.to_string());
                let _ = reply.send(res);
            }
            McpCommand::GetContactChats { req, reply } => {
                let res = self
                    .archive
                    .mcp_get_contact_chats(&req.jid, req.limit, req.page)
                    .map_err(|e| e.to_string());
                let _ = reply.send(res);
            }
            McpCommand::GetLastInteraction { req, reply } => {
                let res = self
                    .archive
                    .mcp_get_last_interaction(&req.jid)
                    .map_err(|e| e.to_string());
                let _ = reply.send(res);
            }
            McpCommand::ListMessages { req, reply } => {
                let res = self
                    .archive
                    .mcp_list_messages(
                        req.after,
                        req.before,
                        req.sender_phone_number.as_deref(),
                        req.chat_jid.as_deref(),
                        req.query.as_deref(),
                        req.limit,
                        req.page,
                    )
                    .map_err(|e| e.to_string());
                let _ = reply.send(res);
            }
            McpCommand::GetMessageContext { req, reply } => {
                let res = self
                    .archive
                    .mcp_get_message_context(
                        &req.message_id,
                        req.chat_jid.as_deref(),
                        req.before,
                        req.after,
                    )
                    .map_err(|e| e.to_string());
                let _ = reply.send(res);
            }
            McpCommand::Send { req, reply } => {
                if self.connected_client().is_none() {
                    let _ = reply.send(SendMessageResponse {
                        success: false,
                        message: "ZapFast is not connected to WhatsApp".to_owned(),
                    });
                    return;
                }
                let Some(chat) = self.resolve_recipient_chat(&req.recipient) else {
                    let _ = reply.send(SendMessageResponse {
                        success: false,
                        message: format!("Invalid recipient: {}", req.recipient),
                    });
                    return;
                };
                if req.message.is_empty() && req.media_path.is_empty() {
                    let _ = reply.send(SendMessageResponse {
                        success: false,
                        message: "Message or media_path is required".to_owned(),
                    });
                    return;
                }
                let fallback_name = self.archive.mcp_sender_name(&chat, None);
                let _ = self.archive.ensure_chat(&chat, &fallback_name);

                if !req.media_path.is_empty() {
                    let media_path = PathBuf::from(&req.media_path);
                    if !media_path.is_file() {
                        let _ = reply.send(SendMessageResponse {
                            success: false,
                            message: format!("Media file not found: {}", req.media_path),
                        });
                        return;
                    }
                    let is_ogg = media_path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("ogg"));
                    if (req.is_voice || (is_ogg && req.message.is_empty()))
                        && let Ok(bytes) = std::fs::read(&media_path)
                        && let Ok(samples) = crate::voice::decode(&bytes)
                    {
                        self.mcp_sends.entry(chat.clone()).or_default().push(reply);
                        self.send_voice(chat, samples, None);
                        return;
                    }
                    let caption = (!req.message.is_empty()).then_some(req.message);
                    self.mcp_sends.entry(chat.clone()).or_default().push(reply);
                    self.send_files(
                        chat,
                        vec![media_path],
                        FileSendOptions {
                            documents: false,
                            view_once: false,
                        },
                        caption,
                        Vec::new(),
                        None,
                    );
                } else {
                    self.mcp_sends.entry(chat.clone()).or_default().push(reply);
                    self.send_text(chat, req.message, None, Vec::new(), None);
                }
            }
            McpCommand::Download { req, reply } => {
                let mut chat = if !req.chat_jid.trim().is_empty() {
                    self.resolve_recipient_chat(&req.chat_jid)
                        .unwrap_or_else(|| req.chat_jid.clone())
                } else {
                    String::new()
                };
                if (chat.is_empty()
                    || self
                        .archive
                        .message(&chat, &req.message_id)
                        .ok()
                        .flatten()
                        .is_none())
                    && let Ok(Some(found_chat)) =
                        self.archive.mcp_find_message_chat(&req.message_id)
                {
                    chat = found_chat;
                }
                if chat.is_empty() {
                    let _ = reply.send(DownloadMediaResponse {
                        success: false,
                        message: format!("Message {} not found in archive", req.message_id),
                        filename: None,
                        path: None,
                    });
                    return;
                }

                // Return cached path immediately if already downloaded by ZapFast!
                if let Ok(Some(msg)) = self.archive.message(&chat, &req.message_id)
                    && let Some(media) = msg.content.media()
                    && let Some(existing_path) = &media.path
                    && existing_path.is_file()
                {
                    let filename = existing_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned());
                    let path_str = existing_path.to_string_lossy().into_owned();
                    let _ = reply.send(DownloadMediaResponse {
                        success: true,
                        message: "Media already cached locally".to_owned(),
                        filename,
                        path: Some(path_str),
                    });
                    return;
                }

                if self.connected_client().is_none() {
                    let _ = reply.send(DownloadMediaResponse {
                        success: false,
                        message: "ZapFast is not connected to WhatsApp".to_owned(),
                        filename: None,
                        path: None,
                    });
                    return;
                }

                self.mcp_downloads
                    .entry((chat.clone(), req.message_id.clone()))
                    .or_default()
                    .push(reply);
                self.download_media(chat, req.message_id, None, true);
            }
        }
    }

    pub(super) fn notify_mcp_sent(&mut self, chat: &str, error: Option<&str>) {
        let Some(waiters) = self.mcp_sends.get_mut(chat) else {
            return;
        };
        if let Some(reply) = waiters.pop() {
            let response = match error {
                Some(err) => SendMessageResponse {
                    success: false,
                    message: format!("Failed to send message: {err}"),
                },
                None => SendMessageResponse {
                    success: true,
                    message: format!("Message sent to {chat}"),
                },
            };
            let _ = reply.send(response);
        }
        if waiters.is_empty() {
            self.mcp_sends.remove(chat);
        }
    }

    pub(super) fn notify_mcp_downloaded(
        &mut self,
        chat: &str,
        id: &str,
        result: &Result<PathBuf, String>,
    ) {
        let Some(waiters) = self.mcp_downloads.remove(&(chat.to_owned(), id.to_owned())) else {
            return;
        };
        for reply in waiters {
            let response = match result {
                Ok(path) => {
                    let filename = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    DownloadMediaResponse {
                        success: true,
                        message: "Successfully downloaded media".to_owned(),
                        filename,
                        path: Some(path.to_string_lossy().into_owned()),
                    }
                }
                Err(err) => DownloadMediaResponse {
                    success: false,
                    message: format!("Failed to download media: {err}"),
                    filename: None,
                    path: None,
                },
            };
            let _ = reply.send(response);
        }
    }
}
