//! The derivation itself: the normalized observation a reported fact reduces
//! to, the state that folding those observations builds, and the journal
//! entries each one is recorded as.
//!
//! `run_health` is derived, never a source of truth. The same fold runs over a
//! live fact and over the journal that fact was written to, which is what makes
//! the row recomputable: replaying a run's events from an empty state has to
//! land on the row the incremental path already wrote.

use std::collections::BTreeSet;

use agens_core::{FactPath, ToolOutcome, ToolResultFacts};
use agens_store::{EventClass, EventRow, EvidenceClass, RunHealthRow};
use serde_json::{Value, json};

use super::detectors::CheckpointStanding;

/// One reported fact, normalized to the shape the fold reads.
///
/// Normalizing here rather than in the fold is what keeps a noisy or buggy
/// agent from inflating the control plane's state: every value is drawn from a
/// closed set or a bounded field before anything is folded or written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Observation {
    /// A write or an edit that ran to completion and named a path.
    Mutation {
        path: String,
    },
    /// A completed mutation whose path the harness could not represent.
    UnrepresentableMutation,
    /// A call that reports no filesystem effect: a read, a search, or a
    /// mutation that failed or was denied.
    InertTool,
    CommandExit {
        code: i64,
    },
    TurnStarted,
    TurnEnded {
        tokens: i64,
    },
    ContextExhausted,
    /// The provider refused the turn for quota and the run parked on the
    /// reset. Like an exhausted context, and unlike an idle turn: the worker
    /// is waiting on something outside itself.
    QuotaReached,
    Checkpoint {
        evidence_class: EvidenceClass,
        claims_progress: bool,
    },
    CheckpointExpired,
    GenesisPathsFrozen {
        paths: Vec<String>,
    },
}

impl Observation {
    /// The typed facts of one tool call, reduced to what health derivation
    /// reads.
    ///
    /// A magnitude is present exactly when the call ran to completion, so a
    /// mutation without one measured nothing and is inert. `bash` reports only
    /// its own exit status; the paths a command touched are invisible to the
    /// passive layer and are re-derived from git at the delivery gate instead.
    pub(crate) fn from_tool_result(facts: &ToolResultFacts) -> Self {
        match facts {
            ToolResultFacts::Write {
                path,
                outcome: ToolOutcome::Succeeded,
                written: Some(_),
            } => Self::mutation(path),
            ToolResultFacts::Edit {
                path,
                outcome: ToolOutcome::Succeeded,
                changed: Some(magnitude),
            } if magnitude.lines_added + magnitude.lines_removed > 0 => Self::mutation(path),
            ToolResultFacts::Bash {
                exit_code: Some(code),
                ..
            } => Self::CommandExit {
                code: i64::from(*code),
            },
            _ => Self::InertTool,
        }
    }

    fn mutation(path: &FactPath) -> Self {
        path.relative()
            .map_or(Self::UnrepresentableMutation, |value| Self::Mutation {
                path: value.to_owned(),
            })
    }

