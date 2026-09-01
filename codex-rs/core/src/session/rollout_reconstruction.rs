use super::*;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::is_user_turn_boundary;
use codex_history::ResponseItemEnvelope;
use codex_protocol::protocol::SessionContextWindow;
use uuid::Uuid;

// Return value of `Session::reconstruct_history_from_rollout`, bundling the rebuilt history with
// the resume/fork hydration metadata derived from the same replay.
#[derive(Debug)]
pub(super) struct RolloutReconstruction {
    pub(super) history: Vec<ResponseItemEnvelope>,
    pub(super) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(super) reference_context_item: Option<TurnContextItem>,
    pub(super) world_state_baseline: Option<WorldStateSnapshot>,
    pub(super) window_number: u64,
    pub(super) first_window_id: Option<Uuid>,
    pub(super) previous_window_id: Option<Uuid>,
    pub(super) window_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy)]
struct ReconstructedWindow {
    number: u64,
    first_id: Option<Uuid>,
    previous_id: Option<Uuid>,
    id: Option<Uuid>,
}

#[derive(Debug)]
struct RolloutReconstructionPlan {
    base_replacement_history_index: Option<usize>,
    rollout_suffix_start: usize,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: TurnReferenceContextItem,
    world_state_replay_indices: Vec<usize>,
    window: ReconstructedWindow,
}

#[derive(Debug, Default)]
enum TurnReferenceContextItem {
    /// No `TurnContextItem` has been seen for this replay span yet.
    ///
    /// This differs from `Cleared`: `NeverSet` means there is no evidence this turn ever
    /// established a baseline, while `Cleared` means a baseline existed and a later compaction
    /// invalidated it. Only the latter must emit an explicit clearing segment for resume/fork
    /// hydration.
    #[default]
    NeverSet,
    /// A previously established baseline was invalidated by later compaction.
    Cleared,
    /// The latest baseline established by this replay span.
    Latest(Box<TurnContextItem>),
}

#[derive(Debug, Default)]
struct ActiveReplaySegment {
    turn_id: Option<String>,
    counts_as_user_turn: bool,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: TurnReferenceContextItem,
    world_state_replay_indices: Vec<usize>,
    base_replacement_history_index: Option<usize>,
    window: Option<ReconstructedWindow>,
}

fn turn_ids_are_compatible(active_turn_id: Option<&str>, item_turn_id: Option<&str>) -> bool {
    active_turn_id
        .is_none_or(|turn_id| item_turn_id.is_none_or(|item_turn_id| item_turn_id == turn_id))
}

fn finalize_active_segment(
    active_segment: ActiveReplaySegment,
    base_replacement_history_index: &mut Option<usize>,
    previous_turn_settings: &mut Option<PreviousTurnSettings>,
    reference_context_item: &mut TurnReferenceContextItem,
    world_state_replay_indices: &mut Vec<usize>,
    window: &mut Option<ReconstructedWindow>,
    pending_rollback_turns: &mut usize,
) {
    // Thread rollback drops the newest surviving real user-message boundaries. In replay, that
    // means skipping the next finalized segments that contain a non-contextual
    // `EventMsg::UserMessage`.
    if *pending_rollback_turns > 0 {
        if active_segment.counts_as_user_turn {
            *pending_rollback_turns -= 1;
        }
        return;
    }

    world_state_replay_indices.extend(active_segment.world_state_replay_indices);

    // A surviving replacement-history checkpoint is a complete history base. Once we
    // know the newest surviving one, older rollout items do not affect rebuilt history.
    if base_replacement_history_index.is_none()
        && let Some(segment_base_replacement_history_index) =
            active_segment.base_replacement_history_index
    {
        *base_replacement_history_index = Some(segment_base_replacement_history_index);
    }

    if window.is_none() {
        *window = active_segment.window;
    }

    // `previous_turn_settings` come from the newest surviving user turn that established them.
    if previous_turn_settings.is_none() && active_segment.counts_as_user_turn {
        *previous_turn_settings = active_segment.previous_turn_settings;
    }

    // `reference_context_item` comes from the newest surviving user turn baseline, or
    // from a surviving compaction that explicitly cleared that baseline.
    if matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
        && (active_segment.counts_as_user_turn
            || matches!(
                active_segment.reference_context_item,
                TurnReferenceContextItem::Cleared
            ))
    {
        *reference_context_item = active_segment.reference_context_item;
    }
}

