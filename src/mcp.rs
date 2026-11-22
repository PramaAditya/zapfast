//! Native stdio Model Context Protocol (MCP) server (`zapfast mcp`).
//!
//! Speaks JSON-RPC 2.0 over standard input and standard output with zero
//! external protocol dependencies. Read queries use the live localhost bridge
//! (`127.0.0.1:47120`) when the desktop app is running and fall back to opening
//! the encrypted SQLCipher `archive.db` in query-only mode via the OS keyring
//! when the desktop app is closed. Write and media-download actions auto-launch
//! `zapfast --start-hidden` if the desktop process is not yet running.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use jiff::{Timestamp, Zoned, civil};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::archive::{Archive, McpChat, McpMessage, McpMessageContext};
use crate::model::AccountId;
use crate::paths::AppDirs;

const DEFAULT_MCP_PORT: u16 = 47120;

fn bridge_addr() -> SocketAddr {
    let port = std::env::var("ZAPFAST_MCP_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(DEFAULT_MCP_PORT);
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn format_unix_iso(ts: i64) -> String {
    if let Ok(stamp) = Timestamp::from_second(ts) {
        let zoned = stamp.to_zoned(jiff::tz::TimeZone::system());
        return zoned.strftime("%Y-%m-%dT%H:%M:%S").to_string();
    }
    ts.to_string()
}

fn format_unix_space(ts: i64) -> String {
    if let Ok(stamp) = Timestamp::from_second(ts) {
        let zoned = stamp.to_zoned(jiff::tz::TimeZone::system());
        return zoned.strftime("%Y-%m-%d %H:%M:%S").to_string();
    }
    ts.to_string()
}

fn parse_iso_to_unix(input: &str) -> Result<i64, String> {
    let trimmed = input.trim();
    if let Ok(ts) = trimmed.parse::<Timestamp>() {
        return Ok(ts.as_second());
    }
    if let Ok(zoned) = trimmed.parse::<Zoned>() {
        return Ok(zoned.timestamp().as_second());
    }
    if let Ok(dt) = trimmed.parse::<civil::DateTime>()
        && let Ok(zoned) = dt.to_zoned(jiff::tz::TimeZone::system())
    {
        return Ok(zoned.timestamp().as_second());
    }
    if let Ok(date) = trimmed.parse::<civil::Date>()
        && let Ok(zoned) = date.at(0, 0, 0, 0).to_zoned(jiff::tz::TimeZone::system())
    {
        return Ok(zoned.timestamp().as_second());
    }
    Err(format!(
        "Invalid date format: '{trimmed}'. Please use ISO-8601 format (e.g. 2026-11-13 or 2026-11-13T08:30:00)."
    ))
}

fn format_mcp_message_line(msg: &McpMessage, show_chat_info: bool) -> String {
    let ts_str = format_unix_space(msg.timestamp);
    let mut out = if show_chat_info {
        if let Some(chat_name) = msg.chat_name.as_deref().filter(|s| !s.is_empty()) {
            format!("[{ts_str}] Chat: {chat_name} ")
        } else {
            format!("[{ts_str}] ")
        }
    } else {
        format!("[{ts_str}] ")
    };

    let prefix = if let Some(media_type) = msg.media_type.as_deref().filter(|s| !s.is_empty()) {
        format!(
            "[{media_type} - Message ID: {} - Chat JID: {}] ",
            msg.id, msg.chat_jid
        )
    } else {
        String::new()
    };

    let sender_name = if msg.is_from_me {
        "Me"
    } else {
        msg.sender_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&msg.sender)
    };
    out.push_str(&format!("From: {sender_name}: {prefix}{}\n", msg.content));
    out
}

fn format_mcp_messages_list(messages: &[McpMessage], show_chat_info: bool) -> String {
    if messages.is_empty() {
        return "No messages to display.".to_owned();
    }
    let mut out = String::new();
    for msg in messages {
        out.push_str(&format_mcp_message_line(msg, show_chat_info));
    }
    out
}

fn chat_to_json(chat: &McpChat) -> Value {
    json!({
        "jid": chat.jid,
        "name": chat.name,
        "last_message_time": chat.last_message_time.filter(|&t| t > 0).map(format_unix_iso),
        "last_message": chat.last_message,
        "last_sender": chat.last_sender,
        "last_is_from_me": chat.last_is_from_me,
        "is_archived": chat.is_archived,
        "is_pinned": chat.is_pinned,
        "unread_count": chat.unread_count,
        "is_muted": chat.is_muted,
        "is_locked": chat.is_locked,
        "kind": chat.kind,
    })
}

fn message_to_json(msg: &McpMessage) -> Value {
    json!({
        "id": msg.id,
        "chat_jid": msg.chat_jid,
        "chat_name": msg.chat_name,
        "sender": msg.sender,
        "sender_name": msg.sender_name,
        "content": msg.content,
        "timestamp": format_unix_iso(msg.timestamp),
        "is_from_me": msg.is_from_me,
        "media_type": msg.media_type,
        "media_path": msg.media_path,
    })
}

fn context_to_json(ctx: &McpMessageContext) -> Value {
    json!({
        "message": message_to_json(&ctx.message),
        "before": ctx.before.iter().map(message_to_json).collect::<Vec<_>>(),
        "after": ctx.after.iter().map(message_to_json).collect::<Vec<_>>(),
    })
}

fn http_request(
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<(u16, Value), String> {
    let addr = bridge_addr();
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(400))
        .map_err(|e| format!("bridge offline: {e}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let payload = match body {
        Some(val) => serde_json::to_vec(val).map_err(|e| e.to_string())?,
        None => Vec::new(),
    };

    let req_head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream
        .write_all(req_head.as_bytes())
        .and_then(|()| stream.write_all(&payload))
        .and_then(|()| stream.flush())
        .map_err(|e| e.to_string())?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, resp_body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .ok_or_else(|| "invalid HTTP response from bridge".to_owned())?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(500);
    let parsed = serde_json::from_str::<Value>(resp_body.trim())
        .unwrap_or_else(|_| json!({ "raw": resp_body.trim() }));
    Ok((status, parsed))
}

fn ensure_bridge_and_post(
    path: &str,
    body: &Value,
    timeout: Duration,
) -> Result<(u16, Value), String> {
    if let Ok(res) = http_request("POST", path, Some(body), timeout) {
        return Ok(res);
    }

    if let Ok(exe) = std::env::current_exe() {
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--start-hidden")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        let _ = cmd.spawn();

        for _ in 0..35 {
            std::thread::sleep(Duration::from_millis(400));
            if let Ok((200, status_val)) =
                http_request("GET", "/api/status", None, Duration::from_secs(2))
                && status_val
                    .get("connected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            {
                break;
            }
        }
    }

    http_request("POST", path, Some(body), timeout)
}

#[derive(Deserialize)]
struct AccountsRosterFile {
    #[serde(default)]
    active: String,
    #[serde(default)]
    order: Vec<String>,
}

fn digits_only(input: &str) -> String {
    let head = input.split('@').next().unwrap_or("");
    let head = head.split(':').next().unwrap_or("");
    head.chars().filter(char::is_ascii_digit).collect()
}

struct OfflineAccount {
    account_id: String,
    active: bool,
    archive_path: PathBuf,
    me_pn: Option<String>,
    me_lid: Option<String>,
    me_name: Option<String>,
}

struct McpState {
    dirs: AppDirs,
    archives: HashMap<String, Archive>,
}

impl McpState {
    fn new() -> Self {
        Self {
            dirs: AppDirs::discover(),
            archives: HashMap::new(),
        }
    }

    fn read_roster(&self) -> (String, Vec<String>) {
        let mut active = "1".to_owned();
        let mut order = Vec::new();
        let accounts_file = self.dirs.accounts_file();
        if let Ok(bytes) = std::fs::read(&accounts_file)
            && let Ok(parsed) = serde_json::from_slice::<AccountsRosterFile>(&bytes)
        {
            if !parsed.active.trim().is_empty() {
                active = parsed.active.trim().to_owned();
            }
            for item in parsed.order {
                let trimmed = item.trim().to_owned();
                if !trimmed.is_empty() && !order.contains(&trimmed) {
                    order.push(trimmed);
                }
            }
        }

        let accounts_dir = self.dirs.state.join("accounts");
        if let Ok(entries) = std::fs::read_dir(&accounts_dir) {
            let mut found: Vec<String> = entries
                .flatten()
                .filter_map(|e| {
                    let path = e.path();
                    if path.join("archive.db").is_file() {
                        e.file_name().into_string().ok()
                    } else {
                        None
                    }
                })
                .collect();
            found.sort();
            for id in found {
                if !order.contains(&id) {
                    order.push(id);
                }
            }
        }

        if order.is_empty() && self.dirs.archive_db().is_file() {
            order.push("1".to_owned());
        }
        if !order.is_empty() && !order.contains(&active) {
            active = order[0].clone();
        }
        (active, order)
    }

    fn archive_path_for(&self, account_id: &str) -> PathBuf {
        if AccountId::is_safe(account_id) {
            let candidate = self
                .dirs
                .account(&AccountId(account_id.to_owned()))
                .archive_db();
            if candidate.is_file() || account_id != "1" {
                return candidate;
            }
        }
        self.dirs.archive_db()
    }

    fn archive_for_id(&mut self, account_id: &str) -> Result<&Archive, String> {
        if !self.archives.contains_key(account_id) {
            let path = self.archive_path_for(account_id);
            let archive = Archive::mcp_open_readonly(&path)
                .map_err(|e| format!("Failed to open archive at {}: {e:#}", path.display()))?;
            self.archives.insert(account_id.to_owned(), archive);
        }
        self.archives
            .get(account_id)
            .ok_or_else(|| "Archive unavailable".to_owned())
    }

    fn discover_accounts(&mut self) -> Vec<OfflineAccount> {
        let (active_id, order) = self.read_roster();
        let mut out = Vec::new();
        for id in order {
            let path = self.archive_path_for(&id);
            if !path.is_file() {
                continue;
            }
            let (me_pn, me_lid, me_name) = match self.archive_for_id(&id) {
                Ok(arch) => (
                    arch.meta("me_pn").ok().flatten(),
                    arch.meta("me_lid").ok().flatten(),
                    arch.meta("me_name").ok().flatten(),
                ),
                Err(_) => (None, None, None),
            };
            out.push(OfflineAccount {
                active: id == active_id,
                account_id: id,
                archive_path: path,
                me_pn,
                me_lid,
                me_name,
            });
        }
        out
    }

    fn resolve_account_id(&mut self, selector: Option<&str>) -> Result<String, String> {
        let (active_id, order) = self.read_roster();
        let Some(raw) = selector.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(active_id);
        };
        if order.iter().any(|id| id == raw) {
            return Ok(raw.to_owned());
        }
        let needle_digits = digits_only(raw);
        let needle_lower = raw.to_lowercase();
        let accounts = self.discover_accounts();
        for acc in &accounts {
            if acc.account_id == raw {
                return Ok(acc.account_id.clone());
            }
            if let Some(pn) = &acc.me_pn {
                let acc_digits = digits_only(pn);
                if !needle_digits.is_empty()
                    && !acc_digits.is_empty()
                    && (acc_digits == needle_digits || acc_digits.ends_with(&needle_digits))
                {
                    return Ok(acc.account_id.clone());
                }
                if pn.to_lowercase() == needle_lower {
                    return Ok(acc.account_id.clone());
                }
            }
            if let Some(name) = &acc.me_name
                && name.to_lowercase().contains(&needle_lower)
            {
                return Ok(acc.account_id.clone());
            }
        }
        Err(format!("WhatsApp account '{raw}' not found in ZapFast."))
    }

    fn open_selected_archive(&mut self, selector: Option<&str>) -> Result<&Archive, String> {
        let id = self.resolve_account_id(selector)?;
        self.archive_for_id(&id)
    }

    fn list_accounts_json(&mut self) -> Value {
        let mut live_by_id: HashMap<String, Value> = HashMap::new();
        if let Ok((200, st)) = http_request("GET", "/api/status", None, Duration::from_millis(500))
        {
            if let Some(arr) = st.get("accounts").and_then(Value::as_array) {
                for item in arr {
                    if let Some(id) = item.get("account_id").and_then(Value::as_str) {
                        live_by_id.insert(id.to_owned(), item.clone());
                    }
                }
            } else if let Some(id) = st.get("account_id").and_then(Value::as_str) {
                live_by_id.insert(id.to_owned(), st);
            }
        }

        let accounts = self.discover_accounts();
        let list: Vec<Value> = accounts
            .into_iter()
            .map(|acc| {
                let live = live_by_id.get(&acc.account_id);
                let jid = live
                    .and_then(|v| v.get("me_pn"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(acc.me_pn);
                let phone = jid.as_deref().map(digits_only).filter(|s| !s.is_empty());
                let name = live
                    .and_then(|v| v.get("me_name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(acc.me_name);
                let connected = live
                    .and_then(|v| v.get("connected"))
                    .and_then(Value::as_bool);
                let status = live
                    .and_then(|v| v.get("status"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                json!({
                    "account_id": acc.account_id,
                    "active": acc.active,
                    "phone_number": phone,
                    "jid": jid,
                    "lid": acc.me_lid,
                    "name": name,
                    "connected": connected,
                    "status": status,
                    "archive_path": acc.archive_path.to_string_lossy(),
                })
            })
            .collect();
        Value::Array(list)
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn arg_usize(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(default)
}

fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

impl McpState {
    fn call_tool(&mut self, name: &str, args: &Value) -> Result<String, String> {
        let account = arg_str(args, "account");
        match name {
            "list_accounts" => {
                let val = self.list_accounts_json();
                serde_json::to_string_pretty(&val).map_err(|e| e.to_string())
            }
            "search_contacts" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                let arch = self.open_selected_archive(account)?;
                let contacts = arch.mcp_search_contacts(query).map_err(|e| e.to_string())?;
                serde_json::to_string_pretty(&contacts).map_err(|e| e.to_string())
            }
            "list_chats" => {
                let query = arg_str(args, "query");
                let limit = arg_usize(args, "limit", 20);
                let page = arg_usize(args, "page", 0);
                let include_last = arg_bool(args, "include_last_message", true);
                let sort_by = arg_str(args, "sort_by").unwrap_or("last_active");
                let archived = args.get("archived").and_then(Value::as_bool);
                let unread_only = arg_bool(args, "unread_only", false);
                let chat_type = arg_str(args, "chat_type");
                let arch = self.open_selected_archive(account)?;
                let chats = arch
                    .mcp_list_chats(
                        query,
                        limit,
                        page,
                        include_last,
                        sort_by,
                        archived,
                        unread_only,
                        chat_type,
                    )
                    .map_err(|e| e.to_string())?;
                let out: Vec<Value> = chats.iter().map(chat_to_json).collect();
                serde_json::to_string_pretty(&out).map_err(|e| e.to_string())
            }
            "get_chat" => {
                let chat_jid = arg_str(args, "chat_jid")
                    .ok_or_else(|| "Missing required parameter 'chat_jid'".to_owned())?;
                let include_last = arg_bool(args, "include_last_message", true);
                let arch = self.open_selected_archive(account)?;
                let chat = arch
                    .mcp_get_chat(chat_jid, include_last)
                    .map_err(|e| e.to_string())?;
                let val = chat.as_ref().map_or(Value::Null, chat_to_json);
                serde_json::to_string_pretty(&val).map_err(|e| e.to_string())
            }
            "get_direct_chat_by_contact" => {
                let phone = arg_str(args, "sender_phone_number")
                    .ok_or_else(|| "Missing required parameter 'sender_phone_number'".to_owned())?;
                let arch = self.open_selected_archive(account)?;
                let chat = arch
                    .mcp_get_direct_chat_by_contact(phone)
                    .map_err(|e| e.to_string())?;
                let val = chat.as_ref().map_or(Value::Null, chat_to_json);
                serde_json::to_string_pretty(&val).map_err(|e| e.to_string())
            }
            "get_contact_chats" => {
                let jid = arg_str(args, "jid")
                    .ok_or_else(|| "Missing required parameter 'jid'".to_owned())?;
                let limit = arg_usize(args, "limit", 20);
                let page = arg_usize(args, "page", 0);
                let arch = self.open_selected_archive(account)?;
                let chats = arch
                    .mcp_get_contact_chats(jid, limit, page)
                    .map_err(|e| e.to_string())?;
                let out: Vec<Value> = chats.iter().map(chat_to_json).collect();
                serde_json::to_string_pretty(&out).map_err(|e| e.to_string())
            }
            "get_last_interaction" => {
                let jid = arg_str(args, "jid")
                    .ok_or_else(|| "Missing required parameter 'jid'".to_owned())?;
                let arch = self.open_selected_archive(account)?;
                let msg = arch
                    .mcp_get_last_interaction(jid)
                    .map_err(|e| e.to_string())?;
                match msg {
                    Some(m) => Ok(format_mcp_message_line(&m, true)),
                    None => Ok("No messages found for contact.".to_owned()),
                }
            }
            "list_messages" => {
                let after = match arg_str(args, "after") {
                    Some(s) => Some(parse_iso_to_unix(s)?),
                    None => None,
                };
                let before = match arg_str(args, "before") {
                    Some(s) => Some(parse_iso_to_unix(s)?),
                    None => None,
                };
                let sender = arg_str(args, "sender_phone_number");
                let chat_jid = arg_str(args, "chat_jid");
                let query = arg_str(args, "query");
                let limit = arg_usize(args, "limit", 20);
                let page = arg_usize(args, "page", 0);
                let include_context = arg_bool(args, "include_context", true);
                let context_before = arg_usize(args, "context_before", 1);
                let context_after = arg_usize(args, "context_after", 1);

                let arch = self.open_selected_archive(account)?;
                let matches = arch
                    .mcp_list_messages(after, before, sender, chat_jid, query, limit, page)
                    .map_err(|e| e.to_string())?;

                if include_context
                    && !matches.is_empty()
                    && (context_before > 0 || context_after > 0)
                {
                    let mut combined = Vec::new();
                    let mut seen = HashSet::new();
                    for msg in &matches {
                        if let Ok(Some(ctx)) = arch.mcp_get_message_context(
                            &msg.id,
                            Some(&msg.chat_jid),
                            context_before,
                            context_after,
                        ) {
                            for item in ctx
                                .before
                                .into_iter()
                                .chain(std::iter::once(ctx.message))
                                .chain(ctx.after)
                            {
                                if seen.insert((item.chat_jid.clone(), item.id.clone())) {
                                    combined.push(item);
                                }
                            }
                        } else if seen.insert((msg.chat_jid.clone(), msg.id.clone())) {
                            combined.push(msg.clone());
                        }
                    }
                    return Ok(format_mcp_messages_list(&combined, true));
                }
                Ok(format_mcp_messages_list(&matches, true))
            }
            "get_message_context" => {
                let message_id = arg_str(args, "message_id")
                    .ok_or_else(|| "Missing required parameter 'message_id'".to_owned())?;
                let chat_jid = arg_str(args, "chat_jid");
                let before = arg_usize(args, "before", 5);
                let after = arg_usize(args, "after", 5);
                let arch = self.open_selected_archive(account)?;
                let ctx = arch
                    .mcp_get_message_context(message_id, chat_jid, before, after)
                    .map_err(|e| e.to_string())?;
                let val = ctx.as_ref().map_or(Value::Null, context_to_json);
                serde_json::to_string_pretty(&val).map_err(|e| e.to_string())
            }
            "get_group_info" => {
                let chat_jid = arg_str(args, "chat_jid")
                    .ok_or_else(|| "Missing required parameter 'chat_jid'".to_owned())?;
                let arch = self.open_selected_archive(account)?;
                let info = arch
                    .mcp_get_group_info(chat_jid)
                    .map_err(|e| e.to_string())?;
                let val = serde_json::to_value(info).unwrap_or(Value::Null);
                serde_json::to_string_pretty(&val).map_err(|e| e.to_string())
            }
            "query_archive" => {
                let sql = arg_str(args, "sql")
                    .ok_or_else(|| "Missing required parameter 'sql'".to_owned())?;
                let arch = self.open_selected_archive(account)?;
                let rows = arch.mcp_query_readonly(sql)?;
                serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())
            }
            "send_message" => {
                let recipient = arg_str(args, "recipient")
                    .ok_or_else(|| "Missing required parameter 'recipient'".to_owned())?;
                let message = args.get("message").and_then(Value::as_str).unwrap_or("");
                let payload = json!({
                    "recipient": recipient,
                    "message": message,
                    "reply_to": arg_str(args, "reply_to"),
                    "mentions": args.get("mentions").and_then(Value::as_array).cloned().unwrap_or_default(),
                    "account": account,
                });
                let (_, resp) =
                    ensure_bridge_and_post("/api/send", &payload, Duration::from_secs(60))?;
                serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
            }
            "send_file" => {
                let recipient = arg_str(args, "recipient")
                    .ok_or_else(|| "Missing required parameter 'recipient'".to_owned())?;
                let media_path = arg_str(args, "media_path")
                    .ok_or_else(|| "Missing required parameter 'media_path'".to_owned())?;
                let payload = json!({
                    "recipient": recipient,
                    "media_path": media_path,
                    "message": arg_str(args, "caption").unwrap_or(""),
                    "reply_to": arg_str(args, "reply_to"),
                    "as_document": arg_bool(args, "as_document", false),
                    "view_once": arg_bool(args, "view_once", false),
                    "account": account,
                });
                let (_, resp) =
                    ensure_bridge_and_post("/api/send", &payload, Duration::from_secs(120))?;
                serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
            }
            "send_audio_message" => {
                let recipient = arg_str(args, "recipient")
                    .ok_or_else(|| "Missing required parameter 'recipient'".to_owned())?;
                let media_path = arg_str(args, "media_path")
                    .ok_or_else(|| "Missing required parameter 'media_path'".to_owned())?;
                let payload = json!({
                    "recipient": recipient,
                    "media_path": media_path,
                    "is_voice": true,
                    "reply_to": arg_str(args, "reply_to"),
                    "account": account,
                });
                let (_, resp) =
                    ensure_bridge_and_post("/api/send", &payload, Duration::from_secs(120))?;
                serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
            }
            "react_message" => {
                let message_id = arg_str(args, "message_id")
                    .ok_or_else(|| "Missing required parameter 'message_id'".to_owned())?;
                let emoji = args.get("emoji").and_then(Value::as_str).unwrap_or("");
                let payload = json!({
                    "message_id": message_id,
                    "emoji": emoji,
                    "chat_jid": arg_str(args, "chat_jid"),
                    "account": account,
                });
                let (_, resp) =
                    ensure_bridge_and_post("/api/react", &payload, Duration::from_secs(30))?;
                serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
            }
            "edit_message" => {
                let message_id = arg_str(args, "message_id")
                    .ok_or_else(|| "Missing required parameter 'message_id'".to_owned())?;
                let text = arg_str(args, "text")
                    .ok_or_else(|| "Missing required parameter 'text'".to_owned())?;
                let payload = json!({
                    "message_id": message_id,
                    "text": text,
                    "chat_jid": arg_str(args, "chat_jid"),
                    "account": account,
                });
                let (_, resp) =
                    ensure_bridge_and_post("/api/edit", &payload, Duration::from_secs(30))?;
                serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
            }
            "revoke_message" => {
                let message_id = arg_str(args, "message_id")
                    .ok_or_else(|| "Missing required parameter 'message_id'".to_owned())?;
                let payload = json!({
                    "message_id": message_id,
                    "chat_jid": arg_str(args, "chat_jid"),
                    "account": account,
                });
                let (_, resp) =
                    ensure_bridge_and_post("/api/revoke", &payload, Duration::from_secs(30))?;
                serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
            }
            "chat_action" => {
                let chat_jid = arg_str(args, "chat_jid")
                    .ok_or_else(|| "Missing required parameter 'chat_jid'".to_owned())?;
                let action = arg_str(args, "action")
                    .ok_or_else(|| "Missing required parameter 'action'".to_owned())?;
                let payload = json!({
                    "chat_jid": chat_jid,
                    "action": action,
                    "account": account,
                });
                let (_, resp) =
                    ensure_bridge_and_post("/api/chat-action", &payload, Duration::from_secs(30))?;
                serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
            }
            "download_media" => {
                let message_id = arg_str(args, "message_id")
                    .ok_or_else(|| "Missing required parameter 'message_id'".to_owned())?;
                let chat_jid = arg_str(args, "chat_jid");
                let payload = json!({
                    "message_id": message_id,
                    "chat_jid": chat_jid.unwrap_or(""),
                    "account": account,
                });
                let (_, resp) =
                    ensure_bridge_and_post("/api/download", &payload, Duration::from_secs(120))?;
                serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
            }
            "send_poll" => {
                let chat_jid = arg_str(args, "chat_jid")
                    .ok_or_else(|| "Missing required parameter 'chat_jid'".to_owned())?;
                let question = arg_str(args, "question")
                    .ok_or_else(|| "Missing required parameter 'question'".to_owned())?;
                let options = args
                    .get("options")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "Missing required array parameter 'options'".to_owned())?;
                let payload = json!({
                    "chat_jid": chat_jid,
                    "question": question,
                    "options": options,
                    "multiple": arg_bool(args, "multiple", true),
                    "account": account,
                });
                let (_, resp) =
                    ensure_bridge_and_post("/api/poll", &payload, Duration::from_secs(30))?;
                serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())
            }
            other => Err(format!("Unknown tool: {other}")),
        }
    }
}

fn tools_schema() -> Value {
    json!({
        "tools": [
            {
                "name": "list_accounts",
                "description": "List all WhatsApp accounts configured in ZapFast, including account_id, phone_number, name, active status, and connection state.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "search_contacts",
                "description": "Search WhatsApp contacts by name or phone number.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search term to match against contact names or phone numbers" },
                        "account": { "type": "string", "description": "Optional account selector (account_id, phone number, or name)" }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "list_messages",
                "description": "Get WhatsApp messages matching specified criteria with optional surrounding context.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "after": { "type": "string", "description": "Optional ISO-8601 date/time string to return messages after" },
                        "before": { "type": "string", "description": "Optional ISO-8601 date/time string to return messages before" },
                        "sender_phone_number": { "type": "string", "description": "Optional phone number or JID to filter by sender" },
                        "chat_jid": { "type": "string", "description": "Optional chat JID or phone number to filter by chat" },
                        "query": { "type": "string", "description": "Optional search term to filter messages by text/caption" },
                        "limit": { "type": "integer", "default": 20 },
                        "page": { "type": "integer", "default": 0 },
                        "include_context": { "type": "boolean", "default": true },
                        "context_before": { "type": "integer", "default": 1 },
                        "context_after": { "type": "integer", "default": 1 },
                        "account": { "type": "string", "description": "Optional account selector" }
                    }
                }
            },
            {
                "name": "list_chats",
                "description": "Get WhatsApp chats matching specified criteria, including archive, pin, unread, mute, and kind metadata.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Optional search term to filter chats by name or JID" },
                        "limit": { "type": "integer", "default": 20 },
                        "page": { "type": "integer", "default": 0 },
                        "include_last_message": { "type": "boolean", "default": true },
                        "sort_by": { "type": "string", "default": "last_active", "description": "last_active or name" },
                        "archived": { "type": "boolean", "description": "false = non-archived inbox only, true = archived only, omitted = all" },
                        "unread_only": { "type": "boolean", "default": false },
                        "chat_type": { "type": "string", "description": "direct or group" },
                        "account": { "type": "string", "description": "Optional account selector" }
                    }
                }
            },
            {
                "name": "get_chat",
                "description": "Get WhatsApp chat metadata by JID or phone number.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "chat_jid": { "type": "string" },
                        "include_last_message": { "type": "boolean", "default": true },
                        "account": { "type": "string" }
                    },
                    "required": ["chat_jid"]
                }
            },
            {
                "name": "get_direct_chat_by_contact",
                "description": "Get WhatsApp direct chat metadata by sender phone number.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sender_phone_number": { "type": "string" },
                        "account": { "type": "string" }
                    },
                    "required": ["sender_phone_number"]
                }
            },
            {
                "name": "get_contact_chats",
                "description": "Get all WhatsApp direct and group chats involving the contact.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "jid": { "type": "string" },
                        "limit": { "type": "integer", "default": 20 },
                        "page": { "type": "integer", "default": 0 },
                        "account": { "type": "string" }
                    },
                    "required": ["jid"]
                }
            },
            {
                "name": "get_last_interaction",
                "description": "Get most recent WhatsApp message involving the contact.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "jid": { "type": "string" },
                        "account": { "type": "string" }
                    },
                    "required": ["jid"]
                }
            },
            {
                "name": "get_message_context",
                "description": "Get messages immediately before and after a specific WhatsApp message ID.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "message_id": { "type": "string" },
                        "chat_jid": { "type": "string" },
                        "before": { "type": "integer", "default": 5 },
                        "after": { "type": "integer", "default": 5 },
                        "account": { "type": "string" }
                    },
                    "required": ["message_id"]
                }
            },
            {
                "name": "get_group_info",
                "description": "Get WhatsApp group metadata including description, participant count, and full participant list with resolved phone numbers, names, and admin flags.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "chat_jid": { "type": "string" },
                        "account": { "type": "string" }
                    },
                    "required": ["chat_jid"]
                }
            },
            {
                "name": "query_archive",
                "description": "Execute a read-only SQL query (SELECT / WITH / PRAGMA) against ZapFast's encrypted SQLCipher archive.db.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sql": { "type": "string", "description": "Read-only SQL query" },
                        "account": { "type": "string" }
                    },
                    "required": ["sql"]
                }
            },
            {
                "name": "send_message",
                "description": "Send a WhatsApp message to a person or group. Supports quoting/replying to a message and @mentioning users.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "recipient": { "type": "string", "description": "Phone number or JID" },
                        "message": { "type": "string", "description": "Message text to send" },
                        "reply_to": { "type": "string", "description": "Optional message ID to quote-reply to" },
                        "mentions": { "type": "array", "items": { "type": "string" }, "description": "Optional list of phone numbers or JIDs to @mention" },
                        "account": { "type": "string" }
                    },
                    "required": ["recipient", "message"]
                }
            },
            {
                "name": "send_file",
                "description": "Send a file (picture, video, audio, or document) via WhatsApp to the specified recipient.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "recipient": { "type": "string" },
                        "media_path": { "type": "string", "description": "Absolute path to the file" },
                        "caption": { "type": "string" },
                        "reply_to": { "type": "string" },
                        "as_document": { "type": "boolean", "default": false },
                        "view_once": { "type": "boolean", "default": false },
                        "account": { "type": "string" }
                    },
                    "required": ["recipient", "media_path"]
                }
            },
            {
                "name": "send_audio_message",
                "description": "Send an audio file as a WhatsApp voice message to the specified recipient.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "recipient": { "type": "string" },
                        "media_path": { "type": "string" },
                        "reply_to": { "type": "string" },
                        "account": { "type": "string" }
                    },
                    "required": ["recipient", "media_path"]
                }
            },
            {
                "name": "react_message",
                "description": "React to a WhatsApp message with an emoji (pass empty string to remove reaction).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "message_id": { "type": "string" },
                        "emoji": { "type": "string" },
                        "chat_jid": { "type": "string" },
                        "account": { "type": "string" }
                    },
                    "required": ["message_id", "emoji"]
                }
            },
            {
                "name": "edit_message",
                "description": "Edit a previously sent WhatsApp text message.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "message_id": { "type": "string" },
                        "text": { "type": "string" },
                        "chat_jid": { "type": "string" },
                        "account": { "type": "string" }
                    },
                    "required": ["message_id", "text"]
                }
            },
            {
                "name": "revoke_message",
                "description": "Revoke (delete for everyone) a previously sent WhatsApp message.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "message_id": { "type": "string" },
                        "chat_jid": { "type": "string" },
                        "account": { "type": "string" }
                    },
                    "required": ["message_id"]
                }
            },
            {
                "name": "chat_action",
                "description": "Perform a chat state action: typing, stop_typing, mark_read, mark_unread, archive, unarchive, pin, unpin, mute, or unmute.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "chat_jid": { "type": "string" },
                        "action": { "type": "string" },
                        "account": { "type": "string" }
                    },
                    "required": ["chat_jid", "action"]
                }
            },
            {
                "name": "download_media",
                "description": "Download media from a WhatsApp message and return the local file path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "message_id": { "type": "string" },
                        "chat_jid": { "type": "string" },
                        "account": { "type": "string" }
                    },
                    "required": ["message_id", "chat_jid"]
                }
            },
            {
                "name": "send_poll",
                "description": "Create and send a native WhatsApp poll to a chat or group.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "chat_jid": { "type": "string" },
                        "question": { "type": "string" },
                        "options": { "type": "array", "items": { "type": "string" } },
                        "multiple": { "type": "boolean", "default": true },
                        "account": { "type": "string" }
                    },
                    "required": ["chat_jid", "question", "options"]
                }
            }
        ]
    })
}

