//! One summary schema with three projections: compaction, run report, memory.
//!
//! The schema exists because the same eight sections were needed by three
//! consumers that would otherwise have written three formats and drifted. A
//! [`RunSummary`] always carries all eight, and each projection drops the ones
//! its consumer has no use for. An absent section and an empty one mean
//! different things, so nothing here can omit a section: the type holds every
//! field and every serializer emits every section it projects, empty or not.
//!
//! Most sections are assembled from rows that already exist rather than
//! written by a model. [`RunSummary::assemble`] takes plain input structs that
//! mirror the control-plane rows without depending on the store, so a report
//! can be produced with every provider capped.
//!
//! [`RunSummary::critical_context`] is the one field a model may fill. It is
//! also the only field with a setter, which is what keeps an optional
//! narrative pass from contradicting what was assembled: the narrator has no
//! way to reach the other seven sections.

pub mod render;

/// One of the eight sections every summary carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SummarySection {
    Goal,
    ConstraintsAndPreferences,
    Progress,
    KeyDecisions,
    Discoveries,
    NextSteps,
    CriticalContext,
    RelevantFiles,
}

impl SummarySection {
    /// Canonical order. Every serializer walks this list, so two projections
    /// never disagree about where a section sits.
    pub const ALL: [Self; 8] = [
        Self::Goal,
        Self::ConstraintsAndPreferences,
        Self::Progress,
        Self::KeyDecisions,
        Self::Discoveries,
        Self::NextSteps,
        Self::CriticalContext,
        Self::RelevantFiles,
    ];

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Goal => "Goal",
            Self::ConstraintsAndPreferences => "Constraints & Preferences",
            Self::Progress => "Progress",
            Self::KeyDecisions => "Key Decisions",
            Self::Discoveries => "Discoveries",
            Self::NextSteps => "Next Steps",
            Self::CriticalContext => "Critical Context",
            Self::RelevantFiles => "Relevant Files",
        }
    }
}

/// Which consumer a summary is being serialized for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SummaryProjection {
    /// Replaces the thread, so it carries everything.
    Compaction,
    /// Projects what is actionable. Constraints belong to the run's own spec
    /// and the narrative adds nothing a reader of the rows needs.
    RunReport,
    /// Memory keeps lessons. The paths of an ephemeral worktree are not one,
    /// which is why relevant files stop here.
    Engram,
}

impl SummaryProjection {
    #[must_use]
    pub const fn includes(self, section: SummarySection) -> bool {
        match self {
            Self::Compaction => true,
            Self::RunReport => !matches!(
                section,
                SummarySection::ConstraintsAndPreferences | SummarySection::CriticalContext
            ),
            Self::Engram => !matches!(
                section,
                SummarySection::CriticalContext | SummarySection::RelevantFiles
            ),
        }
    }

    /// The projected sections, in canonical order.
    pub fn sections(self) -> impl Iterator<Item = SummarySection> {
        SummarySection::ALL
            .into_iter()
            .filter(move |section| self.includes(*section))
    }
}

/// What the run was approved to do, frozen at approval.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Goal {
    pub scope: String,
    pub definition_of_done: String,
}

/// Where a constraint came from. Kept because a preference learned from the
/// person and a constraint written into the run's spec are not equally
/// negotiable, and a flat list would lose that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintSource {
    Spec,
    Preference,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constraint {
    pub source: ConstraintSource,
    pub text: String,
}

/// How well a finding's claim is backed. Mirrors the control-plane column of
/// the same name; the schema keeps its own copy so a summary can be assembled
/// without a dependency on the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceClass {
    Deterministic,
    Inferential,
    Insufficient,
}

impl EvidenceClass {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::Inferential => "inferential",
            Self::Insufficient => "insufficient",
        }
    }
}

/// Whether the run caused the finding or found it already there. Mirrors the
/// control-plane column of the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CausalDisposition {
    CandidateCaused,
    PreExisting,
    Unknown,
}

impl CausalDisposition {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CandidateCaused => "candidate-caused",
            Self::PreExisting => "pre-existing",
            Self::Unknown => "unknown",
        }
    }
}

/// A claim about the work with the evidence behind it. Key decisions and
/// discoveries are both this: assembled from the same rows, never rewritten,
/// so a fact never exists twice in two wordings and its proof travels with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub description: String,
    pub evidence_class: EvidenceClass,
    pub proof_refs: Vec<String>,
    pub causal_disposition: CausalDisposition,
}

