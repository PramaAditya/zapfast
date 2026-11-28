---
title: "Phase 5: Meeting Booking Tool & Post-Call WhatsApp Follow-Up"
status: todo
priority: medium
created_at: 2026-11-28T18:45:05+07:00
tags:
  - phase-5
  - meeting-setter
  - tools
  - follow-up
---

# Phase 5: Meeting Booking Tool & Post-Call WhatsApp Follow-Up

**Target:** Capture meeting booking outcomes directly from the ElevenLabs agent during the call and automatically prepare a personalized WhatsApp follow-up message in the same chat right after hangup.

## Technical Design
- **Client Tool Contract:** Register a client tool (`record_meeting_outcome`) in the ElevenLabs Agent configuration accepting `{ outcome: "booked" | "callback" | "not_interested" | "voicemail", proposed_datetime: string?, email: string?, summary: string }`.
- **Safe Follow-Up Default:** On call completion, pre-populate the active chat's composer draft (plus a post-call summary banner) with the agreed meeting time and booking link so the user can review and hit Send in one click.
- **Low-Latency Tool Response (<50 ms) (Gap #5):** Because ElevenLabs pauses agent speech while waiting for `client_tool_result`, acknowledge the tool call from memory in <50 ms and persist to disk/DB asynchronously in the background.
- **ZapFast MCP & SSE Integration (Gap #5):** Emit `record_meeting_outcome` and AI call lifecycle updates onto the existing localhost `/api/events` SSE stream and expose call outcomes via ZapFast's MCP server / `archive.db` so external CRM or research agents can react automatically.

## Checklist
- [ ] **Client tool handler (`src/ai_call/tools.rs`):** Handle `client_tool_call` for `record_meeting_outcome`, store the structured result on the active call state, and reply with `client_tool_result` acknowledging success to ElevenLabs.
- [ ] **Post-call draft injection (`src/app/calls.rs`, `src/ui/conversation.rs`):** When an AI call ends with `booked` or `callback`, pre-fill the chat composer with a clean confirmation template (prospect name, agreed time, calendar link) and show a dismissible summary banner.
- [ ] **Local call transcript history:** Save AI call outcome + transcript locally in the app data directory (never print message/transcript content to logs) with an option to copy summary to clipboard.
- [ ] **Unit tests:** Validate tool argument parsing, fallback behavior on malformed JSON from the agent, and draft template generation.
- [ ] **MCP & SSE event streaming:** Broadcast AI call state and `record_meeting_outcome` payloads to `/api/events` SSE and expose recent call outcomes through `src/mcp.rs` / `archive.db` (acknowledging `client_tool_result` from memory in <50 ms before disk persistence).