fn handle_rpc_request(state: &mut McpState, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    // Notifications have no "id" and expect no response.
    let req_id = id?;

    let result = match method {
        "initialize" => {
            let client_version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05");
            Ok(json!({
                "protocolVersion": client_version,
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    }
                },
                "serverInfo": {
                    "name": "zapfast",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_schema()),
        "resources/list" => Ok(json!({ "resources": [] })),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        "tools/call" => {
            let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match state.call_tool(tool_name, &args) {
                Ok(text) => Ok(json!({
                    "content": [
                        {
                            "type": "text",
                            "text": text
                        }
                    ],
                    "isError": false
                })),
                Err(err) => Ok(json!({
                    "content": [
                        {
                            "type": "text",
                            "text": format!("Error: {err}")
                        }
                    ],
                    "isError": true
                })),
            }
        }
        _ => Err((-32601, format!("Method not found: {method}"))),
    };

    match result {
        Ok(res) => Some(json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": res
        })),
        Err((code, message)) => Some(json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {
                "code": code,
                "message": message
            }
        })),
    }
}

/// Runs the native stdio JSON-RPC 2.0 MCP server until standard input closes.
pub fn run_stdio() -> eframe::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let mut state = McpState::new();
    let mut line = String::new();

    loop {
        line.clear();
        let read_bytes = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(_) => break,
        };
        if read_bytes == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Support both newline-delimited JSON-RPC (standard MCP stdio) and Content-Length framing.
        if let Some(rest) = trimmed
            .strip_prefix("Content-Length:")
            .or_else(|| trimmed.strip_prefix("content-length:"))
            && let Ok(len) = rest.trim().parse::<usize>()
        {
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let mut buf = vec![0u8; len];
            if reader.read_exact(&mut buf).is_err() {
                break;
            }
            if let Ok(req) = serde_json::from_slice::<Value>(&buf)
                && let Some(resp) = handle_rpc_request(&mut state, &req)
                && let Ok(bytes) = serde_json::to_vec(&resp)
            {
                let _ = write!(writer, "Content-Length: {}\r\n\r\n", bytes.len());
                let _ = writer.write_all(&bytes);
                let _ = writer.flush();
            }
            continue;
        }

        if let Ok(req) = serde_json::from_str::<Value>(trimmed)
            && let Some(resp) = handle_rpc_request(&mut state, &req)
            && let Ok(serialized) = serde_json::to_string(&resp)
        {
            let _ = writeln!(writer, "{serialized}");
            let _ = writer.flush();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_rpc_initialize_and_tools_list() {
        let mut state = McpState::new();
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05"
            }
        });
        let init_resp = handle_rpc_request(&mut state, &init_req).unwrap();
        assert_eq!(init_resp["id"], 1);
        assert_eq!(init_resp["result"]["serverInfo"]["name"], "zapfast");

        let list_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let list_resp = handle_rpc_request(&mut state, &list_req).unwrap();
        let tools = list_resp["result"]["tools"].as_array().unwrap();
        assert!(tools.len() >= 19);
    }
}