    /// The journal entry type this observation is recorded under.
    const fn event_type(&self) -> &'static str {
        match self {
            Self::Mutation { .. }
            | Self::UnrepresentableMutation
            | Self::InertTool
            | Self::CommandExit { .. } => "tool_result_fact",
            Self::TurnStarted => "turn_started",
            Self::TurnEnded { .. } => "turn_ended",
            Self::ContextExhausted => "context_exhausted",
            Self::QuotaReached => "quota_parked",
            Self::Checkpoint { .. } => "checkpoint_recorded",
            Self::CheckpointExpired => "checkpoint_expired",
            Self::GenesisPathsFrozen { .. } => "genesis_paths_frozen",
        }
    }

    /// Facts the harness reported are agent behavior; what the coordinator
    /// derived around them is its own machinery.
    const fn class(&self) -> EventClass {
        match self {
            Self::CheckpointExpired | Self::GenesisPathsFrozen { .. } => EventClass::Infra,
            _ => EventClass::Agent,
        }
    }

    fn detail(&self, credited: bool) -> Value {
        match self {
            Self::Mutation { path } => json!({ "observation": "mutation", "path": path }),
            Self::UnrepresentableMutation => json!({ "observation": "unrepresentable_mutation" }),
            Self::InertTool => json!({ "observation": "inert" }),
            Self::CommandExit { code } => {
                json!({ "observation": "command_exit", "exit_code": code })
            }
            Self::TurnEnded { tokens } => json!({ "tokens": tokens }),
            Self::Checkpoint {
                evidence_class,
                claims_progress,
            } => json!({
                "evidence_class": evidence_class.as_str(),
                "claims_progress": claims_progress,
                "credited": credited,
            }),
            Self::GenesisPathsFrozen { paths } => json!({ "paths": paths }),
            Self::TurnStarted
            | Self::ContextExhausted
            | Self::QuotaReached
            | Self::CheckpointExpired => json!({}),
        }
    }

    /// The journal entry for this observation, carrying the attempt it belongs
    /// to so a fact is never attributed to a run's later attempt.
    pub(crate) fn to_event(
        &self,
        run_id: i64,
        attempt_id: Option<i64>,
        turn: i64,
        ts: i64,
        credited: bool,
    ) -> EventRow {
        let mut payload = json!({ "attempt_id": attempt_id, "turn": turn });
        if let (Some(target), Some(detail)) =
            (payload.as_object_mut(), self.detail(credited).as_object())
        {
            for (key, value) in detail {
                target.insert(key.clone(), value.clone());
            }
        }

        EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: self.event_type().to_owned(),
            class: self.class(),
            payload: payload.to_string(),
            ts,
        }
    }

    /// Reads back an observation this module journaled, with the turn it
    /// belonged to. Entries written by anything else return `None` and are
    /// skipped by a replay.
    pub(crate) fn from_event(event: &EventRow) -> Option<(Self, i64)> {
        let payload: Value = serde_json::from_str(&event.payload).ok()?;
        let turn = payload.get("turn").and_then(Value::as_i64).unwrap_or(0);

        let observation = match event.event_type.as_str() {
            "tool_result_fact" => match payload.get("observation").and_then(Value::as_str)? {
                "mutation" => Self::Mutation {
                    path: payload.get("path").and_then(Value::as_str)?.to_owned(),
                },
                "unrepresentable_mutation" => Self::UnrepresentableMutation,
                "command_exit" => Self::CommandExit {
                    code: payload.get("exit_code").and_then(Value::as_i64)?,
                },
                _ => Self::InertTool,
            },
            "turn_started" => Self::TurnStarted,
            "turn_ended" => Self::TurnEnded {
                tokens: payload.get("tokens").and_then(Value::as_i64).unwrap_or(0),
            },
            "context_exhausted" => Self::ContextExhausted,
            "quota_parked" => Self::QuotaReached,
            "checkpoint_recorded" => Self::Checkpoint {
                evidence_class: EvidenceClass::parse(
                    payload.get("evidence_class").and_then(Value::as_str)?,
                )?,
                claims_progress: payload
                    .get("claims_progress")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            "checkpoint_expired" => Self::CheckpointExpired,
            "genesis_paths_frozen" => Self::GenesisPathsFrozen {
                paths: payload
                    .get("paths")?
                    .as_array()?
                    .iter()
                    .filter_map(|path| path.as_str().map(str::to_owned))
                    .collect(),
            },
            _ => return None,
        };

        Some((observation, turn))
    }
}

/// How many consecutive commands must exit with the same code before it is a
/// signature rather than one failure. One failing run is ordinary; the same one
/// twice is a worker not getting past it.
const REPEATS_BEFORE_SIGNATURE: i64 = 2;

/// The whole derived state of one run, of which [`RunHealthRow`] is the
/// persisted projection.
///
/// The rest is what the fold needs between facts — whether this turn has seen
/// progress, which exit code is repeating, the frozen genesis paths — and every
/// bit of it is rebuilt by replaying the run's journal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HealthState {
    last_progress_turn: Option<i64>,
    noop_turns: i64,
    failing_test_signature: Option<String>,
    tokens_since_progress: i64,
    progress_this_turn: bool,
    parked_this_turn: bool,
    repeating_exit_code: Option<i64>,
    repeats: i64,
    genesis_paths: Option<BTreeSet<String>>,
    checkpoint: CheckpointStanding,
    /// Set while a lost-worker signal stands, so a stall raises the signal once
    /// rather than on every turn that keeps it true.
    lost_reported: bool,
}

/// What one folded observation asks the caller to do beyond storing the row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Folded {
    pub(crate) credited_progress: bool,
    /// A checkpoint arrived while the genesis paths were still unfrozen, so the
    /// caller reads the evidence ledger and freezes them if it recorded any
    /// touched path.
    pub(crate) freeze_due: bool,
    /// A mutation the frozen genesis paths do not contain.
    pub(crate) divergent_path: Option<String>,
    /// A completed mutation that cannot be compared against the frozen set.
    pub(crate) uncomparable_mutation: bool,
}

