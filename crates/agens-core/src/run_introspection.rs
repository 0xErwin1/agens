//! The two payloads a worker reports its own progress through: a checkpoint
//! and a question.
//!
//! Both are typed all the way down. Nothing here is prose a later stage parses
//! back into fields: a checkpoint arrives as claims with their proofs and their
//! evidence class already separated, and a question arrives as a blocked
//! decision with its options, because the consumers of both are mechanical.
//! Run health credits progress only for a `deterministic` claim, and the
//! coordinator parks a run on a question row it can deliver an answer to.
//!
//! The one rule the types enforce rather than describe is that a
//! [`EvidenceClass::Deterministic`] claim carries at least one proof reference.
//! A claim nobody can re-run is not deterministic no matter what the worker
//! calls it, and the whole point of the class is that something downstream acts
//! on it.
//!
//! Neither payload reads a clock. Timestamps are the caller's, the same way the
//! control-plane store takes them, so a coordinator reconciling after a restart
//! decides what "now" means.

use std::collections::BTreeSet;

pub const MAX_CHECKPOINT_CLAIMS: usize = 16;
pub const MAX_CLAIM_PROOF_REFS: usize = 8;
pub const MAX_CHECKPOINT_BLOCKERS: usize = 8;
pub const MAX_CHECKPOINT_TOUCHED_PATHS: usize = 256;
pub const MAX_CLAIM_DESCRIPTION_CHARS: usize = 1_024;
pub const MAX_PROOF_REF_CHARS: usize = 512;
pub const MAX_CHECKPOINT_GOAL_CHARS: usize = 1_024;
pub const MAX_CHECKPOINT_HYPOTHESIS_CHARS: usize = 1_024;
pub const MAX_BLOCKER_CHARS: usize = 512;
pub const MAX_TOUCHED_PATH_CHARS: usize = 1_024;
pub const MAX_CHECKPOINT_CHARS: usize = 16 * 1_024;

pub const MAX_ASK_OPTIONS: usize = 8;
pub const MAX_ASK_DECISION_CHARS: usize = 2_048;
pub const MAX_ASK_OPTION_ID_CHARS: usize = 64;
pub const MAX_ASK_OPTION_LABEL_CHARS: usize = 512;
pub const MAX_ASK_RECOMMENDATION_CHARS: usize = 1_024;
pub const MAX_ASK_CHARS: usize = 16 * 1_024;

/// The text a team-mode worker's prompt carries about what a checkpoint costs
/// and what it buys.
///
/// It states the mechanical consequence rather than asking for rigour in the
/// abstract, because the consequence is what is actually implemented: run
/// health credits progress for a `deterministic` claim and records the other
/// two without crediting it, so a worker that narrates instead of proving keeps
/// accumulating no-progress turns and is eventually surfaced as stalled.
///
/// Kept beside the payload it describes so the two cannot drift: the classes
/// named here are the variants of [`EvidenceClass`], and the proof rule it
/// states is the one [`Checkpoint::new`] enforces.
pub const WORKER_CHECKPOINT_PROMPT: &str = concat!(
    "Report a checkpoint at each milestone of your work, never once per turn: ",
    "the passive layer already records the fine-grained detail. A checkpoint ",
    "carries the evidence you gathered since the last one, the hypothesis you ",
    "are working from, the goal you are going for next, a revised estimate, ",
    "and anything blocking you.\n\n",
    "Every claim you make is classified. `deterministic` means a reader can ",
    "re-run your proof and get your result: a test invocation and its exit ",
    "code, a command and its output, a file and the lines you changed. ",
    "`inferential` means you reasoned to it from something you did observe. ",
    "`insufficient` means you have not established it yet.\n\n",
    "Only a `deterministic` claim credits progress. An `inferential` or ",
    "`insufficient` claim is recorded, stays visible, and resets none of your ",
    "progress counters, so a run that keeps reporting unproven claims reads as ",
    "stalled however confidently it is written. Classify honestly: an ",
    "`insufficient` claim costs you nothing you had, and a `deterministic` one ",
    "with no reproducible proof reference is refused outright."
);

/// How well a claim is backed.
///
/// The values match the control plane's own column, and the mapping is the
/// point: this is the field run health reads, not a label for a person.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceClass {
    /// Backed by something a reader can re-run and get the same answer from.
    Deterministic,
    /// Reasoned to from something observed, without a proof that re-runs.
    Inferential,
    /// Not established yet, and reported as such.
    Insufficient,
}