/// Which of the two finding-backed sections a row belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindingSection {
    KeyDecision,
    Discovery,
}

impl FindingSection {
    /// The default routing when a caller has nothing better: what the run
    /// caused is a decision it took, what was already there is something it
    /// found. A caller that knows more may override it per row.
    #[must_use]
    pub const fn derive(causal_disposition: CausalDisposition) -> Self {
        match causal_disposition {
            CausalDisposition::CandidateCaused => Self::KeyDecision,
            CausalDisposition::PreExisting | CausalDisposition::Unknown => Self::Discovery,
        }
    }
}

/// Declared goals against the evidence for them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: Vec<String>,
    pub in_progress: Vec<String>,
    pub blocked: Vec<String>,
}

impl Progress {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.done.is_empty() && self.in_progress.is_empty() && self.blocked.is_empty()
    }
}

/// The only field a model may write.
///
/// A summary always has one; it is empty until a narrative pass fills it.
/// Empty and absent stay distinguishable because the section is rendered
/// either way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CriticalContext(Option<String>);

impl CriticalContext {
    #[must_use]
    pub fn empty() -> Self {
        Self(None)
    }

    /// Narration from a model. Text that is blank once trimmed leaves the
    /// section empty rather than storing whitespace a reader cannot tell from
    /// content.
    #[must_use]
    pub fn narrated(text: impl Into<String>) -> Self {
        let text = text.into();
        let trimmed = text.trim();

        if trimmed.is_empty() {
            return Self(None);
        }

        Self(Some(trimmed.to_owned()))
    }

    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.0.as_deref()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }
}

/// How a path was touched. A path both read and written is reported as
/// modified: the stronger fact is the one a reader acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathAccess {
    Read,
    Modified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TouchedPath {
    pub path: String,
    pub access: PathAccess,
}

/// The cumulative file tracking of one run, split by how each path was
/// touched. Both lists are deduplicated and sorted, so two assemblies of the
/// same rows produce the same bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelevantFiles {
    pub read: Vec<String>,
    pub modified: Vec<String>,
}

impl RelevantFiles {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.modified.is_empty()
    }
}

/// One finding row with the section it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindingInput {
    pub finding: Finding,
    pub section: FindingSection,
}

/// One checkpoint's declared goal and whether evidence backs it.
///
/// `next_goal` is the checkpoint's typed next-goal field. Only the last
/// checkpoint's is read, because an older one has already been superseded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointInput {
    pub declared_goal: String,
    pub evidenced: bool,
    pub next_goal: Option<String>,
}

/// One open question, by the decision it is blocking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenQuestionInput {
    pub blocked_decision: String,
}

/// The passive health signals that make a run blocked without anyone saying
/// so.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunHealthInput {
    pub noop_turns: i64,
    pub failing_test_signature: Option<String>,
}

/// Everything [`RunSummary::assemble`] reads.
///
/// Plain owned data on purpose: the control plane, a test and a replay of the
/// journal all produce it, and none of them has to agree on a row type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunSummaryInputs {
    /// `runs.scope` and `runs.dod`, frozen at approval.
    pub goal: Goal,
    /// The run's spec plus the preferences memory carries.
    pub constraints: Vec<Constraint>,
    /// The findings rows of the run.
    pub findings: Vec<FindingInput>,
    /// Checkpoints in the order they were reached.
    pub checkpoints: Vec<CheckpointInput>,
    /// Questions still open.
    pub open_questions: Vec<OpenQuestionInput>,
    /// Health for this run, when it has a row.
    pub health: Option<RunHealthInput>,
    /// Paths the evidence ledger reports the run touched.
    pub touched_paths: Vec<TouchedPath>,
}

/// The eight sections, assembled.
///
/// The fields are private and only [`Self::set_critical_context`] mutates: an
/// optional narrative pass can add context but cannot rewrite an assembled
/// section into something the rows do not say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    goal: Goal,
    constraints: Vec<Constraint>,
    progress: Progress,
    key_decisions: Vec<Finding>,
    discoveries: Vec<Finding>,
    next_steps: Vec<String>,
    critical_context: CriticalContext,
    relevant_files: RelevantFiles,
}

