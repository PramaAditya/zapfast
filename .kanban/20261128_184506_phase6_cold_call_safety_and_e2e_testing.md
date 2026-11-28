---
title: "Phase 6: Safety Guardrails, Prompt Pack & End-to-End Live Test"
status: todo
priority: medium
created_at: 2026-11-28T18:45:06+07:00
tags:
  - phase-6
  - reliability
  - testing
  - demo
---

# Phase 6: Safety Guardrails, Prompt Pack & End-to-End Live Test

**Target:** Harden the AI calling workflow against network drops, awkward dead air, and WhatsApp account ban risks, then validate end-to-end audio quality and Takeover smoothness on a real test call.

## Checklist
- [ ] **Call safety guardrails:** Enforce configurable max AI call duration (auto wrap-up / hangup), minimum cooldown warning between outbound AI calls, and a first-run notice recommending a dedicated secondary/business number.
- [ ] **Mid-call disconnect recovery:** If the ElevenLabs WebSocket drops or returns an auth/quota error mid-call, display an immediate warning toast/banner on the call screen and highlight the **Takeover** button so the user can rescue the call seamlessly.
- [ ] **Cold-call prompt pack:** Document reusable Indonesian and English ElevenLabs Agent system prompts tuned for concise B2B (Business-to-Business) meeting booking, polite objection handling, and natural handoff when the user clicks Takeover.
- [ ] **End-to-end live test (self/secondary number):**
  - Confirm 16 kHz voice clarity on both ends (no clipping, no robotic speed drift).
  - Verify barge-in stops AI audio within ~300-400 ms when the peer interrupts.
  - Verify Live Monitor plays both Peer and AI voices locally with zero echo/feedback loop.
  - Verify clicking **Takeover** mid-sentence switches cleanly to the user's mic without dropping the WhatsApp call.
- [ ] **Build & quality check:** `cargo test` and `cargo clippy` clean on Windows (and cross-platform `cfg` guards intact).
