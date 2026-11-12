//! Read queries on the SQLCipher archive for the local MCP bridge.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{Archive, Result, SEARCHED_TEXT, search_pattern};
use crate::model::{Content, Quoted};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpMessage {
    pub id: String,
    pub chat_jid: String,
    pub chat_name: Option<String>,
    pub sender: String,
    pub sender_name: Option<String>,
    pub content: String,
    pub timestamp: i64,
    pub is_from_me: bool,
    pub media_type: Option<String>,
    pub media_path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpChat {
    pub jid: String,
    pub name: Option<String>,
    pub last_message_time: Option<i64>,
    pub last_message: Option<String>,
    pub last_sender: Option<String>,
    pub last_is_from_me: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpContact {
    pub phone_number: String,
    pub name: Option<String>,
    pub jid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpMessageContext {
    pub message: McpMessage,
    pub before: Vec<McpMessage>,
    pub after: Vec<McpMessage>,
}

fn content_to_mcp(raw_json: &str) -> (String, Option<String>, Option<String>) {
    let parsed: Content = serde_json::from_str(raw_json).unwrap_or(Content::Unsupported {
        what: "unreadable".into(),
    });
    match &parsed {
        Content::Text { text, .. } | Content::Interactive { text, .. } => {
            (text.clone(), None, None)
        }
        Content::Image { caption, media, .. } => (
            caption.clone().unwrap_or_default(),
            Some("image".to_owned()),
            media
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        ),
        Content::Video {
            caption,
            media,
            gif,
            ..
        } => (
            caption.clone().unwrap_or_default(),
            Some(if *gif { "gif" } else { "video" }.to_owned()),
            media
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        ),
        Content::Audio {
            media,
            voice_note,
            seconds,
            ..
        } => {
            let label = match seconds {
                Some(s) if *voice_note => format!("Voice message ({s}s)"),
                Some(s) => format!("Audio ({s}s)"),
                None if *voice_note => "Voice message".to_owned(),
                None => "Audio".to_owned(),
            };
            (
                label,
                Some(if *voice_note { "voice" } else { "audio" }.to_owned()),
                media
                    .path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
            )
        }
        Content::Document {
            media,
            file_name,
            caption,
            ..
        } => {
            let text = match caption.as_deref() {
                Some(c) if !c.trim().is_empty() => format!("{file_name}: {c}"),
                _ => file_name.clone(),
            };
            (
                text,
                Some("document".to_owned()),
                media
                    .path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
            )
        }
        Content::Sticker { media, .. } => (
            "Sticker".to_owned(),
            Some("sticker".to_owned()),
            media
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        ),
        other => (other.full_summary(), None, None),
    }
}

fn format_with_quote(text: String, quoted_json: Option<&str>) -> String {
    let Some(quoted_str) = quoted_json else {
        return text;
    };
    let Ok(quoted) = serde_json::from_str::<Quoted>(quoted_str) else {
        return text;
    };
    if quoted.summary.trim().is_empty() {
        return text;
    }
    let who = quoted
        .sender_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&quoted.sender);
    format!("> [Reply to {who}: {}]\n{text}", quoted.summary)
}

impl Archive {
    /// Resolves a JID or phone number to all known archive JIDs (canonical `@s.whatsapp.net` and `@lid`).
    pub fn mcp_resolve_jids(&self, input: &str) -> Vec<String> {
        let trimmed = input.trim().trim_start_matches('+');
        if trimmed.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut push_unique = |val: String| {
            if !val.is_empty() && !out.contains(&val) {
                out.push(val);
            }
        };

        if let Some((user, server)) = trimmed.split_once('@') {
            push_unique(trimmed.to_owned());
            if server == "lid" {
                if let Ok(Some(pn)) = self
                    .connection
                    .query_row("SELECT pn FROM lids WHERE lid = ?1", params![user], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()
                {
                    push_unique(format!("{pn}@s.whatsapp.net"));
                    push_unique(pn);
                }
            } else if server == "s.whatsapp.net" {
                push_unique(user.to_owned());
                if let Ok(mut stmt) = self
                    .connection
                    .prepare("SELECT lid FROM lids WHERE pn = ?1")
                    && let Ok(rows) = stmt.query_map(params![user], |r| r.get::<_, String>(0))
                {
                    for lid in rows.flatten() {
                        push_unique(format!("{lid}@lid"));
                    }
                }
            }
        } else {
            push_unique(format!("{trimmed}@s.whatsapp.net"));
            push_unique(trimmed.to_owned());
            if let Ok(mut stmt) = self
                .connection
                .prepare("SELECT lid FROM lids WHERE pn = ?1")
                && let Ok(rows) = stmt.query_map(params![trimmed], |r| r.get::<_, String>(0))
            {
                for lid in rows.flatten() {
                    push_unique(format!("{lid}@lid"));
                }
            }
            if let Ok(Some(pn)) = self
                .connection
                .query_row(
                    "SELECT pn FROM lids WHERE lid = ?1",
                    params![trimmed],
                    |r| r.get::<_, String>(0),
                )
                .optional()
            {
                push_unique(format!("{pn}@s.whatsapp.net"));
                push_unique(pn);
            }
        }
        out
    }

    /// Best known display name for a sender JID.
    pub fn mcp_sender_name(&self, sender: &str, row_sender_name: Option<String>) -> String {
        let candidates = self.mcp_resolve_jids(sender);
        for candidate in &candidates {
            if !candidate.contains('@') {
                continue;
            }
            if let Ok(Some(name)) = self
                .connection
                .query_row(
                    "SELECT COALESCE(NULLIF(full_name, ''), NULLIF(first_name, ''), NULLIF(push_name, ''))
                     FROM contacts WHERE id = ?1",
                    params![candidate],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .map(Option::flatten)
                && !name.trim().is_empty()
            {
                return name;
            }
            if let Ok(Some(name)) = self
                .connection
                .query_row(
                    "SELECT name FROM chats WHERE id = ?1 AND kind = 'direct'",
                    params![candidate],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                && !name.trim().is_empty()
            {
                return name;
            }
        }
        if let Some(name) = row_sender_name.filter(|s| !s.trim().is_empty()) {
            return name;
        }
        candidates
            .into_iter()
            .find(|c| c.ends_with("@s.whatsapp.net"))
            .unwrap_or_else(|| sender.to_owned())
    }

    fn row_to_mcp_message(
        &self,
        id: String,
        chat_jid: String,
        chat_name: Option<String>,
        sender: String,
        row_sender_name: Option<String>,
        from_me: bool,
        timestamp: i64,
        content_json: String,
        quoted_json: Option<String>,
    ) -> McpMessage {
        let (raw_text, media_type, media_path) = content_to_mcp(&content_json);
        let content = format_with_quote(raw_text, quoted_json.as_deref());
        let sender_name = if from_me {
            Some("Me".to_owned())
        } else {
            Some(self.mcp_sender_name(&sender, row_sender_name))
        };
        McpMessage {
            id,
            chat_jid,
            chat_name,
            sender,
            sender_name,
            content,
            timestamp,
            is_from_me: from_me,
            media_type,
            media_path,
        }
    }

    fn row_to_mcp_chat(
        &self,
        row: &rusqlite::Row<'_>,
        include_last: bool,
    ) -> rusqlite::Result<McpChat> {
        let jid: String = row.get(0)?;
        let name: String = row.get(1)?;
        let last_activity: i64 = row.get(2)?;
        let (last_message, last_sender, last_is_from_me, last_msg_ts) = if include_last {
            let content_json: Option<String> = row.get(3)?;
            let sender: Option<String> = row.get(4)?;
            let from_me: Option<bool> = row.get(5)?;
            let msg_ts: Option<i64> = row.get(6)?;
            let summary = content_json.map(|c| content_to_mcp(&c).0);
            (summary, sender, from_me, msg_ts)
        } else {
            (None, None, None, None)
        };
        let ts = last_msg_ts.unwrap_or(last_activity);
        Ok(McpChat {
            jid,
            name: (!name.is_empty()).then_some(name),
            last_message_time: (ts > 0).then_some(ts),
            last_message,
            last_sender,
            last_is_from_me,
        })
    }

    pub fn mcp_list_chats(
        &self,
        query: Option<&str>,
        limit: usize,
        page: usize,
        include_last_message: bool,
        sort_by: &str,
    ) -> Result<Vec<McpChat>> {
        let pattern = query.and_then(search_pattern);
        let order_by = if sort_by == "name" {
            "c.name ASC, c.last_activity DESC"
        } else {
            "c.last_activity DESC, c.name ASC"
        };
        let sql = format!(
            "SELECT c.id, c.name, c.last_activity, m.content, m.sender, m.from_me, m.timestamp
             FROM chats c
             LEFT JOIN messages m ON m.chat = c.id AND m.rowid = (
                 SELECT rowid FROM messages WHERE chat = c.id
                 ORDER BY timestamp DESC, COALESCE(history_order, 9223372036854775807) DESC, rowid DESC
                 LIMIT 1
             )
             WHERE c.id != 'zapfast-call-link'
               AND (?1 IS NULL OR lower(c.name) LIKE ?1 ESCAPE '\\' OR lower(c.id) LIKE ?1 ESCAPE '\\')
             ORDER BY {order_by}
             LIMIT ?2 OFFSET ?3"
        );
        let mut stmt = self.connection.prepare(&sql)?;
        let offset = page.saturating_mul(limit);
        let rows = stmt.query_map(params![pattern, limit as i64, offset as i64], |row| {
            self.row_to_mcp_chat(row, include_last_message)
        })?;
        rows.collect()
    }

    pub fn mcp_get_chat(
        &self,
        chat_jid: &str,
        include_last_message: bool,
    ) -> Result<Option<McpChat>> {
        let candidates = self.mcp_resolve_jids(chat_jid);
        let sql = "SELECT c.id, c.name, c.last_activity, m.content, m.sender, m.from_me, m.timestamp
             FROM chats c
             LEFT JOIN messages m ON m.chat = c.id AND m.rowid = (
                 SELECT rowid FROM messages WHERE chat = c.id
                 ORDER BY timestamp DESC, COALESCE(history_order, 9223372036854775807) DESC, rowid DESC
                 LIMIT 1
             )
             WHERE c.id = ?1
             LIMIT 1";
        let mut stmt = self.connection.prepare(sql)?;
        for candidate in candidates {
            if !candidate.contains('@') {
                continue;
            }
            if let Some(chat) = stmt
                .query_row(params![candidate], |row| {
                    self.row_to_mcp_chat(row, include_last_message)
                })
                .optional()?
            {
                return Ok(Some(chat));
            }
        }
        Ok(None)
    }

    pub fn mcp_get_direct_chat_by_contact(&self, phone: &str) -> Result<Option<McpChat>> {
        let candidates = self.mcp_resolve_jids(phone);
        for candidate in &candidates {
            if candidate.contains('@')
                && !candidate.ends_with("@g.us")
                && let Some(chat) = self.mcp_get_chat(candidate, true)?
            {
                return Ok(Some(chat));
            }
        }
        let cleaned = phone.trim().trim_start_matches('+');
        let Some(pattern) = search_pattern(cleaned) else {
            return Ok(None);
        };
        let sql = "SELECT c.id, c.name, c.last_activity, m.content, m.sender, m.from_me, m.timestamp
             FROM chats c
             LEFT JOIN messages m ON m.chat = c.id AND m.rowid = (
                 SELECT rowid FROM messages WHERE chat = c.id
                 ORDER BY timestamp DESC, COALESCE(history_order, 9223372036854775807) DESC, rowid DESC
                 LIMIT 1
             )
             WHERE c.kind = 'direct' AND lower(c.id) LIKE ?1 ESCAPE '\\'
             ORDER BY c.last_activity DESC
             LIMIT 1";
        self.connection
            .query_row(params![pattern], |row| self.row_to_mcp_chat(row, true))
            .optional()
    }

    pub fn mcp_search_contacts(&self, query: &str) -> Result<Vec<McpContact>> {
        let pattern = search_pattern(query).unwrap_or_else(|| "%".to_owned());
        let sql = "
            SELECT jid, name FROM (
                SELECT c.id AS jid, c.name AS name, c.last_activity AS weight
                FROM chats c
                WHERE c.kind = 'direct' AND c.id NOT LIKE '%@g.us' AND c.id NOT LIKE '%@newsletter'
                UNION ALL
                SELECT ct.id AS jid,
                       COALESCE(NULLIF(ct.full_name, ''), NULLIF(ct.first_name, ''), NULLIF(ct.push_name, ''), ct.id) AS name,
                       0 AS weight
                FROM contacts ct
                WHERE ct.id NOT IN (SELECT id FROM chats WHERE kind = 'direct')
            )
            WHERE lower(name) LIKE ?1 ESCAPE '\\' OR lower(jid) LIKE ?1 ESCAPE '\\'
            ORDER BY weight DESC, name ASC, jid ASC
            LIMIT 50
        ";
        let mut stmt = self.connection.prepare(sql)?;
        let rows = stmt.query_map(params![pattern], |row| {
            let jid: String = row.get(0)?;
            let name: String = row.get(1)?;
            let phone_number = jid.split('@').next().unwrap_or(&jid).to_owned();
            Ok(McpContact {
                phone_number,
                name: (!name.is_empty()).then_some(name),
                jid,
            })
        })?;
        rows.collect()
    }

    pub fn mcp_get_contact_chats(
        &self,
        jid: &str,
        limit: usize,
        page: usize,
    ) -> Result<Vec<McpChat>> {
        let candidates = self.mcp_resolve_jids(jid);
        let primary = candidates
            .first()
            .cloned()
            .unwrap_or_else(|| jid.to_owned());
        let secondary = candidates
            .get(1)
            .cloned()
            .unwrap_or_else(|| primary.clone());
        let like_primary = search_pattern(&primary).unwrap_or_default();
        let offset = page.saturating_mul(limit);
        let sql = "
            SELECT c.id, c.name, c.last_activity, lm.content, lm.sender, lm.from_me, lm.timestamp
            FROM chats c
            LEFT JOIN messages lm ON lm.chat = c.id AND lm.rowid = (
                SELECT rowid FROM messages WHERE chat = c.id
                ORDER BY timestamp DESC, COALESCE(history_order, 9223372036854775807) DESC, rowid DESC
                LIMIT 1
            )
            WHERE c.id IN (?1, ?2)
               OR (?3 != '' AND lower(c.participants) LIKE ?3 ESCAPE '\\')
               OR EXISTS (
                   SELECT 1 FROM messages m
                   WHERE m.chat = c.id AND m.sender IN (?1, ?2)
               )
            ORDER BY c.last_activity DESC
            LIMIT ?4 OFFSET ?5
        ";
        let mut stmt = self.connection.prepare(sql)?;
        let rows = stmt.query_map(
            params![
                primary,
                secondary,
                like_primary,
                limit as i64,
                offset as i64
            ],
            |row| self.row_to_mcp_chat(row, true),
        )?;
        rows.collect()
    }

    pub fn mcp_get_last_interaction(&self, jid: &str) -> Result<Option<McpMessage>> {
        let candidates = self.mcp_resolve_jids(jid);
        let primary = candidates
            .first()
            .cloned()
            .unwrap_or_else(|| jid.to_owned());
        let secondary = candidates
            .get(1)
            .cloned()
            .unwrap_or_else(|| primary.clone());
        let sql = "
            SELECT m.id, m.chat, c.name, m.sender, m.sender_name, m.from_me, m.timestamp, m.content, m.quoted
            FROM messages m
            LEFT JOIN chats c ON c.id = m.chat
            WHERE (m.chat IN (?1, ?2) OR m.sender IN (?1, ?2))
              AND json_valid(m.content)
              AND json_extract(m.content, '$.kind') != 'disappearing_timer'
            ORDER BY m.timestamp DESC, COALESCE(m.history_order, 9223372036854775807) DESC, m.rowid DESC
            LIMIT 1
        ";
        let row = self
            .connection
            .query_row(params![primary, secondary], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, bool>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })
            .optional()?;
        Ok(row.map(
            |(
                id,
                chat_jid,
                chat_name,
                sender,
                sender_name,
                from_me,
                timestamp,
                content,
                quoted,
            )| {
                self.row_to_mcp_message(
                    id,
                    chat_jid,
                    chat_name,
                    sender,
                    sender_name,
                    from_me,
                    timestamp,
                    content,
                    quoted,
                )
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mcp_list_messages(
        &self,
        after: Option<i64>,
        before: Option<i64>,
        sender_phone_number: Option<&str>,
        chat_jid: Option<&str>,
        query: Option<&str>,
        limit: usize,
        page: usize,
    ) -> Result<Vec<McpMessage>> {
        let after_ts = after.unwrap_or(i64::MIN);
        let before_ts = before.unwrap_or(i64::MAX);
        let chat_candidates = chat_jid
            .map(|c| self.mcp_resolve_jids(c))
            .unwrap_or_default();
        let chat_1 = chat_candidates.first().cloned();
        let chat_2 = chat_candidates.get(1).cloned().or_else(|| chat_1.clone());

        let sender_candidates = sender_phone_number
            .map(|s| self.mcp_resolve_jids(s))
            .unwrap_or_default();
        let sender_1 = sender_candidates.first().cloned();
        let sender_2 = sender_candidates
            .get(1)
            .cloned()
            .or_else(|| sender_1.clone());
        let sender_like = sender_phone_number.and_then(search_pattern);
        let text_pattern = query.and_then(search_pattern);
        let offset = page.saturating_mul(limit);

        let sql = format!(
            "SELECT m.id, m.chat, c.name, m.sender, m.sender_name, m.from_me, m.timestamp, m.content, m.quoted
             FROM messages m
             LEFT JOIN chats c ON c.id = m.chat
             WHERE m.timestamp > ?1 AND m.timestamp < ?2
               AND json_valid(m.content)
               AND json_extract(m.content, '$.kind') != 'disappearing_timer'
               AND (?3 IS NULL OR m.chat IN (?3, ?4))
               AND (?5 IS NULL OR m.sender IN (?5, ?6) OR (?7 IS NOT NULL AND lower(m.sender) LIKE ?7 ESCAPE '\\'))
               AND (?8 IS NULL OR {SEARCHED_TEXT} LIKE ?8 ESCAPE '\\')
             ORDER BY m.timestamp DESC, COALESCE(m.history_order, 9223372036854775807) DESC, m.rowid DESC
             LIMIT ?9 OFFSET ?10"
        );
        let mut stmt = self.connection.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                after_ts,
                before_ts,
                chat_1,
                chat_2,
                sender_1,
                sender_2,
                sender_like,
                text_pattern,
                limit as i64,
                offset as i64,
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, bool>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (id, chat, chat_name, sender, sender_name, from_me, ts, content, quoted) = row?;
            out.push(self.row_to_mcp_message(
                id,
                chat,
                chat_name,
                sender,
                sender_name,
                from_me,
                ts,
                content,
                quoted,
            ));
        }
        Ok(out)
    }

    pub fn mcp_get_message_context(
        &self,
        message_id: &str,
        chat_jid: Option<&str>,
        before_count: usize,
        after_count: usize,
    ) -> Result<Option<McpMessageContext>> {
        let chat_filter = chat_jid.and_then(|c| {
            self.mcp_resolve_jids(c)
                .into_iter()
                .find(|j| j.contains('@'))
        });
        let target_row = self
            .connection
            .query_row(
                "SELECT m.id, m.chat, c.name, m.sender, m.sender_name, m.from_me, m.timestamp, m.content, m.quoted, m.rowid
                 FROM messages m
                 LEFT JOIN chats c ON c.id = m.chat
                 WHERE m.id = ?1 AND (?2 IS NULL OR m.chat = ?2)
                 ORDER BY m.timestamp DESC
                 LIMIT 1",
                params![message_id, chat_filter],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, bool>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, Option<String>>(8)?,
                        r.get::<_, i64>(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, chat, chat_name, sender, sender_name, from_me, ts, content, quoted, rowid)) =
            target_row
        else {
            return Ok(None);
        };
        let message = self.row_to_mcp_message(
            id,
            chat.clone(),
            chat_name,
            sender,
            sender_name,
            from_me,
            ts,
            content,
            quoted,
        );

        let mut before_stmt = self.connection.prepare(
            "SELECT m.id, m.chat, c.name, m.sender, m.sender_name, m.from_me, m.timestamp, m.content, m.quoted
             FROM messages m
             LEFT JOIN chats c ON c.id = m.chat
             WHERE m.chat = ?1
               AND (m.timestamp < ?2 OR (m.timestamp = ?2 AND m.rowid < ?3))
               AND json_valid(m.content)
               AND json_extract(m.content, '$.kind') != 'disappearing_timer'
             ORDER BY m.timestamp DESC, m.rowid DESC
             LIMIT ?4",
        )?;
        let before_rows =
            before_stmt.query_map(params![&chat, ts, rowid, before_count as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, bool>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })?;
        let mut before = Vec::new();
        for row in before_rows {
            let (id, ch, cn, s, sn, fm, t, c, q) = row?;
            before.push(self.row_to_mcp_message(id, ch, cn, s, sn, fm, t, c, q));
        }
        before.reverse();

        let mut after_stmt = self.connection.prepare(
            "SELECT m.id, m.chat, c.name, m.sender, m.sender_name, m.from_me, m.timestamp, m.content, m.quoted
             FROM messages m
             LEFT JOIN chats c ON c.id = m.chat
             WHERE m.chat = ?1
               AND (m.timestamp > ?2 OR (m.timestamp = ?2 AND m.rowid > ?3))
               AND json_valid(m.content)
               AND json_extract(m.content, '$.kind') != 'disappearing_timer'
             ORDER BY m.timestamp ASC, m.rowid ASC
             LIMIT ?4",
        )?;
        let after_rows =
            after_stmt.query_map(params![&chat, ts, rowid, after_count as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, bool>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })?;
        let mut after = Vec::new();
        for row in after_rows {
            let (id, ch, cn, s, sn, fm, t, c, q) = row?;
            after.push(self.row_to_mcp_message(id, ch, cn, s, sn, fm, t, c, q));
        }

        Ok(Some(McpMessageContext {
            message,
            before,
            after,
        }))
    }

    pub fn mcp_find_message_chat(&self, message_id: &str) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT chat FROM messages WHERE id = ?1 ORDER BY timestamp DESC LIMIT 1",
                params![message_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Chat, Contact, Delivery, Message};

    fn fixture_message(
        id: &str,
        chat: &str,
        sender: &str,
        from_me: bool,
        ts: i64,
        text: &str,
    ) -> Message {
        Message {
            id: id.into(),
            chat: chat.into(),
            sender: sender.into(),
            sender_name: Some("Fixture Sender".into()),
            from_me,
            timestamp: ts,
            history_order: None,
            content: Content::text(text),
            status: Delivery::Read,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        }
    }

    #[test]
    fn mcp_queries_chats_messages_contacts_and_lids() {
        let archive = Archive::in_memory().unwrap();
        let mut chat = Chat::new("628123456789@s.whatsapp.net".into(), "Budi Santoso".into());
        chat.last_activity = 200;
        archive.upsert_chat(&chat).unwrap();
        archive
            .upsert_contact(&Contact {
                id: "628123456789@s.whatsapp.net".into(),
                full_name: Some("Budi Santoso Full".into()),
                first_name: Some("Budi".into()),
                push_name: Some("BudiS".into()),
            })
            .unwrap();
        archive.put_lid("999888777", "628123456789").unwrap();

        let m1 = fixture_message(
            "m1",
            "628123456789@s.whatsapp.net",
            "628123456789@s.whatsapp.net",
            false,
            100,
            "Halo bro",
        );
        let m2 = fixture_message(
            "m2",
            "628123456789@s.whatsapp.net",
            "me@s.whatsapp.net",
            true,
            150,
            "Siap aman",
        );
        let m3 = fixture_message(
            "m3",
            "628123456789@s.whatsapp.net",
            "628123456789@s.whatsapp.net",
            false,
            200,
            "Mantap sekali",
        );
        archive.insert_message(&m1, None).unwrap();
        archive.insert_message(&m2, None).unwrap();
        archive.insert_message(&m3, None).unwrap();

        let resolved = archive.mcp_resolve_jids("999888777@lid");
        assert!(resolved.contains(&"628123456789@s.whatsapp.net".to_owned()));

        let chats = archive
            .mcp_list_chats(Some("budi"), 10, 0, true, "last_active")
            .unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].last_message.as_deref(), Some("Mantap sekali"));

        let direct = archive
            .mcp_get_direct_chat_by_contact("628123456789")
            .unwrap()
            .unwrap();
        assert_eq!(direct.jid, "628123456789@s.whatsapp.net");

        let contacts = archive.mcp_search_contacts("Budi").unwrap();
        assert!(!contacts.is_empty());
        assert_eq!(contacts[0].phone_number, "628123456789");

        let messages = archive
            .mcp_list_messages(None, None, None, Some("999888777@lid"), Some("siap"), 10, 0)
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "m2");
        assert!(messages[0].is_from_me);

        let ctx = archive
            .mcp_get_message_context("m2", None, 1, 1)
            .unwrap()
            .unwrap();
        assert_eq!(ctx.message.id, "m2");
        assert_eq!(ctx.before.len(), 1);
        assert_eq!(ctx.before[0].id, "m1");
        assert_eq!(ctx.after.len(), 1);
        assert_eq!(ctx.after[0].id, "m3");

        let last = archive
            .mcp_get_last_interaction("999888777@lid")
            .unwrap()
            .unwrap();
        assert_eq!(last.id, "m3");
        assert_eq!(last.sender_name.as_deref(), Some("Budi Santoso Full"));
    }
}
