//! Session state, held apart from any surface that renders it.
//!
//! This is what a session *is* — identity, metadata, confinement root, history,
//! active agent — as opposed to what a surface shows about it. It still carries
//! a few view-only fields and one rendering type; those move out next, and until
//! they do this module is not yet listed in the surface-boundary check.

use agens_core::{AgentDefinition, Message, SessionMetadata};
use agens_store::SessionStore;
use agens_tools::{AgentModelValidator, EffectiveCapabilitySet, ToolDispatcher};

use crate::provider::ProviderKind;
use agens_models::ModelSelection;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionContext {
    pub identifier: Option<i64>,
    pub metadata: Option<SessionMetadata>,
    /// The confinement root recorded on a resumed session, read back from its own persisted
    /// `confinement_root` rather than re-derived from the current process's working directory.
    /// `None` for a session that has not been created yet; see
    /// [`crate::session::root::resolve_tui_session_root`] for the fallback that applies then.
    pub confinement_root: Option<std::path::PathBuf>,
    pub messages: Vec<Message>,
    /// The turns this session can take back. Undone turns leave their messages
    /// in `messages` and are excluded by [`SessionContext::live_messages`]
    /// until the next prompt commits the undo.
    pub undo: crate::undo::UndoHistory,
    /// The last prompt that was expanded before being sent, kept only until the
    /// turn it belongs to records itself. See
    /// [`SessionContext::remember_expanded_prompt`].
    pub expanded_prompt: Option<ExpandedPrompt>,
    pub active_agent: Option<ActiveAgentRuntime>,
    pub pending_system_reminder: Option<String>,
    pub selection: Option<ModelSelection>,
    pub provider: Option<ProviderKind>,
    pub chatgpt_unavailable: bool,
    pub resume_error: Option<String>,
    pub resume_notice: Option<String>,
    pub agent_correction_pending: bool,
    pub resume_draft: Option<ResumeDraft>,
    /// Durable media ids staged for the next user turn (composer chips / retry restore).
    /// Surfaces call store ingest; this holds only ids + mimes, never blob bytes or paths.
    pub pending_media_ids: Vec<i64>,
    pub pending_media_mimes: Vec<String>,
    pub selected_subagent: Option<String>,
    /// Why a turn of this session could not be bracketed by snapshots, when
    /// one could not. Set when opening the snapshot repository or capturing
    /// fails, cleared once a turn is bracketed successfully again, and never
    /// set for a project that simply is not a git worktree. Lets `/undo`
    /// explain that turns went unrecorded instead of claiming there is
    /// nothing to undo.
    pub snapshot_degraded: Option<String>,
    pub dangerous_mode: bool,
    /// Whether `Ask` permission prompts are bypassed for this session. Seeded from
    /// `agent.bypass_permission_prompts` (global configuration only) for a brand-new session, but
    /// carried as the session's OWN recorded value on resume rather than re-seeded — see
    /// [`crate::root::resolve_tui_session_root`]'s sibling in `agens-bootstrap` for the
    /// configuration read, and `agens-store`'s `bypass_permission_prompts` accessor for the
    /// persisted value a resume reads back.
    pub bypass_permissions: bool,
    pub running: bool,
}

/// A prompt as the reader typed it, next to the text that was sent for it.
///
/// A slash command or a skill reaches the provider as its expanded body, which
/// can run to kilobytes and is not what the reader would recognise as their
/// prompt. Keeping both lets an undo hand back the invocation instead.
#[derive(Clone, PartialEq, Eq)]
pub struct ExpandedPrompt {
    typed: String,
    expanded: String,
}

impl std::fmt::Debug for ExpandedPrompt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ExpandedPrompt([REDACTED])")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResumeDraft(String);