impl EvidenceClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::Inferential => "inferential",
            Self::Insufficient => "insufficient",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "deterministic" => Some(Self::Deterministic),
            "inferential" => Some(Self::Inferential),
            "insufficient" => Some(Self::Insufficient),
            _ => None,
        }
    }

    /// Whether a claim of this class credits progress for the run it belongs
    /// to. The other two are recorded without crediting it.
    #[must_use]
    pub const fn credits_progress(self) -> bool {
        matches!(self, Self::Deterministic)
    }
}

/// Whether the run caused what the claim describes, or found it already there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CausalDisposition {
    /// The default: a worker reporting on its own work is reporting what its
    /// own work produced unless it says otherwise.
    #[default]
    CandidateCaused,
    PreExisting,
    Unknown,
}

impl CausalDisposition {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CandidateCaused => "candidate_caused",
            Self::PreExisting => "pre_existing",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "candidate_caused" => Some(Self::CandidateCaused),
            "pre_existing" => Some(Self::PreExisting),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// One thing the worker claims, with whatever backs it.
///
/// Built unchecked, the same way an [`crate::ask_user::AskUserQuestion`] is:
/// a claim's bounds are only judged against the whole checkpoint, and
/// [`Checkpoint::new`] is the only thing that produces a value anything
/// downstream will accept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceClaim {
    description: String,
    proof_refs: Vec<String>,
    evidence_class: EvidenceClass,
    disposition: CausalDisposition,
}

