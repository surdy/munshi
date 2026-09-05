//! Resume restore: `munshi restore --resume` (issue #71).
//!
//! [`crate::restore`] brings back the durable *record* — the summary, the verbatim transcript, the
//! extracted outputs. This module does the second half of the
//! [khata-handoff restore flow](../../../docs/khata-handoff.md): it places a restored session back
//! into its **harness home**, so the harness can discover it and the conversation can continue on a
//! wiped or brand-new machine.
//!
//! # Claude Code only, and why the others are a refusal
//!
//! A Claude Code session's resumable state very nearly *is* its transcript: the harness keeps one
//! `<claude home>/projects/<cwd-slug>/<session-id>.jsonl` per session and reads it back on
//! `claude --resume`. Patwari holds that file verbatim, so restoring resumability means deriving the
//! slugged path and putting the archived bytes there.
//!
//! Because the store is keyed by that slug, `claude --resume <id>` is scoped to the projects
//! directory of the *current* working directory: the same command run elsewhere does not see the
//! session. So the transcript's location and the command's location are one fact, and this module
//! reports both — the guidance names the directory to run from, and the missing-directory warning
//! says to create it first. That scoping is version-pinned evidence of the same class as
//! [`claude_project_slug`]'s encoding: if a future harness resolves `--resume` across all projects,
//! naming the directory becomes redundant rather than wrong.
//!
//! Copilot and Codex get a typed refusal instead of a guess. Copilot's `session.db` /
//! `session-store.db` rows are deliberately unarchived (issue #23's allowlist) and may well be
//! load-bearing for resumption; whether a restored `session-state/<id>/events.jsonl` plus sidecars
//! is enough can only be settled by a manual spike against an installed harness, which no automated
//! test can perform. Codex has no adapter sidecars and was never probed. Claiming either resumable
//! would be exactly the failure mode the handoff doc warns about — *"Khata should never claim a
//! snapshot is restorable merely because upload completed"* — so both refuse with their reason and
//! [docs/harness-adapters.md](../../../docs/harness-adapters.md) records what the spike would have
//! to establish.
//!
//! # The compatibility check, and the honest size of it
//!
//! Restorability depends on a matching adapter *and* a matching harness version. The adapter half is
//! enforced: a snapshot whose `source_agent` is not `claude-code` never reaches a write. The version
//! half is *reported*, not enforced, and the report says so — Munshi does not record
//! `capture.source_agent_version` at upload today, so the archived version comes from the version
//! the writing harness stamped on the transcript's own records, and the installed version comes from
//! a best-effort `claude --version`. Either can be absent; absence is a stated warning, never a
//! crash and never a claim.
//!
//! # Never overwrite a live session
//!
//! The target path is somebody's live conversation. A file already there is compared byte for byte:
//! identical means "already present" (a no-op rerun), and *differing* is a refusal that `--force`
//! deliberately does not lift. `--force` exists so a record restore may replace a stale copy of its
//! own archive Markdown; a harness home is not Munshi's to overwrite, and a transcript that differs
//! from the archived one is a session the harness has continued past the snapshot — replacing it
//! would destroy conversation that was never archived.
//!
//! That refusal is also why there is no safety backup here, which the handoff flow's step 8 asked
//! for: nothing is ever replaced. The only two writes are "the file is absent" and "the file is
//! already byte-identical", and neither has anything to back up. If a later change ever admits a
//! replacing write, step 8 comes back with it.
//!
//! # Confirmation
//!
//! Writing into a harness home is a deliberate, single-session act, so it is gated the way
//! `register`'s disclosure is: the plan is printed (and, under `--json`, reported) and nothing is
//! written unless `--yes` was given. An unconfirmed run is a *finding*, not a success — the resume
//! did not happen, and a scripted caller must not read it as done.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::registration::StoredConfig;
use crate::render::{atomic_replace, restored_relative_directory};
use crate::source::{
    SourceKind, claude_transcript_recorded_agent_version, claude_transcript_recorded_origin,
};

/// The command an operator runs to continue a restored Claude Code session.
const CLAUDE_RESUME_COMMAND: &str = "claude --resume";
/// The executable consulted for the installed harness version. Resolved through `PATH` exactly as
/// [`crate::project`]'s `git` probe is, and just as optional.
const CLAUDE_EXECUTABLE: &str = "claude";

