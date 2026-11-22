//! Localhost HTTP + SSE bridge (127.0.0.1:47120) exposing Archive reads, real-time events, and Worker actions.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{FileSendOptions, Worker};
use crate::archive::{
    McpChat, McpContact, McpGroupInfo, McpMessage, McpMessageContext, mcp_describe_content,
};
use crate::backend::{Command, Event};
use crate::model::{ChatActivity, PollDraft, ReplyTarget};
use crate::paths::AppDirs;

const DEFAULT_MCP_PORT: u16 = 47_120;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

static EVENT_BUS: LazyLock<broadcast::Sender<McpSseEvent>> =
    LazyLock::new(|| broadcast::channel(512).0);

#[derive(Clone, Debug, Serialize)]
pub(super) struct McpSseEvent {
    pub event: String,
    pub account_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_pn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_jid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_archived: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_muted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_jid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_me: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_path: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

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
    GetGroupInfo {
        req: GetGroupInfoRequest,
        reply: oneshot::Sender<Result<Option<McpGroupInfo>, String>>,
    },
    QuerySql {
        req: QuerySqlRequest,
        reply: oneshot::Sender<Result<Vec<serde_json::Value>, String>>,
    },
    Send {
        req: SendMessageRequest,
        reply: oneshot::Sender<SendMessageResponse>,
    },
    React {
        req: ReactRequest,
        reply: oneshot::Sender<ActionResponse>,
    },
    Edit {
        req: EditMessageRequest,
        reply: oneshot::Sender<ActionResponse>,
    },
    Revoke {
        req: RevokeMessageRequest,
        reply: oneshot::Sender<ActionResponse>,
    },
    ChatAction {
        req: ChatActionRequest,
        reply: oneshot::Sender<ActionResponse>,
    },
    FetchOlder {
        req: FetchOlderRequest,
        reply: oneshot::Sender<ActionResponse>,
    },
    CreatePoll {
        req: CreatePollRequest,
        reply: oneshot::Sender<ActionResponse>,
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
    #[serde(default)]
    pub archived: Option<bool>,
    #[serde(default)]
    pub unread_only: bool,
    #[serde(default)]
    pub chat_type: Option<String>,
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
pub(super) struct GetGroupInfoRequest {
    pub chat_jid: String,
}

#[derive(Deserialize)]
pub(super) struct QuerySqlRequest {
    pub sql: String,
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
    #[serde(default)]
    pub as_document: bool,
    #[serde(default)]
    pub view_once: bool,
    #[serde(default, alias = "reply_to_message_id")]
    pub reply_to: Option<String>,
    #[serde(default, deserialize_with = "null_to_default")]
    pub mentions: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct SendMessageResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Serialize)]
pub(super) struct ActionResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Deserialize)]
pub(super) struct ReactRequest {
    #[serde(default)]
    pub chat_jid: Option<String>,
    pub message_id: String,
    #[serde(default)]
    pub emoji: String,
}

#[derive(Deserialize)]
pub(super) struct EditMessageRequest {
    #[serde(default)]
    pub chat_jid: Option<String>,
    pub message_id: String,
    pub text: String,
    #[serde(default, deserialize_with = "null_to_default")]
    pub mentions: Vec<String>,
}

#[derive(Deserialize)]
pub(super) struct RevokeMessageRequest {
    #[serde(default)]
    pub chat_jid: Option<String>,
    pub message_id: String,
}

#[derive(Deserialize)]
pub(super) struct ChatActionRequest {
    pub chat_jid: String,
    pub action: String,
    #[serde(default)]
    pub mute_until: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct FetchOlderRequest {
    pub chat_jid: String,
}

#[derive(Deserialize)]
pub(super) struct CreatePollRequest {
    pub chat_jid: String,
    pub question: String,
    pub options: Vec<String>,
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Deserialize)]
pub(super) struct DownloadMediaRequest {
    pub message_id: String,
    #[serde(default, deserialize_with = "null_to_default")]
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

fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
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
    let method = parts.next().unwrap_or("").to_owned();
    let raw_path = parts.next().unwrap_or("").to_owned();

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

    let base_path = raw_path.split('?').next().unwrap_or(&raw_path);
    if method == "GET" && base_path == "/api/events" {
        return serve_sse_stream(stream, &raw_path).await;
    }