fn plan_rollout_reconstruction(rollout_items: &[RolloutItem]) -> RolloutReconstructionPlan {
    // Replay metadata should already match the shape of the future lazy reverse loader, even
    // while history materialization still uses an eager bridge. Scan newest-to-oldest,
    // stopping once a surviving replacement-history checkpoint and the required resume metadata
    // are both known; then replay only the buffered surviving tail forward to preserve exact
    // history semantics.
    let has_legacy_compaction_without_window_number = rollout_items.iter().any(|item| {
        matches!(item, RolloutItem::Compacted(compacted) if compacted.window_number.is_none())
    });
    let initial_window = if has_legacy_compaction_without_window_number {
        None
    } else {
        rollout_items.iter().find_map(|item| match item {
            RolloutItem::SessionMeta(session_meta) => session_meta
                .meta
                .context_window
                .as_ref()
                .and_then(reconstructed_window_from_session_context_window),
            _ => None,
        })
    };
    let mut base_replacement_history_index = None;
    let mut previous_turn_settings = None;
    let mut reference_context_item = TurnReferenceContextItem::NeverSet;
    let mut world_state_replay_indices = Vec::new();
    let mut window = None;
    // Rollback is "drop the newest N user turns". While scanning in reverse, that becomes
    // "skip the next N user-turn segments we finalize".
    let mut pending_rollback_turns = 0usize;
    // Index after the newest surviving replacement-history checkpoint. If no such checkpoint
    // exists, this remains zero and the replay suffix is the full rollout.
    let mut rollout_suffix_start = 0usize;
    // Reverse replay accumulates rollout items into the newest in-progress turn segment until
    // we hit its matching `TurnStarted`, at which point the segment can be finalized.
    let mut active_segment: Option<ActiveReplaySegment> = None;

    for (index, item) in rollout_items.iter().enumerate().rev() {
        match item {
            RolloutItem::Compacted(compacted) => {
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                active_segment.world_state_replay_indices.push(index);
                if active_segment.window.is_none()
                    && let Some(window_number) = compacted.window_number
                {
                    active_segment.window = Some(ReconstructedWindow {
                        number: window_number,
                        first_id: compacted.first_window_id.as_deref().and_then(parse_uuid_v7),
                        previous_id: compacted
                            .previous_window_id
                            .as_deref()
                            .and_then(parse_uuid_v7),
                        id: compacted.window_id.as_deref().and_then(parse_uuid_v7),
                    });
                }
                // Looking backward, compaction clears any older baseline unless a newer
                // `TurnContextItem` in this same segment has already re-established it.
                if matches!(
                    active_segment.reference_context_item,
                    TurnReferenceContextItem::NeverSet
                ) {
                    active_segment.reference_context_item = TurnReferenceContextItem::Cleared;
                }
                if active_segment.base_replacement_history_index.is_none()
                    && compacted.replacement_history.is_some()
                {
                    active_segment.base_replacement_history_index = Some(index);
                    rollout_suffix_start = index + 1;
                }
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                pending_rollback_turns = pending_rollback_turns
                    .saturating_add(usize::try_from(rollback.num_turns).unwrap_or(usize::MAX));
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                // Reverse replay often sees `TurnComplete` before any turn-scoped metadata.
                // Capture the turn id early so later `TurnContext` / abort items can match it.
                if active_segment.turn_id.is_none() {
                    active_segment.turn_id = Some(event.turn_id.clone());
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                if let Some(active_segment) = active_segment.as_mut() {
                    if active_segment.turn_id.is_none()
                        && let Some(turn_id) = &event.turn_id
                    {
                        active_segment.turn_id = Some(turn_id.clone());
                    }
                } else if let Some(turn_id) = &event.turn_id {
                    active_segment = Some(ActiveReplaySegment {
                        turn_id: Some(turn_id.clone()),
                        ..Default::default()
                    });
                }
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                active_segment.counts_as_user_turn = true;
            }
            RolloutItem::TurnContext(ctx) => {
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                // `TurnContextItem` can attach metadata to an existing segment, but only a
                // real `UserMessage` event should make the segment count as a user turn.
                if active_segment.turn_id.is_none() {
                    active_segment.turn_id = ctx.turn_id.clone();
                }
                if turn_ids_are_compatible(
                    active_segment.turn_id.as_deref(),
                    ctx.turn_id.as_deref(),
                ) {
                    active_segment.previous_turn_settings = Some(PreviousTurnSettings {
                        model: ctx.model.clone(),
                        comp_hash: ctx.comp_hash.clone(),
                        realtime_active: ctx.realtime_active,
                    });
                    if matches!(
                        active_segment.reference_context_item,
                        TurnReferenceContextItem::NeverSet
                    ) {
                        active_segment.reference_context_item =
                            TurnReferenceContextItem::Latest(Box::new(ctx.clone()));
                    }
                }
            }
            RolloutItem::WorldState(_) => {
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                active_segment.world_state_replay_indices.push(index);
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                // `TurnStarted` is the oldest boundary of the active reverse segment.
                if active_segment.as_ref().is_some_and(|active_segment| {
                    turn_ids_are_compatible(
                        active_segment.turn_id.as_deref(),
                        Some(event.turn_id.as_str()),
                    )
                }) && let Some(active_segment) = active_segment.take()
                {
                    finalize_active_segment(
                        active_segment,
                        &mut base_replacement_history_index,
                        &mut previous_turn_settings,
                        &mut reference_context_item,
                        &mut world_state_replay_indices,
                        &mut window,
                        &mut pending_rollback_turns,
                    );
                }
            }
            RolloutItem::ResponseItem(response_item) => {
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                active_segment.counts_as_user_turn |= is_user_turn_boundary(&response_item.item);
            }
            RolloutItem::InterAgentCommunication(_) => {
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                active_segment.counts_as_user_turn = true;
            }
            RolloutItem::EventMsg(_)
            | RolloutItem::SessionMeta(_)
            | RolloutItem::RealtimeItem(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. } => {}
        }

        if base_replacement_history_index.is_some()
            && previous_turn_settings.is_some()
            && !matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
        {
            // At this point we have both eager resume metadata values and the replacement-
            // history base for the surviving tail, so older rollout items cannot affect this
            // result.
            break;
        }
    }

    if let Some(active_segment) = active_segment.take() {
        finalize_active_segment(
            active_segment,
            &mut base_replacement_history_index,
            &mut previous_turn_settings,
            &mut reference_context_item,
            &mut world_state_replay_indices,
            &mut window,
            &mut pending_rollback_turns,
        );
    }

    let fallback_window_number = u64::try_from(
        rollout_items
            .iter()
            .filter(|item| matches!(item, RolloutItem::Compacted(_)))
            .count(),
    )
    .unwrap_or(u64::MAX);
    let window = window.or(initial_window).unwrap_or(ReconstructedWindow {
        number: fallback_window_number,
        first_id: None,
        previous_id: None,
        id: None,
    });

    RolloutReconstructionPlan {
        base_replacement_history_index,
        rollout_suffix_start,
        previous_turn_settings,
        reference_context_item,
        world_state_replay_indices,
        window,
    }
}

impl Session {
    pub(super) async fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> RolloutReconstruction {
        let plan = plan_rollout_reconstruction(rollout_items);

        let mut history = ContextManager::new();
        let mut saw_legacy_compaction_without_replacement_history = false;
        if let Some(base_replacement_history_index) = plan.base_replacement_history_index
            && let RolloutItem::Compacted(compacted) =
                &rollout_items[base_replacement_history_index]
            && let Some(replacement_history) = &compacted.replacement_history
        {
            history.replace_annotated(replacement_history.clone());
        }
        // Materialize exact history semantics from the replay-derived suffix. The eventual lazy
        // design should keep this same replay shape, but drive it from a resumable reverse source
        // instead of an eagerly loaded `&[RolloutItem]`.
        for item in &rollout_items[plan.rollout_suffix_start..] {
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    history.record_annotated_items(
                        std::slice::from_ref(response_item),
                        turn_context.model_info().truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunication(communication) => {
                    let response_item = communication.to_model_input_item();
                    history.record_items(
                        std::iter::once(&response_item),
                        turn_context.model_info().truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunicationMetadata { .. } => {}
                RolloutItem::Compacted(compacted) => {
                    if let Some(replacement_history) = &compacted.replacement_history {
                        // This should actually never happen, because the reverse loop above (to build rollout_suffix)
                        // should stop before any compaction that has Some replacement_history
                        history.replace_annotated(replacement_history.clone());
                    } else {
                        saw_legacy_compaction_without_replacement_history = true;
                        // Legacy rollouts without `replacement_history` should rebuild the
                        // historical TurnContext at the correct insertion point from persisted
                        // `TurnContextItem`s. These are rare enough that we currently just clear
                        // `reference_context_item`, reinject canonical context at the end of the
                        // resumed conversation, and accept the temporary out-of-distribution
                        // prompt shape.
                        // TODO(ccunningham): if we drop support for None replacement_history compaction items,
                        // we can get rid of this second loop entirely and just build `history` directly in the first loop.
                        let user_messages =
                            compact::collect_annotated_user_messages(history.annotated_items());
                        let rebuilt = compact::build_compacted_history(
                            Vec::new(),
                            &user_messages,
                            &compacted.message,
                        );
                        history.replace_annotated(rebuilt);
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    history.drop_last_n_user_turns(rollback.num_turns);
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::RealtimeItem(_)
                | RolloutItem::WorldState(_)
                | RolloutItem::SecurityRiskScore(_)
                | RolloutItem::SessionMeta(_) => {}
            }
        }

        let reference_context_item = reference_context_item_from_replay(
            plan.reference_context_item,
            saw_legacy_compaction_without_replacement_history,
        );
        let world_state_baseline =
            reconstruct_world_state_baseline(rollout_items, plan.world_state_replay_indices);

        RolloutReconstruction {
            history: history.into_annotated_items(),
            previous_turn_settings: plan.previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number: plan.window.number,
            first_window_id: plan.window.first_id,
            previous_window_id: plan.window.previous_id,
            window_id: plan.window.id,
        }
    }

    pub(super) async fn reconstruct_history_from_rollout_owned(
        &self,
        turn_context: &TurnContext,
        mut rollout_items: Vec<RolloutItem>,
    ) -> RolloutReconstruction {
        let plan = plan_rollout_reconstruction(&rollout_items);
        let world_state_baseline = reconstruct_world_state_baseline(
            &rollout_items,
            plan.world_state_replay_indices,
        );

        let mut history = ContextManager::new();
        let mut saw_legacy_compaction_without_replacement_history = false;
        if let Some(base_replacement_history_index) = plan.base_replacement_history_index
            && let Some(RolloutItem::Compacted(compacted)) =
                rollout_items.get_mut(base_replacement_history_index)
            && let Some(replacement_history) = compacted.replacement_history.take()
        {
            history.replace_annotated(replacement_history);
        }
        let rollout_suffix = rollout_items.split_off(plan.rollout_suffix_start);
        drop(rollout_items);
        for item in rollout_suffix {
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    history.record_annotated_items(
                        std::slice::from_ref(&response_item),
                        turn_context.model_info().truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunication(communication) => {
                    let response_item = communication.to_model_input_item();
                    history.record_items(
                        std::iter::once(&response_item),
                        turn_context.model_info().truncation_policy.into(),
                    );
                }
                RolloutItem::InterAgentCommunicationMetadata { .. } => {}
                RolloutItem::Compacted(mut compacted) => {
                    if let Some(replacement_history) = compacted.replacement_history.take() {
                        // This should actually never happen, because the reverse loop above (to build rollout_suffix)
                        // should stop before any compaction that has Some replacement_history
                        history.replace_annotated(replacement_history);
                    } else {
                        saw_legacy_compaction_without_replacement_history = true;
                        // Legacy rollouts without `replacement_history` should rebuild the
                        // historical TurnContext at the correct insertion point from persisted
                        // `TurnContextItem`s. These are rare enough that we currently just clear
                        // `reference_context_item`, reinject canonical context at the end of the
                        // resumed conversation, and accept the temporary out-of-distribution
                        // prompt shape.
                        // TODO(ccunningham): if we drop support for None replacement_history compaction items,
                        // we can get rid of this second loop entirely and just build `history` directly in the first loop.
                        let user_messages =
                            compact::collect_annotated_user_messages(history.annotated_items());
                        let rebuilt = compact::build_compacted_history(
                            Vec::new(),
                            &user_messages,
                            &compacted.message,
                        );
                        history.replace_annotated(rebuilt);
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    history.drop_last_n_user_turns(rollback.num_turns);
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::RealtimeItem(_)
                | RolloutItem::WorldState(_)
                | RolloutItem::SecurityRiskScore(_)
                | RolloutItem::SessionMeta(_) => {}
            }
        }

        let reference_context_item = reference_context_item_from_replay(
            plan.reference_context_item,
            saw_legacy_compaction_without_replacement_history,
        );

        RolloutReconstruction {
            history: history.into_annotated_items(),
            previous_turn_settings: plan.previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            window_number: plan.window.number,
            first_window_id: plan.window.first_id,
            previous_window_id: plan.window.previous_id,
            window_id: plan.window.id,
        }
    }
}

fn reference_context_item_from_replay(
    reference_context_item: TurnReferenceContextItem,
    saw_legacy_compaction_without_replacement_history: bool,
) -> Option<TurnContextItem> {
    if saw_legacy_compaction_without_replacement_history {
        return None;
    }
    match reference_context_item {
        TurnReferenceContextItem::NeverSet | TurnReferenceContextItem::Cleared => None,
        TurnReferenceContextItem::Latest(turn_reference_context_item) => {
            Some(*turn_reference_context_item)
        }
    }
}

fn reconstruct_world_state_baseline(
    rollout_items: &[RolloutItem],
    mut world_state_replay_indices: Vec<usize>,
) -> Option<WorldStateSnapshot> {
    // Segments and their contents were collected newest-first; replay the surviving records
    // chronologically so compaction resets and merge patches have their original meaning.
    world_state_replay_indices.reverse();
    let mut world_state_baseline: Option<WorldStateSnapshot> = None;
    for index in world_state_replay_indices {
        match &rollout_items[index] {
            RolloutItem::Compacted(_) => world_state_baseline = None,
            RolloutItem::WorldState(world_state) if world_state.full => {
                world_state_baseline = Some(WorldStateSnapshot::from(&world_state.state));
            }
            RolloutItem::WorldState(world_state) => {
                let Some(baseline) = world_state_baseline.as_mut() else {
                    tracing::warn!("ignored world-state patch without a full snapshot");
                    continue;
                };
                baseline.apply_merge_patch(&world_state.state);
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TurnContext(_)
            | RolloutItem::RealtimeItem(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::EventMsg(_) => {
                unreachable!("only world-state replay items are collected")
            }
        }
    }
    world_state_baseline
}

fn parse_uuid_v7(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value)
        .ok()
        .filter(|uuid| uuid.get_version_num() == 7)
}

fn reconstructed_window_from_session_context_window(
    context_window: &SessionContextWindow,
) -> Option<ReconstructedWindow> {
    let id = parse_uuid_v7(&context_window.window_id)?;
    Some(ReconstructedWindow {
        number: 0,
        first_id: Some(id),
        previous_id: None,
        id: Some(id),
    })
}