/// One `--resume` request.
#[derive(Debug, Clone)]
pub struct ResumeConfig {
    /// `--yes`: the operator accepted the planned harness-home write. Without it the plan is
    /// reported and nothing is written.
    pub confirmed: bool,
    /// Place into this Claude Code home instead of the registered one. Exists because the machine
    /// this command is for — freshly wiped, possibly not registered yet — may have no stored
    /// configuration to read a home from, and because guessing one from an ambient `$HOME` is the
    /// kind of guess [`crate::source::SourceHomes`] deliberately refuses to make.
    pub claude_home_override: Option<PathBuf>,
}

/// Why a resume did not place anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResumeRefusal {
    /// The archived session was produced by a harness whose resume path is not implemented.
    UnsupportedHarness,
    /// No Claude Code home is known: this machine has no usable registration and no
    /// `--claude-home` was given.
    HarnessHomeUnknown,
    /// The record restore did not put this session's summary or transcript on disk, so there is
    /// nothing verified to place.
    NothingToPlace,
    /// The archived transcript records no absolute `cwd`, so the projects-directory slug — which
    /// encodes exactly that — cannot be derived.
    OriginUnknown,
    /// A different transcript already sits at the target path. `--force` does not apply.
    TargetDiffers,
    /// The snapshot's own provenance disagrees with itself (manifest harness vs. summary source),
    /// so which adapter's rules apply is unknown.
    ProvenanceMismatch,
}

/// What became of the harness-home placement.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum ResumeStatus {
    /// The transcript was written into the harness home this run.
    Placed,
    /// A byte-identical transcript was already there; the rerun changed nothing.
    AlreadyPresent,
    /// `--yes` was not given: the plan is reported and nothing was written.
    Planned { message: String },
    Refused {
        reason: ResumeRefusal,
        message: String,
    },
    /// The write itself failed. The restored record is on disk regardless.
    Failed { message: String },
}

/// The resume half of one restore run.
#[derive(Debug, Clone, Serialize)]
pub struct ResumeReport {
    /// Whether `--yes` was given, so a report reads unambiguously on its own.
    pub confirmed: bool,
    /// The archived `source_agent` this resume was judged against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The harness session ID — the name the transcript file takes, and the argument
    /// `claude --resume` wants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The Claude Code version that wrote the archived transcript, when its records state one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_harness_version: Option<String>,
    /// The installed Claude Code version, when one could be detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_harness_version: Option<String>,
    /// The working directory the archived transcript records, which the projects-directory slug
    /// encodes — and, because of that, the directory [`Self::resume_command`] has to be run from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_directory: Option<String>,
    /// The `projects/` subdirectory name derived from it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_slug: Option<String>,
    /// The single planned (or performed) write into the harness home.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_path: Option<String>,
    /// The exact command that continues the session, reported only once a transcript is actually
    /// in place — never as a promise about a write that has not happened.
    ///
    /// **It must be run from [`Self::project_directory`].** `claude --resume <id>` looks the
    /// session up in the projects directory of the *current* working directory, so the same command
    /// run anywhere else finds nothing — the transcript this restore placed is filed under the
    /// session's own cwd slug, not globally. A consumer that shells out has to `cd` first, and if
    /// that directory does not exist on this machine (a reported warning) it has to be created or
    /// cloned before the command will work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
    /// Everything true about this resume that is not a refusal: a missing harness, an unknown
    /// version, a working directory that does not exist on this machine.
    pub warnings: Vec<String>,
    pub status: ResumeStatus,
}