impl RunSummary {
    /// Assembles every section but the narrative one from rows.
    ///
    /// Deterministic and model-free: the same inputs always produce the same
    /// summary, which is what lets a run report exist while every provider is
    /// capped.
    #[must_use]
    pub fn assemble(inputs: RunSummaryInputs) -> Self {
        let RunSummaryInputs {
            goal,
            constraints,
            findings,
            checkpoints,
            open_questions,
            health,
            touched_paths,
        } = inputs;

        let (key_decisions, discoveries) = split_findings(findings);
        let next_steps = next_steps(&checkpoints);
        let progress = assemble_progress(checkpoints, open_questions, health.as_ref());

        Self {
            goal,
            constraints,
            progress,
            key_decisions,
            discoveries,
            next_steps,
            critical_context: CriticalContext::empty(),
            relevant_files: assemble_relevant_files(touched_paths),
        }
    }

    /// Replaces the narrated section. The only mutation the schema offers.
    pub fn set_critical_context(&mut self, critical_context: CriticalContext) {
        self.critical_context = critical_context;
    }

    #[must_use]
    pub fn goal(&self) -> &Goal {
        &self.goal
    }

    #[must_use]
    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }

    #[must_use]
    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    #[must_use]
    pub fn key_decisions(&self) -> &[Finding] {
        &self.key_decisions
    }

    #[must_use]
    pub fn discoveries(&self) -> &[Finding] {
        &self.discoveries
    }

    #[must_use]
    pub fn next_steps(&self) -> &[String] {
        &self.next_steps
    }

    #[must_use]
    pub fn critical_context(&self) -> &CriticalContext {
        &self.critical_context
    }

    #[must_use]
    pub fn relevant_files(&self) -> &RelevantFiles {
        &self.relevant_files
    }

    /// Whether the named section carries anything. Empty is a fact about the
    /// run, never a reason to omit the section.
    #[must_use]
    pub fn section_is_empty(&self, section: SummarySection) -> bool {
        match section {
            SummarySection::Goal => {
                self.goal.scope.is_empty() && self.goal.definition_of_done.is_empty()
            }
            SummarySection::ConstraintsAndPreferences => self.constraints.is_empty(),
            SummarySection::Progress => self.progress.is_empty(),
            SummarySection::KeyDecisions => self.key_decisions.is_empty(),
            SummarySection::Discoveries => self.discoveries.is_empty(),
            SummarySection::NextSteps => self.next_steps.is_empty(),
            SummarySection::CriticalContext => self.critical_context.is_empty(),
            SummarySection::RelevantFiles => self.relevant_files.is_empty(),
        }
    }
}

fn split_findings(findings: Vec<FindingInput>) -> (Vec<Finding>, Vec<Finding>) {
    let mut key_decisions = Vec::new();
    let mut discoveries = Vec::new();

    for input in findings {
        match input.section {
            FindingSection::KeyDecision => key_decisions.push(input.finding),
            FindingSection::Discovery => discoveries.push(input.finding),
        }
    }

    (key_decisions, discoveries)
}

fn next_steps(checkpoints: &[CheckpointInput]) -> Vec<String> {
    checkpoints
        .last()
        .and_then(|checkpoint| checkpoint.next_goal.clone())
        .into_iter()
        .collect()
}

fn assemble_progress(
    checkpoints: Vec<CheckpointInput>,
    open_questions: Vec<OpenQuestionInput>,
    health: Option<&RunHealthInput>,
) -> Progress {
    let mut progress = Progress::default();

    for checkpoint in checkpoints {
        if checkpoint.evidenced {
            progress.done.push(checkpoint.declared_goal);
        } else {
            progress.in_progress.push(checkpoint.declared_goal);
        }
    }

    for question in open_questions {
        progress
            .blocked
            .push(format!("awaiting an answer: {}", question.blocked_decision));
    }

    if let Some(health) = health {
        if let Some(signature) = &health.failing_test_signature {
            progress.blocked.push(format!("failing test: {signature}"));
        }

        if health.noop_turns > 0 {
            progress.blocked.push(format!(
                "{} turns with no recorded progress",
                health.noop_turns
            ));
        }
    }

    progress
}

fn assemble_relevant_files(touched_paths: Vec<TouchedPath>) -> RelevantFiles {
    let mut strongest: Vec<(String, PathAccess)> = Vec::new();

    for touched in touched_paths {
        match strongest.iter_mut().find(|(path, _)| *path == touched.path) {
            Some((_, access)) => *access = (*access).max(touched.access),
            None => strongest.push((touched.path, touched.access)),
        }
    }

    let mut files = RelevantFiles::default();
    for (path, access) in strongest {
        match access {
            PathAccess::Read => files.read.push(path),
            PathAccess::Modified => files.modified.push(path),
        }
    }

    files.read.sort();
    files.modified.sort();
    files
}
