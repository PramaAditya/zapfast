//! Localhost HTTP bridge (127.0.0.1:47120) exposing Archive reads and Worker actions for whatsapp-mcp.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

use super::{FileSendOptions, Worker};
use crate::archive::{McpChat, McpContact, McpMessage, McpMessageContext};

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

#[derive(Serialize)]
pub(super) struct StatusResponse {
    pub connected: bool,
    pub status: String,
    pub me_pn: Option<String>,
    pub me_name: Option<String>,
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

pub(super) fn spawn_server() -> mpsc::UnboundedReceiver<McpCommand> {
    let (tx, rx) = mpsc::unbounded_channel();
    #[cfg(test)]
    if std::env::var_os("ZAPFAST_MCP_PORT").is_none() {
        return rx;
    }
    let port = std::env::var("ZAPFAST_MCP_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(DEFAULT_MCP_PORT);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    tokio::spawn(async move {
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
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, tx).await;
            });
        }
    });
    rx
}

async fn handle_connection(
    stream: TcpStream,
    tx: mpsc::UnboundedSender<McpCommand>,
) -> std::io::Result<()> {
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

    let (status, payload) = route_request(method, path, &body, &tx).await;
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

async fn route_request(
    method: &str,
    path: &str,
    body: &[u8],
    tx: &mpsc::UnboundedSender<McpCommand>,
) -> (u16, Vec<u8>) {
    if method == "GET" && (path == "/api/status" || path == "/health") {
        let (reply, rx) = oneshot::channel();
        if tx.send(McpCommand::Status { reply }).is_err() {
            return json_error(500, "Worker unavailable");
        }
        return match rx.await {
            Ok(res) => json_ok(&res),
            Err(_) => json_error(500, "Worker dropped request"),
        };
    }

    if method != "POST" {
        return json_error(405, "Method not allowed");
    }

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
                    connected: self.status.is_connected(),
                    status: self.status.log_label().to_owned(),
                    me_pn: self.me_pn.clone(),
                    me_name: self.me_name.clone(),
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
