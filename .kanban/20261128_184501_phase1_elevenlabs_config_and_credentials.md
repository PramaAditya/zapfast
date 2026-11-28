---
title: "Phase 1: ElevenLabs Configuration & Secure Credentials"
status: done
priority: high
created_at: 2026-11-28T18:45:01+07:00
completed_at: 2026-11-28T20:05:00+07:00
tags:
  - phase-1
  - settings
  - security
  - elevenlabs
---

# Phase 1: ElevenLabs Configuration & Secure Credentials

**Target:** Store ElevenLabs Agents configuration safely in ZapFast settings and OS keyring so Option A (built-in AI calling) works from the UI without exposing API keys in plaintext files or logs.

## Technical Design
- **Secret Storage:** Store `ELEVENLABS_API_KEY` in the OS keyring via `keyring_core` (reusing the pattern in `src/archive/encryption.rs`), with fallback to the `ELEVENLABS_API_KEY` environment variable for local development. Never persist the API key in `settings.json` or write it to logs.
- **Non-Secret Settings (`src/settings.rs`):** Persist `AiCallSettings` (`agent_id`, `live_monitor_default: bool`, `max_call_duration_secs`, `default_pitch_context`, `default_calendar_link`) with `#[serde(default)]` for full backward compatibility with existing settings files.

## Checklist
- [x] **Settings schema (`src/settings.rs`):** Added `AiCallSettings` struct with safe defaults (`live_monitor_default = true`, `max_call_duration_secs = 300`) and serde legacy/round-trip unit test.
- [x] **Keyring helper (`src/ai_call/credentials.rs`):** Implemented `load_api_key()`, `save_api_key()`, `clear_api_key()`, and `test_connection()` backed by `keyring_core` with `ELEVENLABS_API_KEY` env var fallback.
- [x] **Settings UI section (`src/ui/settings.rs`):** Added "AI Voice Call (ElevenLabs)" section with masked API key field + Save/Clear, Agent ID input, default Live Monitor toggle, max call duration selector, default calendar link, and non-blocking "Test connection" button.
- [x] **Verification:** `cargo test --lib -j 8 ai_call` passed (4/4 tests, 0 Rust compiler warnings).