impl ResumeDraft {
    pub fn new(prompt: String) -> Self {
        Self(prompt)
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::ops::Deref for ResumeDraft {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Debug for ResumeDraft {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ResumeDraft([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveAgentRuntime {
    pub name: String,
    pub model: Option<String>,
    pub system_prompt: String,
    pub capabilities: EffectiveCapabilitySet,
}
impl ActiveAgentRuntime {
    pub fn build(
        agent: &AgentDefinition,
        inherited_model: Option<&str>,
        project: &str,
        dispatcher: &ToolDispatcher,
        validator: &dyn AgentModelValidator,
    ) -> Result<Self, AgentRotationError> {
        if agent
            .model
            .as_deref()
            .is_some_and(|model| validator.validate_model(model).is_err())
        {
            return Err(AgentRotationError::ModelUnavailable);
        }
        let model = agent
            .model
            .as_deref()
            .or(inherited_model)
            .map(str::to_owned);
        Ok(Self {
            name: agent.name.clone(),
            model,
            system_prompt: agent.system_prompt.clone(),
            capabilities: EffectiveCapabilitySet::from_agent(agent, project, dispatcher),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedSubagentTurn {
    pub id: u64,
    pub agent: String,
    pub task: String,
    pub final_result: String,
    pub tool_uses: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMutationError {
    Busy,
}

/// Why the turns a reader took back could not be dropped for good.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoCommitError {
    /// The session has persisted history but no store was handed in to truncate
    /// it. Committing memory alone would let the store hand the undone turn
    /// back on the next completed turn.
    StoreUnavailable,
    Storage(agens_store::SessionStoreError),
}

pub fn reset_session(context: &mut SessionContext) -> Result<(), SessionMutationError> {
    if context.running {
        return Err(SessionMutationError::Busy);
    }

    *context = SessionContext::fresh();
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRotationError {
    Busy,
    ModelUnavailable,
    Persistence,
}
pub fn rotate_active_agent(
    context: &mut SessionContext,
    candidate: &AgentDefinition,
    inherited_model: Option<&str>,
    project: &str,
    dispatcher: &ToolDispatcher,
    validator: &dyn AgentModelValidator,
    store: Option<&mut SessionStore>,
) -> Result<(), AgentRotationError> {
    if context.running {
        return Err(AgentRotationError::Busy);
    }
    let next =
        ActiveAgentRuntime::build(candidate, inherited_model, project, dispatcher, validator)?;
    let reminder = context.active_agent.as_ref().and_then(|current| {
        next.capabilities
            .is_expansion_from(&current.capabilities)
            .then(|| {
                format!(
                    "Agent capabilities expanded: {} -> {}.",
                    current.name, next.name
                )
            })
    });

    let metadata = match (&context.metadata, store) {
        (Some(metadata), Some(store)) => {
            let mut metadata = metadata.clone();
            metadata.active_agent = next.name.clone();
            metadata.updated_at = session_timestamp().ok_or(AgentRotationError::Persistence)?;
            store
                .update_session(&metadata)
                .map_err(|_| AgentRotationError::Persistence)?;
            Some(metadata)
        }
        (Some(_), None) => return Err(AgentRotationError::Persistence),
        (None, _) => None,
    };

    context.active_agent = Some(next);
    context.metadata = metadata;
    if reminder.is_some() {
        context.pending_system_reminder = reminder;
    }

    Ok(())
}

fn session_timestamp() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
}

pub fn current_session_timestamp() -> i64 {
    session_timestamp().unwrap_or_default()
}

impl SessionContext {
    pub fn fresh() -> Self {
        Self::default()
    }

    /// The messages that are still part of the conversation.
    ///
    /// An undone turn's messages stay in `messages` so a redo can be exact, but
    /// they are not what the model is asked to continue from — this is the
    /// history every request is built against.
    pub fn live_messages(&self) -> &[Message] {
        let live = self.undo.visible_message_count(self.messages.len());
        &self.messages[..live]
    }

    /// Remembers what the reader typed for a prompt that was expanded before it
    /// was sent, so the turn recording itself can prefer the invocation over the
    /// expansion.
    pub fn remember_expanded_prompt(&mut self, typed: String, expanded: String) {
        self.expanded_prompt = Some(ExpandedPrompt { typed, expanded });
    }

    /// The text the reader typed for `expanded`, if `expanded` is the expansion
    /// that was last remembered.
    ///
    /// Takes either way: a remembered prompt belongs to one submission, and a
    /// submission that never reached a turn must not be handed to a later one.
    pub fn take_typed_prompt_for(&mut self, expanded: &str) -> Option<String> {
        self.expanded_prompt
            .take()
            .filter(|prompt| prompt.expanded == expanded)
            .map(|prompt| prompt.typed)
    }

    /// Drops the messages an undo held back, from this history and from the
    /// session's persisted one, and ends the undo.
    ///
    /// Called when the next prompt arrives: that submission is the reader
    /// choosing the new direction, and the turns they took back stop being
    /// recoverable at that moment.
    ///
    /// The store is truncated first and a failure is propagated before anything
    /// in memory moves, so the two cannot end up disagreeing: a turn the store
    /// still holds would be reloaded into this history by the next completed
    /// turn and sent to the model. A session with no identifier has persisted
    /// nothing yet and needs no store.
    ///
    /// The truncation is bounded by the length of this history as well as by the
    /// surviving prefix. A turn persisted out of band since this history was
    /// last loaded — a sub-agent turn completing in the background — sits past
    /// that length in the store and was never part of what the reader took back,
    /// so it stays; the next completed turn reloads it into this history.
    ///
    /// That bound is a length, not an identity, and it covers only what the store
    /// holds beyond this history. Everything this history holds past the
    /// surviving prefix is dropped, and the matching range is deleted from the
    /// store. Matching one against the other is therefore only sound while this
    /// history holds its messages in the order the store recorded them, which is
    /// what a caller re-adopting a turn's history has to preserve: a sub-agent
    /// turn persisted mid-turn belongs where it was persisted, not at the tail.
    pub fn commit_undo(&mut self, store: Option<&mut SessionStore>) -> Result<(), UndoCommitError> {
        if !self.undo.has_undone_turns() {
            return Ok(());
        }

        let surviving = self.undo.visible_message_count(self.messages.len());

        if let Some(identifier) = self.identifier {
            let store = store.ok_or(UndoCommitError::StoreUnavailable)?;
            store
                .truncate_session_history(identifier, surviving, self.messages.len())
                .map_err(UndoCommitError::Storage)?;
        }

        self.undo.commit(self.messages.len());
        self.messages.truncate(surviving);

        Ok(())
    }

    /// A resumed context assembled directly. Not test-gated: its callers are
    /// tests in another crate, which `cfg(test)` cannot reach.
    pub fn resumed(
        identifier: i64,
        metadata: SessionMetadata,
        messages: Vec<Message>,
        active_agent: ActiveAgentRuntime,
        confinement_root: std::path::PathBuf,
    ) -> Self {
        Self {
            identifier: Some(identifier),
            metadata: Some(metadata),
            confinement_root: Some(confinement_root),
            messages,
            undo: crate::undo::UndoHistory::default(),
            expanded_prompt: None,
            active_agent: Some(active_agent),
            pending_system_reminder: None,
            selection: None,
            provider: None,
            chatgpt_unavailable: false,
            resume_error: None,
            resume_notice: None,
            agent_correction_pending: false,
            resume_draft: None,
            pending_media_ids: Vec::new(),
            pending_media_mimes: Vec::new(),
            selected_subagent: None,
            snapshot_degraded: None,
            dangerous_mode: false,
            bypass_permissions: false,
            running: false,
        }
    }

    /// Builds a session context for a session that was just resumed from storage.
    ///
    /// `confinement_root` is required, not optional: a resumed session's tools must always be
    /// confined to the root recorded for it, and making the parameter mandatory here rules out a
    /// caller silently constructing a "resumed" context that falls back to the process's own
    /// discovered root through [`crate::session::root::resolve_tui_session_root`]'s `None` branch.
    pub fn restored(
        identifier: i64,
        metadata: SessionMetadata,
        messages: Vec<Message>,
        confinement_root: std::path::PathBuf,
    ) -> Self {
        Self {
            identifier: Some(identifier),
            metadata: Some(metadata),
            confinement_root: Some(confinement_root),
            messages,
            undo: crate::undo::UndoHistory::default(),
            expanded_prompt: None,
            active_agent: None,
            pending_system_reminder: None,
            selection: None,
            provider: None,
            chatgpt_unavailable: false,
            resume_error: None,
            resume_notice: None,
            agent_correction_pending: false,
            resume_draft: None,
            pending_media_ids: Vec::new(),
            pending_media_mimes: Vec::new(),
            selected_subagent: None,
            snapshot_degraded: None,
            dangerous_mode: false,
            bypass_permissions: false,
            running: false,
        }
    }

    /// Stages a durable media attachment for the next user turn.
    pub fn push_pending_media(&mut self, media_id: i64, mime: String) {
        self.pending_media_ids.push(media_id);
        self.pending_media_mimes.push(mime);
    }

    /// Takes staged media for the next turn (clears pending).
    pub fn take_pending_media(&mut self) -> (Vec<i64>, Vec<String>) {
        (
            std::mem::take(&mut self.pending_media_ids),
            std::mem::take(&mut self.pending_media_mimes),
        )
    }

    /// Composer chip labels for staged media (`[Image #N]` / `[File #N]`).
    pub fn pending_media_chip_labels(&self) -> Vec<String> {
        self.pending_media_mimes
            .iter()
            .enumerate()
            .map(|(index, mime)| agens_store::media_chip_label(index + 1, mime))
            .collect()
    }

    pub fn note(&self) -> String {
        if let Some(notice) = &self.resume_notice {
            return notice.clone();
        }
        if let Some(error) = &self.resume_error {
            return error.clone();
        }
        let identifier = self
            .identifier
            .expect("resumed TUI session context always has an identifier");
        let metadata = self
            .metadata
            .as_ref()
            .expect("resumed TUI session context always has metadata");
        format!(
            "Resumed session {identifier}: agent={} turns={}",
            metadata.active_agent, metadata.completed_turn_count
        )
    }
}

#[cfg(test)]
mod tests {
    use agens_core::{CompletedSessionTurn, MessagePart, Role, SessionMessage};

    use super::*;

    /// A temporary store directory that removes itself when the test ends,
    /// whether it ends by returning or by panicking on a failed assertion.
    struct StoreDirectory(std::path::PathBuf);

    impl StoreDirectory {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for StoreDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store_directory(label: &str) -> StoreDirectory {
        StoreDirectory(agens_fixtures::session_directory(&format!("undo-{label}")))
    }

    fn text(role: Role, body: &str) -> Message {
        Message {
            role,
            parts: vec![MessagePart::Text(body.into())],
        }
    }

    fn completed_turn(prompt: &str, answer: &str) -> CompletedSessionTurn {
        CompletedSessionTurn::new(
            [text(Role::User, prompt), text(Role::Assistant, answer)]
                .into_iter()
                .map(SessionMessage::try_from)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        )
        .unwrap()
    }

    /// A session with two persisted turns, held in a context that has taken the second one back.
    fn session_with_an_undone_turn(label: &str) -> (StoreDirectory, SessionStore, SessionContext) {
        let directory = store_directory(label);
        let mut store = SessionStore::open(directory.path()).unwrap();
        let mut metadata = SessionMetadata {
            id: 0,
            project: "project".into(),
            title: "title".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
        };
        metadata = store
            .persist_completed_session_turn(&metadata, &completed_turn("first", "kept"))
            .unwrap();
        metadata = store
            .persist_completed_session_turn(&metadata, &completed_turn("second", "taken back"))
            .unwrap();

        let stored = store.load_session_for_resume(metadata.id).unwrap();
        let mut context = SessionContext::restored(
            metadata.id,
            stored.metadata,
            stored.messages,
            std::path::PathBuf::from("project"),
        );
        context.undo.record(crate::undo::UndoStep::new(
            "second".into(),
            2,
            "before".into(),
            "after".into(),
        ));
        context.undo.undo().expect("a turn to take back");

        (directory, store, context)
    }

    /// The defect this exists for: without the store truncation the taken-back turn is reloaded
    /// into the history by the next completed turn and sent to the model again.
    #[test]
    fn committing_an_undo_drops_the_taken_back_turn_from_the_store() {
        let (_directory, mut store, mut context) = session_with_an_undone_turn("commits");
        let identifier = context.identifier.expect("a persisted session");

        context.commit_undo(Some(&mut store)).unwrap();

        assert_eq!(context.messages.len(), 2);
        let stored = store.load_session_for_resume(identifier).unwrap();
        assert_eq!(stored.messages, context.messages);
        assert_eq!(stored.metadata.completed_turn_count, 1);
    }

    /// A turn persisted while the reader was deciding — a sub-agent turn finishing in the
    /// background — was never taken back, so committing the undo must not take it with the turn
    /// that was.
    #[test]
    fn committing_an_undo_keeps_a_turn_persisted_after_this_history_was_loaded() {
        let (_directory, mut store, mut context) = session_with_an_undone_turn("out-of-band");
        let identifier = context.identifier.expect("a persisted session");
        let metadata = context.metadata.clone().expect("persisted metadata");
        store
            .persist_completed_session_turn(
                &metadata,
                &completed_turn("a sub-agent task", "a sub-agent turn"),
            )
            .unwrap();

        context.commit_undo(Some(&mut store)).unwrap();

        assert_eq!(context.messages.len(), 2);
        let stored = store.load_session_for_resume(identifier).unwrap();
        assert_eq!(stored.messages.len(), 4);
        assert_eq!(stored.messages[..2], context.messages[..]);
        assert_eq!(
            stored.messages[2..],
            [
                text(Role::User, "a sub-agent task"),
                text(Role::Assistant, "a sub-agent turn"),
            ],
            "the background turn stays, and the next completed turn reloads it"
        );
        assert_eq!(stored.metadata.completed_turn_count, 2);
    }

    /// A sub-agent turn persisted while an earlier foreground turn was running sits before the turn
    /// the reader takes back, in the store and in the history in hand alike. The bound derived from
    /// that history then falls after it, so the commit takes back one turn and leaves the turn
    /// nobody undid in place on both sides.
    #[test]
    fn a_turn_persisted_before_the_taken_back_turn_survives_the_commit() {
        let directory = store_directory("store-order");
        let mut store = SessionStore::open(directory.path()).unwrap();
        let mut metadata = SessionMetadata {
            id: 0,
            project: "project".into(),
            title: "title".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
        };
        for turn in [
            completed_turn("first", "kept"),
            completed_turn("a sub-agent task", "a sub-agent turn"),
            completed_turn("second", "taken back"),
        ] {
            metadata = store
                .persist_completed_session_turn(&metadata, &turn)
                .unwrap();
        }

        let stored = store.load_session_for_resume(metadata.id).unwrap();
        let before_the_taken_back_turn = stored.messages[..4].to_vec();
        let mut context = SessionContext::restored(
            metadata.id,
            stored.metadata,
            stored.messages.clone(),
            std::path::PathBuf::from("project"),
        );
        let boundary = crate::undo::turn_boundary(&before_the_taken_back_turn, &context.messages);
        assert_eq!(boundary, 4, "the taken-back turn is the only one held back");
        context.undo.record(crate::undo::UndoStep::new(
            "second".into(),
            boundary,
            "before".into(),
            "after".into(),
        ));
        context.undo.undo().expect("a turn to take back");

        context.commit_undo(Some(&mut store)).unwrap();

        assert_eq!(context.messages, before_the_taken_back_turn);
        let remaining = store.load_session_for_resume(metadata.id).unwrap();
        assert_eq!(
            remaining.messages, before_the_taken_back_turn,
            "the sub-agent turn was never taken back, so it stays in the store"
        );
        assert_eq!(
            remaining.messages[2..],
            [
                text(Role::User, "a sub-agent task"),
                text(Role::Assistant, "a sub-agent turn"),
            ]
        );
    }

    /// Memory and the store must never part company quietly: a commit that cannot reach the store
    /// leaves the undo exactly where it was, so the reader is told rather than served a history
    /// the store will contradict.
    #[test]
    fn a_commit_that_cannot_reach_the_store_changes_nothing() {
        let (_directory, _store, mut context) = session_with_an_undone_turn("unreachable");
        let messages = context.messages.clone();

        assert_eq!(
            context.commit_undo(None),
            Err(UndoCommitError::StoreUnavailable)
        );
        assert_eq!(context.messages, messages);
        assert!(context.undo.has_undone_turns());
        assert_eq!(context.live_messages().len(), 2);
    }

    /// A session that never reached the store has nothing to truncate, which is not a failure.
    #[test]
    fn committing_an_undo_on_an_unpersisted_session_needs_no_store() {
        let mut context = SessionContext::fresh();
        context.messages = vec![
            text(Role::User, "first"),
            text(Role::Assistant, "kept"),
            text(Role::User, "second"),
            text(Role::Assistant, "taken back"),
        ];
        context.undo.record(crate::undo::UndoStep::new(
            "second".into(),
            2,
            "before".into(),
            "after".into(),
        ));
        context.undo.undo().expect("a turn to take back");

        assert_eq!(context.commit_undo(None), Ok(()));
        assert_eq!(context.messages.len(), 2);
        assert!(!context.undo.has_undone_turns());
    }

    #[test]
    fn bypass_permissions_defaults_to_false_for_a_fresh_session() {
        assert!(!SessionContext::fresh().bypass_permissions);
    }

    #[test]
    fn bypass_permissions_defaults_to_false_for_a_restored_session() {
        let metadata = SessionMetadata {
            id: 1,
            project: "project".into(),
            title: "title".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: true,
        };
        let restored =
            SessionContext::restored(1, metadata, Vec::new(), std::path::PathBuf::from("project"));

        assert!(!restored.bypass_permissions);
    }
}
