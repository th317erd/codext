//! Safety-buffering state for active turns.

use super::*;
use codex_app_server_protocol::ModelSafetyBufferingUpdatedNotification;

const SAFETY_BUFFERING_PROMPT_VIEW_ID: &str = "safety-buffering-prompt";

#[derive(Debug)]
struct ActiveSafetyBuffering {
    turn_id: String,
    agent_message_started: bool,
}

#[derive(Debug, Default)]
pub(super) struct SafetyBufferingState {
    submitted_turn: Option<(String, AppCommand)>,
    active: Option<ActiveSafetyBuffering>,
}

impl ChatWidget {
    pub(crate) fn record_safety_buffering_turn(&mut self, turn_id: String, turn: &AppCommand) {
        self.safety_buffering.submitted_turn = Some((turn_id, turn.clone()));
    }

    pub(super) fn reset_safety_buffering_for_turn_start(&mut self) {
        self.bottom_pane
            .dismiss_view_by_id(SAFETY_BUFFERING_PROMPT_VIEW_ID);
        self.safety_buffering.active = None;
    }

    pub(crate) fn clear_safety_buffering(&mut self) {
        self.bottom_pane
            .dismiss_view_by_id(SAFETY_BUFFERING_PROMPT_VIEW_ID);
        self.safety_buffering = SafetyBufferingState::default();
    }

    pub(super) fn mark_safety_buffering_agent_message_started(&mut self) {
        if let Some(active) = self.safety_buffering.active.as_mut() {
            active.agent_message_started = true;
        }
    }

    pub(super) fn safety_buffering_is_waiting(&self) -> bool {
        self.safety_buffering
            .active
            .as_ref()
            .is_some_and(|active| !active.agent_message_started)
    }

    pub(crate) fn can_retry_safety_buffered_turn(&self, turn_id: &str) -> bool {
        self.turn_lifecycle.agent_turn_running
            && self
                .safety_buffering
                .active
                .as_ref()
                .is_some_and(|active| active.turn_id == turn_id && !active.agent_message_started)
    }

    pub(crate) fn prepare_safety_buffered_retry_submission(&mut self, prompt: UserMessage) {
        self.last_rendered_user_message_display = None;
        self.finalize_turn();
        self.safety_buffering_prompt = Some(prompt);
        self.input_queue.user_turn_pending_start = true;
    }

    pub(crate) fn commit_safety_buffered_retry_submission(&mut self, display: UserMessageDisplay) {
        self.on_user_message_display(display);
    }

    pub(crate) fn cancel_safety_buffered_retry_submission(&mut self) {
        self.input_queue.user_turn_pending_start = false;
        self.clear_safety_buffering();
    }

    pub(super) fn on_model_safety_buffering_updated(
        &mut self,
        notification: ModelSafetyBufferingUpdatedNotification,
        replay_kind: Option<ReplayKind>,
    ) {
        let ModelSafetyBufferingUpdatedNotification {
            turn_id,
            ..
        } = notification;
        if matches!(replay_kind, Some(ReplayKind::ResumeInitialMessages))
            || !self.turn_lifecycle.agent_turn_running
            || self.turn_lifecycle.last_turn_id.as_deref() != Some(turn_id.as_str())
        {
            return;
        }
        if self
            .safety_buffering
            .active
            .as_ref()
            .is_some_and(|active| active.turn_id == turn_id)
        {
            self.bottom_pane
                .dismiss_view_by_id(SAFETY_BUFFERING_PROMPT_VIEW_ID);
            self.safety_buffering.active = None;
            self.restore_reasoning_status_header();
        }
    }
}