impl HealthState {
    /// Folds one observation into the derived state.
    ///
    /// Progress is observable, never claimed: a tool call that touched a path
    /// or moved a diff, or a checkpoint whose claim is deterministic. An
    /// inferential or insufficient claim is recorded and credits nothing.
    pub(crate) fn fold(&mut self, turn: i64, observation: &Observation) -> Folded {
        let mut folded = Folded::default();

        match observation {
            Observation::Mutation { path } => {
                self.credit_progress(turn);
                folded.credited_progress = true;
                folded.divergent_path = self
                    .genesis_paths
                    .as_ref()
                    .filter(|frozen| !frozen.contains(path))
                    .map(|_| path.clone());
            }
            Observation::UnrepresentableMutation => {
                self.credit_progress(turn);
                folded.credited_progress = true;
                folded.uncomparable_mutation = self.genesis_paths.is_some();
            }
            Observation::InertTool => {}
            Observation::CommandExit { code } => self.fold_exit_code(*code),
            Observation::TurnStarted => {
                self.progress_this_turn = false;
                self.parked_this_turn = false;
            }
            Observation::TurnEnded { tokens } => {
                self.fold_turn_end(*tokens);
                self.progress_this_turn = false;
                self.parked_this_turn = false;
            }
            Observation::ContextExhausted | Observation::QuotaReached => {
                self.parked_this_turn = true;
            }
            Observation::Checkpoint {
                evidence_class,
                claims_progress,
            } => {
                self.checkpoint = CheckpointStanding {
                    active: true,
                    claims_progress: *claims_progress,
                    expired: false,
                };
                folded.freeze_due = self.genesis_paths.is_none();

                if *claims_progress && *evidence_class == EvidenceClass::Deterministic {
                    self.credit_progress(turn);
                    folded.credited_progress = true;
                }
            }
            Observation::CheckpointExpired => self.checkpoint.expired = true,
            Observation::GenesisPathsFrozen { paths } => {
                self.genesis_paths = Some(paths.iter().cloned().collect());
            }
        }

        if folded.credited_progress {
            self.lost_reported = false;
        }

        folded
    }

    /// A turn that ended without observable progress is a noop turn, and its
    /// tokens were spent without moving anything.
    ///
    /// A turn parked by an exhausted context is neither: the worker is waiting
    /// on a compaction, not idling, and counting it would make every recovery
    /// look like a stall.
    fn fold_turn_end(&mut self, tokens: i64) {
        if self.progress_this_turn || self.parked_this_turn {
            return;
        }

        self.noop_turns = self.noop_turns.saturating_add(1);
        self.tokens_since_progress = self.tokens_since_progress.saturating_add(tokens);
    }

    fn fold_exit_code(&mut self, code: i64) {
        if code == 0 {
            self.repeating_exit_code = None;
            self.repeats = 0;
            self.failing_test_signature = None;
            return;
        }

        if self.repeating_exit_code == Some(code) {
            self.repeats = self.repeats.saturating_add(1);
        } else {
            self.repeating_exit_code = Some(code);
            self.repeats = 1;
        }

        if self.repeats >= REPEATS_BEFORE_SIGNATURE {
            self.failing_test_signature = Some(format!("bash:exit={code}"));
        }
    }

    fn credit_progress(&mut self, turn: i64) {
        self.last_progress_turn = Some(turn.max(0));
        self.noop_turns = 0;
        self.tokens_since_progress = 0;
        self.progress_this_turn = true;
    }

    pub(crate) const fn checkpoint(&self) -> CheckpointStanding {
        self.checkpoint
    }

    pub(crate) const fn lost_reported(&self) -> bool {
        self.lost_reported
    }

    pub(crate) const fn mark_lost_reported(&mut self) {
        self.lost_reported = true;
    }

    /// The persisted projection of this state.
    pub(crate) fn snapshot(&self, run_id: i64, now: i64) -> RunHealthRow {
        RunHealthRow {
            run_id,
            last_progress_turn: self.last_progress_turn,
            noop_turns: self.noop_turns,
            failing_test_signature: self.failing_test_signature.clone(),
            tokens_since_progress: self.tokens_since_progress,
            updated_at: now,
        }
    }
}