    let (status, payload) = route_request(&method, &raw_path, &body).await;
    let status_text = match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        404 => "404 Not Found",
        405 => "405 Method Not Allowed",
        _ => "500 Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status_text}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

fn url_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let raw = input.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'+' {
            bytes.push(b' ');
            i += 1;
        } else if raw[i] == b'%'
            && i + 2 < raw.len()
            && let Ok(hex_str) = std::str::from_utf8(&raw[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex_str, 16)
        {
            bytes.push(val);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[derive(Default)]
struct SseFilters {
    account: Option<String>,
    chat: Option<String>,
    sender: Option<String>,
    types: Vec<String>,
    archived: Option<bool>,
    from_me: Option<bool>,
}

impl SseFilters {
    fn from_raw_path(raw_path: &str) -> Self {
        let mut f = Self::default();
        let Some((_, query)) = raw_path.split_once('?') else {
            return f;
        };
        for pair in query.split('&') {
            let Some((k, raw_v)) = pair.split_once('=') else {
                continue;
            };
            let v = url_decode(raw_v).trim().to_owned();
            if v.is_empty() {
                continue;
            }
            match k {
                "account" => f.account = Some(v.to_ascii_lowercase()),
                "chat" => f.chat = Some(v.to_ascii_lowercase()),
                "sender" => f.sender = Some(v.to_ascii_lowercase()),
                "type" | "types" | "event" => {
                    for item in v.split(',') {
                        let t = item.trim().to_ascii_lowercase();
                        if !t.is_empty() && t != "all" {
                            f.types.push(t);
                        }
                    }
                }
                "archived" => {
                    f.archived = match v.to_ascii_lowercase().as_str() {
                        "1" | "true" | "yes" => Some(true),
                        "0" | "false" | "no" => Some(false),
                        _ => None,
                    };
                }
                "from_me" | "include_from_me" => {
                    f.from_me = match v.to_ascii_lowercase().as_str() {
                        "1" | "true" | "yes" => Some(true),
                        "0" | "false" | "no" => Some(false),
                        _ => None,
                    };
                }
                _ => {}
            }
        }
        f
    }

    fn matches(&self, ev: &McpSseEvent) -> bool {
        if !self.types.is_empty() && !self.types.iter().any(|t| t == &ev.event) {
            return false;
        }
        if let Some(acct) = &self.account {
            let id_match = ev.account_id.to_ascii_lowercase() == *acct;
            let pn_match = ev
                .account_pn
                .as_deref()
                .is_some_and(|p| p.to_ascii_lowercase().contains(acct));
            if !id_match && !pn_match {
                return false;
            }
        }
        if let Some(arch) = self.archived
            && ev.is_archived.is_some_and(|a| a != arch)
        {
            return false;
        }
        if let Some(fm) = self.from_me
            && ev.from_me.is_some_and(|f| f != fm)
        {
            return false;
        }
        if let Some(chat_filter) = &self.chat {
            let jid_ok = ev
                .chat_jid
                .as_deref()
                .is_some_and(|j| j.to_ascii_lowercase().contains(chat_filter));
            let name_ok = ev
                .chat_name
                .as_deref()
                .is_some_and(|n| n.to_ascii_lowercase().contains(chat_filter));
            if !jid_ok && !name_ok {
                return false;
            }
        }
        if let Some(sender_filter) = &self.sender {
            let jid_ok = ev
                .sender_jid
                .as_deref()
                .is_some_and(|j| j.to_ascii_lowercase().contains(sender_filter));
            let name_ok = ev
                .sender_name
                .as_deref()
                .is_some_and(|n| n.to_ascii_lowercase().contains(sender_filter));
            if !jid_ok && !name_ok {
                return false;
            }
        }
        true
    }
}

async fn serve_sse_stream(mut stream: TcpStream, raw_path: &str) -> std::io::Result<()> {
    let filters = SseFilters::from_raw_path(raw_path);
    let mut rx = EVENT_BUS.subscribe();
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\nevent: ready\ndata: {\"status\":\"listening\"}\n\n";
    stream.write_all(headers.as_bytes()).await?;
    stream.flush().await?;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.tick().await;
    loop {
        tokio::select! {
            res = rx.recv() => {
                match res {
                    Ok(ev) => {
                        if !filters.matches(&ev) {
                            continue;
                        }
                        if let Ok(json) = serde_json::to_string(&ev) {
                            let frame = format!("event: {}\ndata: {json}\n\n", ev.event);
                            if stream.write_all(frame.as_bytes()).await.is_err()
                                || stream.flush().await.is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = heartbeat.tick() => {
                if stream.write_all(b": keepalive\n\n").await.is_err()
                    || stream.flush().await.is_err()
                {
                    break;
                }
            }
        }
    }
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
        "/api/group-info" => {
            let Ok(req) = serde_json::from_slice::<GetGroupInfoRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::GetGroupInfo { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(Ok(info)) => json_ok(&info),
                Ok(Err(err)) => json_error(500, err),
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/query" => {
            let Ok(req) = serde_json::from_slice::<QuerySqlRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::QuerySql { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(Ok(rows)) => json_ok(&rows),
                Ok(Err(err)) => json_error(400, err),
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
        "/api/react" => {
            let Ok(req) = serde_json::from_slice::<ReactRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::React { req, reply }).is_err() {
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
        "/api/edit" => {
            let Ok(req) = serde_json::from_slice::<EditMessageRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::Edit { req, reply }).is_err() {
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
        "/api/revoke" => {
            let Ok(req) = serde_json::from_slice::<RevokeMessageRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::Revoke { req, reply }).is_err() {
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
        "/api/chat-action" => {
            let Ok(req) = serde_json::from_slice::<ChatActionRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::ChatAction { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(res) => {
                    let status = if res.success { 200 } else { 400 };
                    (status, serde_json::to_vec(&res).unwrap_or_default())
                }
                Err(_) => json_error(500, "Worker dropped request"),
            }
        }
        "/api/fetch-older" => {
            let Ok(req) = serde_json::from_slice::<FetchOlderRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::FetchOlder { req, reply }).is_err() {
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
        "/api/poll" => {
            let Ok(req) = serde_json::from_slice::<CreatePollRequest>(body) else {
                return json_error(400, "Invalid request body");
            };
            let (reply, rx) = oneshot::channel();
            if tx.send(McpCommand::CreatePoll { req, reply }).is_err() {
                return json_error(500, "Worker unavailable");
            }
            match rx.await {
                Ok(res) => {
                    let status = if res.success { 200 } else { 400 };
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
                        req.archived,
                        req.unread_only,
                        req.chat_type.as_deref(),
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
            McpCommand::GetGroupInfo { req, reply } => {
                let res = self
                    .archive
                    .mcp_get_group_info(&req.chat_jid)
                    .map_err(|e| e.to_string());
                let _ = reply.send(res);
            }
            McpCommand::QuerySql { req, reply } => {
                let res = self.archive.mcp_query_readonly(&req.sql);
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

                let quoting = req
                    .reply_to
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|id| ReplyTarget {
                        id: id.to_owned(),
                        chat: Some(chat.clone()),
                    });
                let mentions: Vec<String> = req
                    .mentions
                    .iter()
                    .filter_map(|m| self.resolve_recipient_chat(m))
                    .collect();

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
                    if (req.is_voice || (is_ogg && req.message.is_empty() && !req.as_document))
                        && let Ok(bytes) = std::fs::read(&media_path)
                        && let Ok(samples) = crate::voice::decode(&bytes)
                    {
                        self.mcp_sends.entry(chat.clone()).or_default().push(reply);
                        self.send_voice(chat, samples, quoting);
                        return;
                    }
                    let caption = (!req.message.is_empty()).then_some(req.message);
                    self.mcp_sends.entry(chat.clone()).or_default().push(reply);
                    self.send_files(
                        chat,
                        vec![media_path],
                        FileSendOptions {
                            documents: req.as_document,
                            view_once: req.view_once,
                        },
                        caption,
                        mentions,
                        quoting,
                    );
                } else {
                    self.mcp_sends.entry(chat.clone()).or_default().push(reply);
                    self.send_text(chat, req.message, quoting, mentions, None);
                }
            }
            McpCommand::React { req, reply } => {
                let chat = self.resolve_chat_for_message(req.chat_jid.as_deref(), &req.message_id);
                let Some(chat) = chat else {
                    let _ = reply.send(ActionResponse {
                        success: false,
                        message: format!("Could not find chat for message {}", req.message_id),
                    });
                    return;
                };
                let _ = self.commands.send(Command::React {
                    chat: chat.clone(),
                    message: req.message_id.clone(),
                    emoji: req.emoji.clone(),
                });
                let _ = reply.send(ActionResponse {
                    success: true,
                    message: format!("Reaction '{}' queued on {}", req.emoji, req.message_id),
                });
            }
            McpCommand::Edit { req, reply } => {
                let chat = self.resolve_chat_for_message(req.chat_jid.as_deref(), &req.message_id);
                let Some(chat) = chat else {
                    let _ = reply.send(ActionResponse {
                        success: false,
                        message: format!("Could not find chat for message {}", req.message_id),
                    });
                    return;
                };
                let mentions: Vec<String> = req
                    .mentions
                    .iter()
                    .filter_map(|m| self.resolve_recipient_chat(m))
                    .collect();
                let _ = self.commands.send(Command::EditText {
                    chat,
                    id: req.message_id.clone(),
                    text: req.text,
                    mentions,
                });
                let _ = reply.send(ActionResponse {
                    success: true,
                    message: format!("Edit queued for message {}", req.message_id),
                });
            }
            McpCommand::Revoke { req, reply } => {
                let chat = self.resolve_chat_for_message(req.chat_jid.as_deref(), &req.message_id);
                let Some(chat) = chat else {
                    let _ = reply.send(ActionResponse {
                        success: false,
                        message: format!("Could not find chat for message {}", req.message_id),
                    });
                    return;
                };
                let _ = self.commands.send(Command::Revoke {
                    chat,
                    id: req.message_id.clone(),
                });
                let _ = reply.send(ActionResponse {
                    success: true,
                    message: format!("Revoke queued for message {}", req.message_id),
                });
            }
            McpCommand::ChatAction { req, reply } => {
                let Some(chat) = self.resolve_recipient_chat(&req.chat_jid) else {
                    let _ = reply.send(ActionResponse {
                        success: false,
                        message: format!("Invalid chat_jid: {}", req.chat_jid),
                    });
                    return;
                };
                let action = req.action.trim().to_ascii_lowercase();
                let cmd = match action.as_str() {
                    "typing" | "composing" => Command::Composing {
                        chat: chat.clone(),
                        composing: true,
                    },
                    "stop_typing" | "paused" => Command::Composing {
                        chat: chat.clone(),
                        composing: false,
                    },
                    "mark_read" | "read" => Command::MarkRead {
                        chat: chat.clone(),
                        receipts: true,
                    },
                    "mark_unread" | "unread" => Command::MarkUnread(chat.clone()),
                    "archive" => Command::SetArchived(chat.clone(), true),
                    "unarchive" => Command::SetArchived(chat.clone(), false),
                    "pin" => Command::SetPinned(chat.clone(), true),
                    "unpin" => Command::SetPinned(chat.clone(), false),
                    "mute" => Command::SetMuted(chat.clone(), Some(req.mute_until.unwrap_or(0))),
                    "unmute" => Command::SetMuted(chat.clone(), None),
                    other => {
                        let _ = reply.send(ActionResponse {
                            success: false,
                            message: format!(
                                "Unknown chat action '{other}'. Supported: typing, stop_typing, mark_read, mark_unread, archive, unarchive, pin, unpin, mute, unmute"
                            ),
                        });
                        return;
                    }
                };
                let _ = self.commands.send(cmd);
                let _ = reply.send(ActionResponse {
                    success: true,
                    message: format!("Action '{action}' executed on {chat}"),
                });
            }
            McpCommand::FetchOlder { req, reply } => {
                let Some(chat) = self.resolve_recipient_chat(&req.chat_jid) else {
                    let _ = reply.send(ActionResponse {
                        success: false,
                        message: format!("Invalid chat_jid: {}", req.chat_jid),
                    });
                    return;
                };
                let _ = self.commands.send(Command::FetchOlder {
                    chat: chat.clone(),
                    explicit: true,
                });
                let _ = reply.send(ActionResponse {
                    success: true,
                    message: format!("Requested older history from phone for {chat}"),
                });
            }
            McpCommand::CreatePoll { req, reply } => {
                let Some(chat) = self.resolve_recipient_chat(&req.chat_jid) else {
                    let _ = reply.send(ActionResponse {
                        success: false,
                        message: format!("Invalid chat_jid: {}", req.chat_jid),
                    });
                    return;
                };
                if req.question.trim().is_empty() || req.options.len() < 2 {
                    let _ = reply.send(ActionResponse {
                        success: false,
                        message: "Poll requires a question and at least 2 options".to_owned(),
                    });
                    return;
                }
                let _ = self.commands.send(Command::CreatePoll {
                    chat: chat.clone(),
                    draft: PollDraft {
                        question: req.question,
                        options: req.options,
                        multiple: req.multiple,
                    },
                });
                let _ = reply.send(ActionResponse {
                    success: true,
                    message: format!("Poll creation queued for {chat}"),
                });
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

    fn resolve_chat_for_message(
        &self,
        chat_hint: Option<&str>,
        message_id: &str,
    ) -> Option<String> {
        if let Some(hint) = chat_hint.map(str::trim).filter(|s| !s.is_empty())
            && let Some(resolved) = self.resolve_recipient_chat(hint)
            && self
                .archive
                .message(&resolved, message_id)
                .ok()
                .flatten()
                .is_some()
        {
            return Some(resolved);
        }
        if let Ok(Some(found)) = self.archive.mcp_find_message_chat(message_id) {
            return Some(found);
        }
        chat_hint
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|h| self.resolve_recipient_chat(h))
    }

    pub(super) fn publish_mcp_event(&self, event: &Event) {
        if EVENT_BUS.receiver_count() == 0 {
            return;
        }
        let account_id = self.dirs.id.as_str().to_owned();
        let account_pn = self.me_pn.clone();
        let chat_meta = |chat_id: &str| {
            let chat_row = self.archive.chat(chat_id).ok().flatten();
            let name = chat_row
                .as_ref()
                .map(|c| c.name.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| Some(self.archive.mcp_sender_name(chat_id, None)));
            let kind = Some(
                if chat_id.ends_with("@g.us") {
                    "group"
                } else if chat_id.ends_with("@newsletter") {
                    "channel"
                } else {
                    "direct"
                }
                .to_owned(),
            );
            let is_archived = chat_row.as_ref().map(|c| c.archived);
            let now_ts = crate::util::now();
            let is_muted = chat_row
                .as_ref()
                .map(|c| c.muted_until.is_some_and(|m| m <= 0 || m > now_ts));
            (name, kind, is_archived, is_muted)
        };

        let sse = match event {
            Event::Incoming { chat, message } => {
                let (chat_name, kind, is_archived, is_muted) = chat_meta(chat);
                let (text, media_type, media_path) =
                    mcp_describe_content(&message.content, message.quoted.as_ref());
                let sender_name = if message.from_me {
                    Some("Me".to_owned())
                } else {
                    Some(
                        self.archive
                            .mcp_sender_name(&message.sender, message.sender_name.clone()),
                    )
                };
                let mentions = message.mentions.iter().map(|m| m.id.clone()).collect();
                McpSseEvent {
                    event: "incoming".to_owned(),
                    account_id,
                    account_pn,
                    chat_jid: Some(chat.clone()),
                    chat_name,
                    kind,
                    is_archived,
                    is_muted,
                    message_id: Some(message.id.clone()),
                    sender_jid: Some(message.sender.clone()),
                    sender_name,
                    from_me: Some(message.from_me),
                    timestamp: Some(message.timestamp),
                    text: Some(text),
                    media_type,
                    media_path,
                    mentions,
                    detail: None,
                }
            }
            Event::MessageUpdated(message) => {
                let (chat_name, kind, is_archived, is_muted) = chat_meta(&message.chat);
                let (text, media_type, media_path) =
                    mcp_describe_content(&message.content, message.quoted.as_ref());
                let sender_name = if message.from_me {
                    Some("Me".to_owned())
                } else {
                    Some(
                        self.archive
                            .mcp_sender_name(&message.sender, message.sender_name.clone()),
                    )
                };
                let mentions = message.mentions.iter().map(|m| m.id.clone()).collect();
                McpSseEvent {
                    event: "message_updated".to_owned(),
                    account_id,
                    account_pn,
                    chat_jid: Some(message.chat.clone()),
                    chat_name,
                    kind,
                    is_archived,
                    is_muted,
                    message_id: Some(message.id.clone()),
                    sender_jid: Some(message.sender.clone()),
                    sender_name,
                    from_me: Some(message.from_me),
                    timestamp: Some(message.timestamp),
                    text: Some(text),
                    media_type,
                    media_path,
                    mentions,
                    detail: Some(if message.edited { "edited" } else { "updated" }.to_owned()),
                }
            }
            Event::MessageDeleted { chat, id } => {
                let (chat_name, kind, is_archived, is_muted) = chat_meta(chat);
                McpSseEvent {
                    event: "message_deleted".to_owned(),
                    account_id,
                    account_pn,
                    chat_jid: Some(chat.clone()),
                    chat_name,
                    kind,
                    is_archived,
                    is_muted,
                    message_id: Some(id.clone()),
                    sender_jid: None,
                    sender_name: None,
                    from_me: None,
                    timestamp: Some(crate::util::now()),
                    text: None,
                    media_type: None,
                    media_path: None,
                    mentions: Vec::new(),
                    detail: None,
                }
            }
            Event::Typing {
                chat,
                sender,
                activity,
            } => {
                let (chat_name, kind, is_archived, is_muted) = chat_meta(chat);
                let sender_name = Some(self.archive.mcp_sender_name(sender, None));
                let detail = match activity {
                    Some(ChatActivity::Typing) => "typing",
                    Some(ChatActivity::RecordingAudio) => "recording",
                    None => "stopped",
                };
                McpSseEvent {
                    event: "typing".to_owned(),
                    account_id,
                    account_pn,
                    chat_jid: Some(chat.clone()),
                    chat_name,
                    kind,
                    is_archived,
                    is_muted,
                    message_id: None,
                    sender_jid: Some(sender.clone()),
                    sender_name,
                    from_me: Some(false),
                    timestamp: Some(crate::util::now()),
                    text: None,
                    media_type: None,
                    media_path: None,
                    mentions: Vec::new(),
                    detail: Some(detail.to_owned()),
                }
            }
            Event::Presence {
                id,
                online,
                last_seen,
            } => {
                let sender_name = Some(self.archive.mcp_sender_name(id, None));
                McpSseEvent {
                    event: "presence".to_owned(),
                    account_id,
                    account_pn,
                    chat_jid: Some(id.clone()),
                    chat_name: sender_name.clone(),
                    kind: Some("direct".to_owned()),
                    is_archived: None,
                    is_muted: None,
                    message_id: None,
                    sender_jid: Some(id.clone()),
                    sender_name,
                    from_me: None,
                    timestamp: (*last_seen).or_else(|| Some(crate::util::now())),
                    text: None,
                    media_type: None,
                    media_path: None,
                    mentions: Vec::new(),
                    detail: Some(if *online { "online" } else { "offline" }.to_owned()),
                }
            }
            Event::Call(update) | Event::PhoneOnlyCall(update) => {
                let (chat_name, kind, is_archived, is_muted) = chat_meta(&update.chat);
                let detail = format!(
                    "{}:{:?}",
                    if update.video { "video" } else { "voice" },
                    update.phase
                )
                .to_ascii_lowercase();
                McpSseEvent {
                    event: "call".to_owned(),
                    account_id,
                    account_pn,
                    chat_jid: Some(update.chat.clone()),
                    chat_name,
                    kind,
                    is_archived,
                    is_muted,
                    message_id: None,
                    sender_jid: Some(update.chat.clone()),
                    sender_name: None,
                    from_me: None,
                    timestamp: Some(crate::util::now()),
                    text: None,
                    media_type: None,
                    media_path: None,
                    mentions: Vec::new(),
                    detail: Some(detail),
                }
            }
            Event::Link(status) => McpSseEvent {
                event: "status".to_owned(),
                account_id,
                account_pn,
                chat_jid: None,
                chat_name: None,
                kind: None,
                is_archived: None,
                is_muted: None,
                message_id: None,
                sender_jid: None,
                sender_name: None,
                from_me: None,
                timestamp: Some(crate::util::now()),
                text: None,
                media_type: None,
                media_path: None,
                mentions: Vec::new(),
                detail: Some(status.log_label().to_owned()),
            },
            _ => return,
        };
        let _ = EVENT_BUS.send(sse);
    }
}