impl ResumeReport {
    /// The exit code this resume deserves, ranked into [`crate::restore`]'s table: a failed write is
    /// a local failure (1), and every refusal — including a plan nobody confirmed and an
    /// unsupported harness for an explicitly named session — is a finding (4). Neither is success:
    /// the operator asked for a resumable session and did not get one.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self.status {
            ResumeStatus::Placed | ResumeStatus::AlreadyPresent => 0,
            ResumeStatus::Failed { .. } => 1,
            ResumeStatus::Planned { .. } | ResumeStatus::Refused { .. } => 4,
        }
    }

    /// The harness-home path a successful placement wrote to, which is the transcript location the
    /// session row should now point at.
    pub(crate) fn placed_transcript(&self) -> Option<PathBuf> {
        matches!(
            self.status,
            ResumeStatus::Placed | ResumeStatus::AlreadyPresent
        )
        .then(|| self.target_path.as_ref().map(PathBuf::from))
        .flatten()
    }

    /// How to continue a session that is now in place — the command *and* the directory to run it
    /// from, because one without the other does not work. Naming only the command would be advice
    /// that fails everywhere except by luck: the harness resolves `--resume` inside the projects
    /// directory of the current working directory, and this transcript is filed under the session's
    /// own cwd.
    fn continue_guidance(&self) -> String {
        let command = self
            .resume_command
            .as_deref()
            .unwrap_or(CLAUDE_RESUME_COMMAND);
        match &self.project_directory {
            Some(directory) => format!("continue it from {directory} with `{command}`"),
            None => format!("continue it with `{command}`, run from the session's own directory"),
        }
    }

    pub(crate) fn print_human(&self) {
        for warning in &self.warnings {
            println!("  resume warning: {warning}");
        }
        match &self.status {
            ResumeStatus::Placed => println!(
                "resume: placed {} — {}",
                self.target_path.as_deref().unwrap_or("the transcript"),
                self.continue_guidance(),
            ),
            ResumeStatus::AlreadyPresent => println!(
                "resume: already present at {} — {}",
                self.target_path.as_deref().unwrap_or("the harness home"),
                self.continue_guidance(),
            ),
            ResumeStatus::Planned { message } => {
                println!("resume: planned, nothing written — {message}");
                if let Some(target) = &self.target_path {
                    println!("  would write {target}");
                }
            }
            ResumeStatus::Refused { message, .. } => println!("resume: refused — {message}"),
            ResumeStatus::Failed { message } => println!("resume: failed — {message}"),
        }
    }
}

/// Everything a resume needs from the record restore that just ran.
pub(crate) struct ResumeInput<'a> {
    pub config: &'a ResumeConfig,
    /// The registration, when the machine has one: it is where the registered Claude Code home
    /// comes from.
    pub stored: Option<&'a StoredConfig>,
    /// The archive output directory the record was restored into.
    pub output_directory: &'a Path,
    /// The archived harness label from the snapshot manifest (`claude-code`, `copilot-cli`, …).
    pub source_agent: Option<&'a str>,
    /// `capture.source_agent_version` from the manifest, when the uploader recorded one.
    pub manifest_agent_version: Option<&'a str>,
    /// The restored session, when its record actually reached disk.
    pub restored: Option<RestoredTranscript<'a>>,
}

/// The verified transcript a completed record restore left on disk, which is the only thing this
/// module ever copies from. Reusing it is the point: `--resume` extends the record restore rather
/// than downloading anything a second time.
pub(crate) struct RestoredTranscript<'a> {
    pub source: SourceKind,
    pub session_id: &'a str,
    /// The archive Markdown path, relative to the output directory.
    pub markdown_relative: &'a Path,
    /// Whether the snapshot's `transcript.jsonl` is present locally and proved identical to the
    /// archived original by this run (written, replaced, or hash-matched).
    pub transcript_verified: bool,
}

