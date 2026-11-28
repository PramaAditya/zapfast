---
title: "Phase 4: Built-in AI Call UI, Live Monitor Toggle & Takeover Button"
status: todo
priority: high
created_at: 2026-11-28T18:45:04+07:00
tags:
  - phase-4
  - ui
  - calls
  - ux
---

# Phase 4: Built-in AI Call UI, Live Monitor Toggle & Takeover Button

**Target:** Add native egui controls in ZapFast's chat header and live call screen so the user can launch an AI cold call in one click, monitor the live conversation with real-time transcripts, and click Takeover to jump in seamlessly.

## Technical Design
- **UI/Runtime Boundary:** Views in `src/ui/` only render state from `CallUpdate` and emit `model::Action` variants (`StartAiCall`, `ToggleAiMonitor`, `TakeoverAiCall`); `src/app/calls.rs` dispatches matching `Command`s to the worker and wakes the window on every update.
- **Visual Clarity:** Clearly distinguish AI-driven calls from human calls using status pills (`AI Dialing`, `AI Speaking`, `AI Listening`, `Human Takeover`) so the user always knows whether the microphone is live.

## Checklist
- [ ] **Model & App actions (`src/model.rs`, `src/app/calls.rs`):** Add `Action::StartAiCall { chat, prospect_name, company, note }`, `Action::SetAiMonitor(bool)`, and `Action::TakeoverAiCall` with optimistic UI feedback and backend routing.
- [ ] **Chat header trigger (`src/ui/conversation.rs`):** Add an "AI Call" button (disabled with tooltip if ElevenLabs credentials/Agent ID are not configured) plus a lightweight pre-call modal to confirm/edit prospect name, company, and call objective before dialing.
- [ ] **Live Call Screen (`src/ui/call.rs`):**
  - Status pill indicating AI vs Human state and speaking/listening activity.
  - **Live Monitor toggle button** (headphones/speaker icon) to mute/unmute local monitoring mid-call.
  - High-contrast **Takeover button** (microphone icon + label "Takeover") that transitions the call to the user's mic and reveals standard Mute/Device controls once taken over.
  - Scrollable **Live Transcript panel** rendering `Peer` and `AI` turns as they arrive.
- [ ] **Minimized Call Strip (`src/ui/call.rs::strip`):** Display an `AI` indicator and quick Takeover/return access when the call surface is minimized.
- [ ] **Theme & Demo verification:** Verify layout in both Light and Dark themes using the `demo` feature fixture.