impl EvidenceClaim {
    pub fn new(
        description: impl Into<String>,
        proof_refs: Vec<String>,
        evidence_class: EvidenceClass,
        disposition: CausalDisposition,
    ) -> Self {
        Self {
            description: description.into(),
            proof_refs,
            evidence_class,
            disposition,
        }
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn proof_refs(&self) -> &[String] {
        &self.proof_refs
    }

    #[must_use]
    pub const fn evidence_class(&self) -> EvidenceClass {
        self.evidence_class
    }

    #[must_use]
    pub const fn disposition(&self) -> CausalDisposition {
        self.disposition
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointError {
    NoNextGoal,
    TooManyClaims,
    TooManyProofRefs,
    TooManyBlockers,
    TooManyTouchedPaths,
    /// A claim called deterministic with nothing to re-run behind it.
    DeterministicClaimWithoutProof,
    EmptyField(&'static str),
    FieldTooLong(&'static str),
    ControlCharacter(&'static str),
    NegativeEstimate,
    CheckpointTooLarge,
}

/// One milestone report: what was established since the last one, where the
/// work is going, and when the next report is due.
///
/// Every field is private and [`Checkpoint::new`] is the only constructor, so
/// a value of this type has already passed its bounds, its control-character
/// checks and the deterministic-claim proof rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    claims: Vec<EvidenceClaim>,
    hypothesis: Option<String>,
    next_goal: String,
    /// Seconds of work the worker now expects to still need. Revised at every
    /// checkpoint, which is the only reason it is worth recording: the first
    /// estimate is a guess, and the series is the signal.
    revised_estimate_seconds: Option<i64>,
    blockers: Vec<String>,
    /// Epoch seconds by which the worker promises its next checkpoint. The
    /// deadline is the worker's own, and the timer wheel holds it to it.
    next_checkpoint_at: Option<i64>,
    /// The paths this checkpoint's work touched, as the worker reports them.
    ///
    /// The genesis-path freeze reads the evidence ledger, never this list: a
    /// worker cannot be the source of the facts it is measured against. What
    /// this carries is the correlation — the first checkpoint whose work
    /// touched anything is the one that triggers the freeze, and these paths
    /// are what the ledger's own paths for the same attempt are checked
    /// against.
    touched_paths: Vec<String>,
}

impl Checkpoint {
    pub fn new(
        claims: Vec<EvidenceClaim>,
        hypothesis: Option<String>,
        next_goal: String,
        revised_estimate_seconds: Option<i64>,
        blockers: Vec<String>,
        next_checkpoint_at: Option<i64>,
        touched_paths: Vec<String>,
    ) -> Result<Self, CheckpointError> {
        if next_goal.is_empty() {
            return Err(CheckpointError::NoNextGoal);
        }
        check_field_bounds(&next_goal, "next_goal", MAX_CHECKPOINT_GOAL_CHARS)?;

        if let Some(hypothesis) = hypothesis.as_deref() {
            check_required_field(hypothesis, "hypothesis", MAX_CHECKPOINT_HYPOTHESIS_CHARS)?;
        }

        if claims.len() > MAX_CHECKPOINT_CLAIMS {
            return Err(CheckpointError::TooManyClaims);
        }
        for claim in &claims {
            validate_claim(claim)?;
        }

        if blockers.len() > MAX_CHECKPOINT_BLOCKERS {
            return Err(CheckpointError::TooManyBlockers);
        }
        for blocker in &blockers {
            check_required_field(blocker, "blocker", MAX_BLOCKER_CHARS)?;
        }

        if touched_paths.len() > MAX_CHECKPOINT_TOUCHED_PATHS {
            return Err(CheckpointError::TooManyTouchedPaths);
        }
        for path in &touched_paths {
            check_required_field(path, "touched_path", MAX_TOUCHED_PATH_CHARS)?;
        }

        if revised_estimate_seconds.is_some_and(|seconds| seconds < 0) {
            return Err(CheckpointError::NegativeEstimate);
        }

        let aggregate = next_goal.chars().count()
            + hypothesis.as_deref().map_or(0, |text| text.chars().count())
            + claims.iter().map(claim_char_count).sum::<usize>()
            + blockers
                .iter()
                .map(|blocker| blocker.chars().count())
                .sum::<usize>()
            + touched_paths
                .iter()
                .map(|path| path.chars().count())
                .sum::<usize>();

        if aggregate > MAX_CHECKPOINT_CHARS {
            return Err(CheckpointError::CheckpointTooLarge);
        }

        Ok(Self {
            claims,
            hypothesis,
            next_goal,
            revised_estimate_seconds,
            blockers,
            next_checkpoint_at,
            touched_paths,
        })
    }

    #[must_use]
    pub fn claims(&self) -> &[EvidenceClaim] {
        &self.claims
    }

    #[must_use]
    pub fn hypothesis(&self) -> Option<&str> {
        self.hypothesis.as_deref()
    }

    #[must_use]
    pub fn next_goal(&self) -> &str {
        &self.next_goal
    }

    #[must_use]
    pub const fn revised_estimate_seconds(&self) -> Option<i64> {
        self.revised_estimate_seconds
    }

    #[must_use]
    pub fn blockers(&self) -> &[String] {
        &self.blockers
    }

    #[must_use]
    pub const fn next_checkpoint_at(&self) -> Option<i64> {
        self.next_checkpoint_at
    }

    #[must_use]
    pub fn touched_paths(&self) -> &[String] {
        &self.touched_paths
    }

    /// Whether this checkpoint credits progress for its run.
    ///
    /// One deterministic claim is enough: a milestone that established
    /// anything re-runnable moved, whatever else the report also carries.
    #[must_use]
    pub fn credits_progress(&self) -> bool {
        self.claims
            .iter()
            .any(|claim| claim.evidence_class.credits_progress())
    }

    /// Whether this is a checkpoint with a diff behind it, which is what the
    /// genesis-path freeze waits for.
    #[must_use]
    pub fn carries_diff(&self) -> bool {
        !self.touched_paths.is_empty()
    }
}

/// One option a blocked decision offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskOption {
    id: String,
    label: String,
    consequence: Option<String>,
}

impl AskOption {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        consequence: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            consequence,
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    #[must_use]
    pub fn consequence(&self) -> Option<&str> {
        self.consequence.as_deref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskError {
    NoBlockedDecision,
    NoOptions,
    TooManyOptions,
    DuplicateOptionId,
    UnknownRecommendation,
    EmptyField(&'static str),
    FieldTooLong(&'static str),
    ControlCharacter(&'static str),
    AskTooLarge,
}

/// A decision the worker cannot make on its own, with the options it sees.
///
/// Options are required. A question with nothing to choose between is a
/// request for a person to do the analysis the worker was given the run to do,
/// and it is the shape that makes an inbox unanswerable at a glance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    blocked_decision: String,
    options: Vec<AskOption>,
    /// The option the worker would take, named by its id. Optional, because a
    /// worker with no preference should say so rather than invent one.
    recommendation: Option<String>,
}

impl Ask {
    pub fn new(
        blocked_decision: String,
        options: Vec<AskOption>,
        recommendation: Option<String>,
    ) -> Result<Self, AskError> {
        if blocked_decision.is_empty() {
            return Err(AskError::NoBlockedDecision);
        }
        check_ask_bounds(
            &blocked_decision,
            "blocked_decision",
            MAX_ASK_DECISION_CHARS,
        )?;

        if options.is_empty() {
            return Err(AskError::NoOptions);
        }
        if options.len() > MAX_ASK_OPTIONS {
            return Err(AskError::TooManyOptions);
        }

        let mut seen = BTreeSet::new();
        for option in &options {
            check_required_ask_field(&option.id, "option_id", MAX_ASK_OPTION_ID_CHARS)?;
            check_required_ask_field(&option.label, "option_label", MAX_ASK_OPTION_LABEL_CHARS)?;
            if let Some(consequence) = option.consequence.as_deref() {
                check_required_ask_field(
                    consequence,
                    "option_consequence",
                    MAX_ASK_OPTION_LABEL_CHARS,
                )?;
            }
            if !seen.insert(option.id.as_str()) {
                return Err(AskError::DuplicateOptionId);
            }
        }

        if let Some(recommendation) = recommendation.as_deref() {
            check_required_ask_field(
                recommendation,
                "recommendation",
                MAX_ASK_RECOMMENDATION_CHARS,
            )?;
            if !options.iter().any(|option| option.id == recommendation) {
                return Err(AskError::UnknownRecommendation);
            }
        }

        let aggregate = blocked_decision.chars().count()
            + recommendation
                .as_deref()
                .map_or(0, |text| text.chars().count())
            + options.iter().map(option_char_count).sum::<usize>();

        if aggregate > MAX_ASK_CHARS {
            return Err(AskError::AskTooLarge);
        }

        Ok(Self {
            blocked_decision,
            options,
            recommendation,
        })
    }

    #[must_use]
    pub fn blocked_decision(&self) -> &str {
        &self.blocked_decision
    }

    #[must_use]
    pub fn options(&self) -> &[AskOption] {
        &self.options
    }

    #[must_use]
    pub fn recommendation(&self) -> Option<&str> {
        self.recommendation.as_deref()
    }
}

/// What a recorded checkpoint became in the control plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointReceipt {
    /// The journal entry the checkpoint was written as. Findings point back at
    /// it; there is no separate checkpoint table.
    pub checkpoint_event_id: i64,
    /// One per claim, in the order the claims were given.
    pub finding_ids: Vec<i64>,
    /// Whether this checkpoint credited progress, as run health will read it.
    pub credited_progress: bool,
}

/// What an opened question became in the control plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskReceipt {
    pub question_id: i64,
    /// The run this parked. Echoed so the worker's tool result names the run
    /// it is now waiting on rather than assuming the reader knows.
    pub run_id: i64,
}

/// Why an introspection call could not be recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunIntrospectionError {
    /// The session is not executing a coordinator run, so there is nothing to
    /// checkpoint against and no run to park. Its own variant because it is
    /// not a failure: it is the tool being called outside team mode.
    NoRun,
    /// The control plane refused the write — a run that already moved, a
    /// transition its state has no path for.
    Refused(String),
    /// The control plane could not be reached at all.
    Unavailable,
}

impl std::fmt::Display for RunIntrospectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRun => formatter.write_str("this session is not executing a run"),
            Self::Refused(detail) => formatter.write_str(detail),
            Self::Unavailable => formatter.write_str("the control plane is unavailable"),
        }
    }
}

impl std::error::Error for RunIntrospectionError {}

/// Where a worker's checkpoints and questions are recorded.
///
/// The port exists for the same reason [`crate::ask_user::AskUserPort`] does:
/// the tool that collects the payload lives in the tools crate and the thing
/// that writes control-plane rows lives in the daemon, and neither may depend
/// on the other.
pub trait RunIntrospectionPort: Send {
    fn checkpoint(
        &mut self,
        checkpoint: &Checkpoint,
    ) -> Result<CheckpointReceipt, RunIntrospectionError>;

    /// Records the question and parks the run on it.
    ///
    /// It does not wait for the answer. The session is suspended once the turn
    /// ends, and the answer reaches the resumed session through the safe-point
    /// queue, so a call that blocked here would hold a provider handle for what
    /// is a human-scale wait.
    fn ask(&mut self, ask: &Ask) -> Result<AskReceipt, RunIntrospectionError>;
}

/// The port a session that is not executing a run holds.
///
/// Registered rather than omitted wherever a runtime has no control plane
/// behind it, so "this session is not a run" is an answer the worker reads
/// instead of a tool that silently does not exist.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableRunIntrospectionPort;

impl RunIntrospectionPort for UnavailableRunIntrospectionPort {
    fn checkpoint(
        &mut self,
        _checkpoint: &Checkpoint,
    ) -> Result<CheckpointReceipt, RunIntrospectionError> {
        Err(RunIntrospectionError::NoRun)
    }

    fn ask(&mut self, _ask: &Ask) -> Result<AskReceipt, RunIntrospectionError> {
        Err(RunIntrospectionError::NoRun)
    }
}

fn validate_claim(claim: &EvidenceClaim) -> Result<(), CheckpointError> {
    check_required_field(
        &claim.description,
        "claim_description",
        MAX_CLAIM_DESCRIPTION_CHARS,
    )?;

    if claim.proof_refs.len() > MAX_CLAIM_PROOF_REFS {
        return Err(CheckpointError::TooManyProofRefs);
    }

    for proof_ref in &claim.proof_refs {
        check_required_field(proof_ref, "proof_ref", MAX_PROOF_REF_CHARS)?;
    }

    if claim.evidence_class == EvidenceClass::Deterministic && claim.proof_refs.is_empty() {
        return Err(CheckpointError::DeterministicClaimWithoutProof);
    }

    Ok(())
}

fn claim_char_count(claim: &EvidenceClaim) -> usize {
    claim.description.chars().count()
        + claim
            .proof_refs
            .iter()
            .map(|proof_ref| proof_ref.chars().count())
            .sum::<usize>()
}

fn option_char_count(option: &AskOption) -> usize {
    option.id.chars().count()
        + option.label.chars().count()
        + option
            .consequence
            .as_deref()
            .map_or(0, |text| text.chars().count())
}

fn check_required_field(
    value: &str,
    field: &'static str,
    max_chars: usize,
) -> Result<(), CheckpointError> {
    if value.is_empty() {
        return Err(CheckpointError::EmptyField(field));
    }

    check_field_bounds(value, field, max_chars)
}

/// Newlines are allowed and every other control character is refused: these
/// payloads are rendered into an inbox and into a journal, and an escape
/// sequence reaching either is a terminal a worker gets to write to.
fn check_field_bounds(
    value: &str,
    field: &'static str,
    max_chars: usize,
) -> Result<(), CheckpointError> {
    if value.chars().count() > max_chars {
        return Err(CheckpointError::FieldTooLong(field));
    }

    if value
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return Err(CheckpointError::ControlCharacter(field));
    }

    Ok(())
}

fn check_required_ask_field(
    value: &str,
    field: &'static str,
    max_chars: usize,
) -> Result<(), AskError> {
    if value.is_empty() {
        return Err(AskError::EmptyField(field));
    }

    check_ask_bounds(value, field, max_chars)
}

fn check_ask_bounds(value: &str, field: &'static str, max_chars: usize) -> Result<(), AskError> {
    if value.chars().count() > max_chars {
        return Err(AskError::FieldTooLong(field));
    }

    if value
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return Err(AskError::ControlCharacter(field));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(class: EvidenceClass, proofs: &[&str]) -> EvidenceClaim {
        EvidenceClaim::new(
            "the parser rejects an empty header",
            proofs.iter().map(|proof| (*proof).to_owned()).collect(),
            class,
            CausalDisposition::CandidateCaused,
        )
    }

    fn checkpoint_with(claims: Vec<EvidenceClaim>) -> Result<Checkpoint, CheckpointError> {
        Checkpoint::new(
            claims,
            None,
            "wire the rejection into the caller".to_owned(),
            Some(900),
            Vec::new(),
            Some(1_700_000_600),
            Vec::new(),
        )
    }

    #[test]
    fn a_deterministic_claim_needs_a_proof_reference() {
        assert_eq!(
            checkpoint_with(vec![claim(EvidenceClass::Deterministic, &[])]),
            Err(CheckpointError::DeterministicClaimWithoutProof)
        );
    }

    #[test]
    fn the_unproven_classes_carry_no_proof_requirement() {
        for class in [EvidenceClass::Inferential, EvidenceClass::Insufficient] {
            assert!(
                checkpoint_with(vec![claim(class, &[])]).is_ok(),
                "{} should not require a proof reference",
                class.as_str()
            );
        }
    }

    #[test]
    fn only_a_deterministic_claim_credits_progress() {
        assert!(
            checkpoint_with(vec![claim(
                EvidenceClass::Deterministic,
                &["cargo test -p agens-core parser::rejects_empty_header => 0"]
            )])
            .expect("a proved claim is valid")
            .credits_progress()
        );

        assert!(
            !checkpoint_with(vec![
                claim(EvidenceClass::Inferential, &[]),
                claim(EvidenceClass::Insufficient, &[]),
            ])
            .expect("unproven claims are valid")
            .credits_progress()
        );
    }

    #[test]
    fn a_checkpoint_with_no_touched_paths_carries_no_diff() {
        let checkpoint = checkpoint_with(Vec::new()).expect("an empty claim set is valid");

        assert!(!checkpoint.carries_diff());
    }

    #[test]
    fn touched_paths_make_a_checkpoint_one_that_carries_a_diff() {
        let checkpoint = Checkpoint::new(
            Vec::new(),
            None,
            "keep going".to_owned(),
            None,
            Vec::new(),
            None,
            vec!["crates/agens-core/src/lib.rs".to_owned()],
        )
        .expect("a checkpoint naming a path is valid");

        assert!(checkpoint.carries_diff());
    }

    #[test]
    fn a_checkpoint_needs_a_next_goal() {
        assert_eq!(
            Checkpoint::new(
                Vec::new(),
                None,
                String::new(),
                None,
                Vec::new(),
                None,
                Vec::new(),
            ),
            Err(CheckpointError::NoNextGoal)
        );
    }

    #[test]
    fn a_control_character_is_refused_wherever_it_appears() {
        assert_eq!(
            Checkpoint::new(
                Vec::new(),
                None,
                "wipe the screen: \u{1b}[2J".to_owned(),
                None,
                Vec::new(),
                None,
                Vec::new(),
            ),
            Err(CheckpointError::ControlCharacter("next_goal"))
        );
    }

    #[test]
    fn a_negative_revised_estimate_is_refused() {
        assert_eq!(
            Checkpoint::new(
                Vec::new(),
                None,
                "keep going".to_owned(),
                Some(-1),
                Vec::new(),
                None,
                Vec::new(),
            ),
            Err(CheckpointError::NegativeEstimate)
        );
    }

    fn option(id: &str) -> AskOption {
        AskOption::new(id, "take the narrow path", None)
    }

    #[test]
    fn a_question_needs_options_to_choose_between() {
        assert_eq!(
            Ask::new("which schema wins".to_owned(), Vec::new(), None),
            Err(AskError::NoOptions)
        );
    }

    #[test]
    fn a_recommendation_has_to_name_one_of_the_options() {
        assert_eq!(
            Ask::new(
                "which schema wins".to_owned(),
                vec![option("keep")],
                Some("migrate".to_owned())
            ),
            Err(AskError::UnknownRecommendation)
        );

        assert!(
            Ask::new(
                "which schema wins".to_owned(),
                vec![option("keep")],
                Some("keep".to_owned())
            )
            .is_ok()
        );
    }

    #[test]
    fn option_ids_are_unique() {
        assert_eq!(
            Ask::new(
                "which schema wins".to_owned(),
                vec![option("keep"), option("keep")],
                None
            ),
            Err(AskError::DuplicateOptionId)
        );
    }

    #[test]
    fn an_unavailable_port_answers_that_there_is_no_run() {
        let mut port = UnavailableRunIntrospectionPort;

        assert_eq!(
            port.checkpoint(&checkpoint_with(Vec::new()).expect("valid")),
            Err(RunIntrospectionError::NoRun)
        );
        assert_eq!(
            port.ask(&Ask::new("which schema wins".to_owned(), vec![option("keep")], None).unwrap()),
            Err(RunIntrospectionError::NoRun)
        );
    }

    /// The prompt is a deliverable, not decoration: it is the only place a
    /// worker is told that an unproven claim resets none of its counters, and
    /// the classes it names have to be the ones the class enum actually has.
    #[test]
    fn the_worker_prompt_states_the_cost_of_an_unproven_claim() {
        for class in [
            EvidenceClass::Deterministic,
            EvidenceClass::Inferential,
            EvidenceClass::Insufficient,
        ] {
            assert!(
                WORKER_CHECKPOINT_PROMPT.contains(class.as_str()),
                "the worker prompt never names {}",
                class.as_str()
            );
        }

        assert!(WORKER_CHECKPOINT_PROMPT.contains("Only a `deterministic` claim credits progress"));
        assert!(WORKER_CHECKPOINT_PROMPT.contains("resets none of your"));
    }
}