/// Places one restored session back into its harness home, or explains why it did not.
///
/// Every exit is a report: a refusal, a plan, a failed write and a completed placement all read the
/// same way to `--json`, because "did the resume happen" is the question a caller asks and a bare
/// error would answer it only for the operator watching the terminal.
pub(crate) fn resume(input: &ResumeInput<'_>) -> ResumeReport {
    let mut report = ResumeReport {
        confirmed: input.config.confirmed,
        harness: input.source_agent.map(ToOwned::to_owned),
        session_id: input.restored.as_ref().map(|r| r.session_id.to_owned()),
        archived_harness_version: input.manifest_agent_version.map(ToOwned::to_owned),
        installed_harness_version: None,
        project_directory: None,
        project_slug: None,
        target_path: None,
        resume_command: None,
        warnings: Vec::new(),
        status: ResumeStatus::Placed,
    };

    // A snapshot whose manifest never arrived states no harness at all, which is a different thing
    // from stating one this build cannot place: the record restore's own failure says what went
    // wrong, and this half simply has nothing to judge.
    let Some(agent) = input.source_agent else {
        return refuse(
            report,
            ResumeRefusal::NothingToPlace,
            "the snapshot's manifest could not be read, so the harness that produced it is unknown; see the snapshot's own status".to_owned(),
        );
    };
    // The adapter check comes first and is the one hard gate: reserved logical paths and harness
    // layouts only mean what this module thinks they mean for the harness it was written against.
    if SourceKind::from_agent_label(agent) != Some(SourceKind::ClaudeCode) {
        return refuse(
            report,
            ResumeRefusal::UnsupportedHarness,
            format!(
                "resume restore is not supported for this harness yet ({agent}); only claude-code sessions can be placed back into a harness home. The record itself was restored — see docs/harness-adapters.md for what a {agent} resume would first have to establish"
            ),
        );
    }
    let Some(restored) = &input.restored else {
        return refuse(
            report,
            ResumeRefusal::NothingToPlace,
            "the record restore placed nothing for this session, so there is no verified transcript to resume from".to_owned(),
        );
    };
    // Two independent statements of the same fact — the manifest's `source_agent` and the restored
    // summary's own `source` frontmatter — and they are written by the same code path, so a
    // disagreement means the snapshot is not what one of them says it is.
    if restored.source != SourceKind::ClaudeCode {
        return refuse(
            report,
            ResumeRefusal::ProvenanceMismatch,
            format!(
                "the snapshot manifest says {agent} but the restored summary records source {}; refusing to guess which adapter's rules apply",
                restored.source.as_selector()
            ),
        );
    }
    if !restored.transcript_verified {
        return refuse(
            report,
            ResumeRefusal::NothingToPlace,
            "this run did not place a verified transcript.jsonl for the session (it was skipped, refused, or failed); rerun the restore until the transcript is present".to_owned(),
        );
    }

    let harness_home = match resolve_claude_home(input) {
        Ok(home) => home,
        Err(message) => return refuse(report, ResumeRefusal::HarnessHomeUnknown, message),
    };
    let staged = input
        .output_directory
        .join(restored_relative_directory(restored.markdown_relative))
        .join(crate::restore::RESTORED_TRANSCRIPT_FILE);

    // Versions are gathered before the plan so an unconfirmed run reports exactly the compatibility
    // evidence a confirmed one would act on.
    if report.archived_harness_version.is_none() {
        report.archived_harness_version = claude_transcript_recorded_agent_version(&staged);
    }
    report.installed_harness_version = installed_claude_version();
    note_version_evidence(&mut report);

    // The slug encodes the session's working directory, and the archived transcript is the only
    // thing that still knows it on a wiped machine — the harness home is empty and the local record
    // stores a project *component*, which is a different, lossy identity.
    let Some(origin) = claude_transcript_recorded_origin(&staged) else {
        return refuse(
            report,
            ResumeRefusal::OriginUnknown,
            format!(
                "the archived transcript at {} records no absolute cwd, so the projects/<slug> directory Claude Code would look in cannot be derived",
                staged.display()
            ),
        );
    };
    let cwd = origin.cwd.to_string_lossy().into_owned();
    let slug = claude_project_slug(&cwd);
    if !is_safe_slug(&slug) {
        return refuse(
            report,
            ResumeRefusal::OriginUnknown,
            format!(
                "the archived transcript records a cwd ({cwd}) that yields no usable projects directory name"
            ),
        );
    }
    // A working directory that no longer exists is expressly not a refusal: the transcript is
    // placed either way, and the operator may well be restoring onto a machine where the repository
    // has not been cloned yet. It does gate the *resume command*, though — which is run from that
    // directory — so the warning says what to do about it rather than merely noting it.
    if !origin.cwd.is_dir() {
        report.warnings.push(format!(
            "the session's working directory {cwd} does not exist on this machine; the transcript is placed regardless, but the resume command is run from that directory, so create or clone it before resuming — and tools that expect its contents will not find them until you do"
        ));
    }
    report.project_directory = Some(cwd);
    report.project_slug = Some(slug.clone());

    let target = harness_home
        .join("projects")
        .join(&slug)
        .join(format!("{}.jsonl", restored.session_id));
    report.target_path = Some(target.display().to_string());

    let bytes = match std::fs::read(&staged) {
        Ok(bytes) => bytes,
        Err(error) => {
            return fail(
                report,
                format!(
                    "could not read the restored transcript {}: {error}",
                    staged.display()
                ),
            );
        }
    };
    match std::fs::read(&target) {
        Ok(existing) if existing == bytes => {
            // Idempotent rerun: the harness home already holds exactly the archived bytes.
            report.resume_command =
                Some(format!("{CLAUDE_RESUME_COMMAND} {}", restored.session_id));
            report.status = ResumeStatus::AlreadyPresent;
            return report;
        }
        Ok(_) => {
            return refuse(
                report,
                ResumeRefusal::TargetDiffers,
                format!(
                    "{} already holds a different transcript for this session; --force does not apply to harness-home writes, because a differing file is a live conversation the harness continued past this snapshot",
                    target.display()
                ),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return fail(
                report,
                format!("could not read {}: {error}", target.display()),
            );
        }
    }

    if !input.config.confirmed {
        report.status = ResumeStatus::Planned {
            message: "pass --yes to write the plan above into the harness home".to_owned(),
        };
        return report;
    }
    // Same temp-file-plus-rename discipline the archive output directory gets: a harness reading
    // the projects directory concurrently never sees a half-written transcript.
    if let Err(error) = atomic_replace(&target, &bytes) {
        return fail(
            report,
            format!("could not write {}: {error}", target.display()),
        );
    }
    // The handoff flow's step 10 — "verify that the source harness can discover or resume the
    // session". What can be verified without launching the harness is that the file the harness
    // looks for is where it looks for it; the report names the command and stops short of claiming
    // the harness accepted it, which is exactly the claim the flow warns against making.
    if !target.is_file() {
        return fail(
            report,
            format!(
                "{} is not readable after the write; the harness will not discover the session",
                target.display()
            ),
        );
    }
    report.resume_command = Some(format!("{CLAUDE_RESUME_COMMAND} {}", restored.session_id));
    report.status = ResumeStatus::Placed;
    report
}

fn refuse(mut report: ResumeReport, reason: ResumeRefusal, message: String) -> ResumeReport {
    report.status = ResumeStatus::Refused { reason, message };
    report
}

fn fail(mut report: ResumeReport, message: String) -> ResumeReport {
    report.status = ResumeStatus::Failed { message };
    report
}

/// The Claude Code home to place into: the explicit override, else the registered one.
///
/// There is deliberately no ambient `$HOME/.claude` fallback. Every other harness-home read in
/// Munshi is confined to installations the operator registered ([`crate::source::SourceHomes`]), and
/// this is a *write*: inferring the target of a write into someone's live harness state from an
/// environment variable is precisely the guess this codebase refuses to make.
fn resolve_claude_home(input: &ResumeInput<'_>) -> Result<PathBuf, String> {
    if let Some(home) = &input.config.claude_home_override {
        if !home.is_absolute() {
            return Err(format!(
                "--claude-home {} is not an absolute path",
                home.display()
            ));
        }
        return Ok(home.clone());
    }
    input
        .stored
        .and_then(|stored| stored.harnesses.source_homes().claude_home)
        .ok_or_else(|| {
            "no Claude Code home is known: this machine has no registered claude-code harness; pass --claude-home, or register the harness first".to_owned()
        })
}

/// Records what the two version readings do and do not establish. Both are evidence for the
/// operator, and a missing one is a gap to state — never a reason to abort a restore, and never a
/// licence to call the snapshot compatible.
fn note_version_evidence(report: &mut ResumeReport) {
    match (
        report.archived_harness_version.as_deref(),
        report.installed_harness_version.as_deref(),
    ) {
        (None, _) => report.warnings.push(
            "the archive records no Claude Code version for this session, so compatibility with the installed harness could not be checked".to_owned(),
        ),
        (Some(_), None) => report.warnings.push(
            "no installed Claude Code could be detected on PATH, so the archived session's version could not be compared against one".to_owned(),
        ),
        (Some(archived), Some(installed)) if archived != installed => report.warnings.push(format!(
            "the session was written by Claude Code {archived} and this machine runs {installed}; the transcript format has been stable across such drift but this is not a compatibility guarantee"
        )),
        (Some(_), Some(_)) => {}
    }
}

/// Best-effort installed Claude Code version, `claude --version` (`2.1.227 (Claude Code)`).
///
/// Same discipline as [`crate::project`]'s `git` probe: resolved through `PATH`, output bounded,
/// and every failure — not installed, not on `PATH`, unexpected output — yields `None` rather than
/// an error. Nothing depends on the answer; it is reported so an operator can judge drift.
fn installed_claude_version() -> Option<String> {
    let output = Command::new(CLAUDE_EXECUTABLE)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > 4 * 1024 {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let version = text.split_whitespace().next()?.trim();
    (!version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')))
    .then(|| version.to_owned())
}

/// The `projects/` subdirectory name Claude Code gives a working directory.
///
/// **The rule:** every character of the absolute path that is not an ASCII letter or digit becomes
/// `-`, one `-` per UTF-16 code unit. So `/Users/alice/repos/munshi` becomes
/// `-Users-alice-repos-munshi`; a space, `.`, `_` and `+` each become one `-`; `ü` (one UTF-16 unit)
/// becomes one `-` and an emoji (a surrogate pair) becomes two. The code-unit detail is not
/// pedantry — it is what the harness's own JavaScript `String.prototype.replace` does, and getting
/// it wrong puts the transcript in a directory the harness never reads.
///
/// **How it was established.** Two checks against Claude Code 2.1.227 on macOS, because a slug this
/// module gets wrong fails silently:
///
/// 1. Every one of the 29 existing directories in a real `~/.claude/projects` was compared against
///    the `cwd` its own transcripts record. All matched this rule — including
///    `/Users/…/AI Coding for Real Engineers`, which rules out the narrower "replace `/`, `.` and
///    `_`" rule a test helper elsewhere in this repo happens to use.
/// 2. The harness itself was run in directories named to probe the edges — `slug_probe.d/ünï_x+y`
///    and `a🎉b`, against a throwaway `CLAUDE_CONFIG_DIR` — and the directories it created were
///    `…-slug-probe-d--n--x-y` and `…-slug-probe-d-a--b`, which is where the per-code-unit rule
///    comes from.
///
/// It remains version-pinned evidence about a private store, exactly like the rest of the Claude
/// Code adapter: if the harness changes its encoding, a resume restore places a transcript the
/// harness will not find, and the report's target path is what makes that visible.
fn claude_project_slug(cwd: &str) -> String {
    let mut slug = String::with_capacity(cwd.len());
    for character in cwd.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else {
            for _ in 0..character.len_utf16() {
                slug.push('-');
            }
        }
    }
    slug
}

/// Whether a derived slug is safe to use as a path component. It is by construction — the encoding
/// emits only `[A-Za-z0-9-]` — so this is the assertion that the construction held, standing between
/// an archive server's `cwd` string and a `join` into someone's harness home.
fn is_safe_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cases that came from real directories, plus the two probe directories the harness
    /// created for this issue. A regression here means transcripts land where nothing reads them.
    #[test]
    fn the_project_slug_matches_observed_claude_code_directories() {
        assert_eq!(
            claude_project_slug("/Users/alice/repos/munshi"),
            "-Users-alice-repos-munshi"
        );
        assert_eq!(claude_project_slug("/"), "-");
        assert_eq!(
            claude_project_slug("/Users/alice/repos/research/apy"),
            "-Users-alice-repos-research-apy"
        );
        // A space is encoded like any other non-alphanumeric, which the narrower
        // separators-only rule would have got wrong.
        assert_eq!(
            claude_project_slug("/Users/alice/repos/AI Coding for Real Engineers"),
            "-Users-alice-repos-AI-Coding-for-Real-Engineers"
        );
        // A path that already contains the encoding of another path doubles its leading dash,
        // exactly as the harness does for scratchpad directories.
        assert_eq!(
            claude_project_slug("/private/tmp/claude-503/-Users-alice-repos-munshi/x"),
            "-private-tmp-claude-503--Users-alice-repos-munshi-x"
        );
        // `.`, `_` and `+` are each one dash; `ü` is one UTF-16 code unit and so one dash.
        assert_eq!(claude_project_slug("/a.d/ünï_x+y"), "-a-d--n--x-y");
        // An emoji is a surrogate pair in UTF-16, and the harness emits one dash per unit.
        assert_eq!(claude_project_slug("/a🎉b"), "-a--b");
    }

    #[test]
    fn every_derived_slug_is_a_safe_path_component() {
        for cwd in [
            "/Users/alice/repos/munshi",
            "/../../etc",
            "/a/../b",
            "/tmp/x\\y",
            "/a🎉b",
        ] {
            let slug = claude_project_slug(cwd);
            assert!(is_safe_slug(&slug), "{cwd} yielded {slug}");
            assert!(
                !slug.contains('/') && !slug.contains('\\') && !slug.contains('.'),
                "{cwd} yielded {slug}"
            );
        }
        assert!(!is_safe_slug(""));
        assert!(!is_safe_slug("../escape"));
    }

    /// A resume that placed nothing must never hand the state importer a path to link to: the
    /// session row would then point at a harness-home file that does not exist.
    #[test]
    fn only_a_completed_placement_offers_a_transcript_path() {
        let base = ResumeReport {
            confirmed: true,
            harness: Some("claude-code".to_owned()),
            session_id: Some("sess".to_owned()),
            archived_harness_version: None,
            installed_harness_version: None,
            project_directory: None,
            project_slug: None,
            target_path: Some("/home/.claude/projects/-x/sess.jsonl".to_owned()),
            resume_command: None,
            warnings: Vec::new(),
            status: ResumeStatus::Placed,
        };
        assert!(base.placed_transcript().is_some());

        let present = ResumeReport {
            status: ResumeStatus::AlreadyPresent,
            ..base.clone()
        };
        assert!(present.placed_transcript().is_some());

        for status in [
            ResumeStatus::Planned {
                message: String::new(),
            },
            ResumeStatus::Refused {
                reason: ResumeRefusal::TargetDiffers,
                message: String::new(),
            },
            ResumeStatus::Failed {
                message: String::new(),
            },
        ] {
            let report = ResumeReport {
                status,
                ..base.clone()
            };
            assert!(report.placed_transcript().is_none());
            assert!(report.exit_code() != 0);
        }
        assert_eq!(base.exit_code(), 0);
        assert_eq!(present.exit_code(), 0);
    }

    /// The command alone is advice that fails everywhere but one directory, so the guidance never
    /// states it without saying where to run it.
    #[test]
    fn continue_guidance_never_states_the_command_without_its_directory() {
        let report = |project_directory: Option<&str>| ResumeReport {
            confirmed: true,
            harness: Some("claude-code".to_owned()),
            session_id: Some("sess".to_owned()),
            archived_harness_version: None,
            installed_harness_version: None,
            project_directory: project_directory.map(ToOwned::to_owned),
            project_slug: None,
            target_path: None,
            resume_command: Some("claude --resume sess".to_owned()),
            warnings: Vec::new(),
            status: ResumeStatus::Placed,
        };

        let named = report(Some("/machine-a/repos/thing")).continue_guidance();
        assert!(named.contains("/machine-a/repos/thing"), "{named}");
        assert!(named.contains("claude --resume sess"), "{named}");

        // A placement always knows its directory, but the fallback still has to carry the
        // requirement rather than silently dropping it.
        let unnamed = report(None).continue_guidance();
        assert!(unnamed.contains("claude --resume sess"), "{unnamed}");
        assert!(unnamed.contains("own directory"), "{unnamed}");
    }

    /// Every version reading that is not a clean match says something; silence would read as a
    /// compatibility claim.
    #[test]
    fn absent_or_drifting_versions_are_warnings_not_verdicts() {
        let report = |archived: Option<&str>, installed: Option<&str>| {
            let mut report = ResumeReport {
                confirmed: true,
                harness: None,
                session_id: None,
                archived_harness_version: archived.map(ToOwned::to_owned),
                installed_harness_version: installed.map(ToOwned::to_owned),
                project_directory: None,
                project_slug: None,
                target_path: None,
                resume_command: None,
                warnings: Vec::new(),
                status: ResumeStatus::Placed,
            };
            note_version_evidence(&mut report);
            report.warnings
        };
        assert!(report(None, Some("2.1.227"))[0].contains("no Claude Code version"));
        assert!(report(Some("2.1.205"), None)[0].contains("no installed Claude Code"));
        assert!(report(Some("2.1.205"), Some("2.1.227"))[0].contains("2.1.205"));
        assert!(report(Some("2.1.227"), Some("2.1.227")).is_empty());
    }
}
