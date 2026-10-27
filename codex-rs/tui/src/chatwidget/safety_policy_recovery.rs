//! One-shot recovery after a provider safety-policy block.

use super::*;

const SAFETY_POLICY_RECOVERY_PROMPT: &str = "The previous response was unavailable because the provider blocked it for safety. Continue only with safe, authorized, defensive assistance relevant to the user's task. Do not try to bypass safety controls or restate disallowed details. If the remaining request cannot be completed under those constraints, explain briefly and ask for a narrower next step.";

#[derive(Debug, Default)]
pub(super) struct SafetyPolicyRecoveryState {
    attempted_after_user_turn: bool,
    recovery_turn_in_flight: bool,
}

impl SafetyPolicyRecoveryState {
    pub(super) fn reset_for_new_user_turn(&mut self) {
        self.attempted_after_user_turn = false;
        self.recovery_turn_in_flight = false;
    }

    pub(super) fn mark_recovery_turn_submitted(&mut self) {
        self.attempted_after_user_turn = true;
        self.recovery_turn_in_flight = true;
    }

    pub(super) fn finish_turn(&mut self) -> bool {
        let was_recovery_turn = self.recovery_turn_in_flight;
        self.recovery_turn_in_flight = false;
        was_recovery_turn
    }

    fn can_submit_recovery(&self) -> bool {
        !self.attempted_after_user_turn && !self.recovery_turn_in_flight
    }
}

impl ChatWidget {
    pub(super) fn record_user_submission_for_safety_policy_recovery(
        &mut self,
        history_record: &UserMessageHistoryRecord,
        turn_running_before_submit: bool,
    ) {
        if matches!(
            history_record,
            UserMessageHistoryRecord::InternalSafetyPolicyRecovery
        ) {
            return;
        }
        if !turn_running_before_submit {
            self.safety_policy_recovery.reset_for_new_user_turn();
        }
    }

    pub(super) fn finish_safety_policy_recovery_turn(&mut self) -> bool {
        self.safety_policy_recovery.finish_turn()
    }

    pub(super) fn maybe_submit_safety_policy_recovery(&mut self) -> bool {
        if !self.safety_policy_recovery.can_submit_recovery()
            || !self.is_session_configured()
            || self.blocks_direct_input
            || self.misalignment_policy_violation.is_some()
            || self.input_queue.suppress_queue_autosend
            || !self.input_queue.pending_steers.is_empty()
            || self.input_queue.has_queued_follow_up_messages()
            || !self.bottom_pane.no_modal_or_popup_active()
            || !self.bottom_pane.composer_is_empty()
            || !self.bottom_pane.composer_pending_pastes().is_empty()
        {
            return false;
        }

        self.safety_policy_recovery.mark_recovery_turn_submitted();
        let submitted = self.submit_user_message_with_history_record(
            UserMessage::from(SAFETY_POLICY_RECOVERY_PROMPT),
            UserMessageHistoryRecord::InternalSafetyPolicyRecovery,
        );
        if !submitted {
            self.safety_policy_recovery.finish_turn();
        }
        submitted
    }
}
