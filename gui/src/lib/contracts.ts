/**
 * TypeScript views of Munshi's versioned CLI/JSON contracts (ADR 0007).
 *
 * These mirror the `serde` structs in `crates/munshi/src/main.rs` and the sink modules. Two rules
 * follow from how those contracts evolve, and both are load-bearing here:
 *
 * 1. Fields are added additively at `schema_version: 1`, so this app must not assume it has seen
 *    every field, and must never fail a panel over one it does not recognise.
 * 2. A field this app *does* rely on may still be absent — on an older CLI, or on a row Munshi
 *    could not fill in. Every optional field below is therefore genuinely optional, and the
 *    readers in `parse.ts` default rather than throw.
 *
 * The compatibility key is `schema_version` plus the field shapes; consumers must not branch on
 * the `command` discriminator alone.
 */

/** The contract version this UI was written against. */
export const EXPECTED_SCHEMA_VERSION = 1;

/** Every contract carries these two. */
export interface ContractEnvelope {
  schema_version: number;
  command: string;
}

/** Munshi's kebab-case lifecycle states, as `sessions --state` accepts them. */
export type LifecycleState =
  | "observed"
  | "summary-pending"
  | "archived"
  | "revision-pending"
  | "interrupted"
  | "processing"
  | "failed"
  | "transcript-lost"
  | "not-archive-worthy"
  | "disabled-project"
  | "unknown";

/** The harnesses Munshi captures from. */
export type SourceKind = "copilot" | "claude-code" | "codex";

/** `sessions --json` → `items[]`. */
export interface SessionListItem {
  source: string;
  session_id: string;
  state: string;
  /** Preferred over `state`; older payloads only carried `state`. */
  lifecycle_state?: string;
  revision?: number;
  completion_reason?: string | null;
  summary_title?: string | null;
  archive_path?: string | null;
  /** The id `restore --session` wants. Additive (issue #76). */
  patwari_session_id?: string | null;
  last_error_code?: string | null;
  /** Additive (issue #56). */
  project?: string | null;
  created_at_ms?: number;
  updated_at_ms?: number;
}

export interface SessionsReport extends ContractEnvelope {
  filter: string | null;
  total: number;
  returned: number;
  items: SessionListItem[];
}

/** `attempts --json` → `items[]`. Outcomes are the `processing_attempts.outcome` CHECK values. */
export type AttemptOutcome = "processing" | "succeeded" | "failed" | "superseded" | "recovered";

export interface AttemptListItem {
  source?: string;
  session_id?: string;
  project?: string | null;
  outcome?: string;
  error_category?: string | null;
  started_at_ms?: number;
  finished_at_ms?: number | null;
}

export interface AttemptsReport extends ContractEnvelope {
  returned: number;
  items: AttemptListItem[];
}

/** `diagnostics --json` → `items[]`. */
export interface DiagnosticListItem {
  source?: string;
  session_id?: string;
  operation?: string;
  category?: string;
  cause_category?: string | null;
  recorded_at_ms?: number;
}

export interface DiagnosticsReport extends ContractEnvelope {
  returned: number;
  items: DiagnosticListItem[];
}

/** `status --json` → `sessions`: the per-state census. */
export interface SessionStateSummary {
  total?: number;
  archived?: number;
  summary_pending?: number;
  revision_pending?: number;
  interrupted?: number;
  processing?: number;
  observed?: number;
  failed?: number;
  [state: string]: number | undefined;
}

export type CheckStatus = "ok" | "warning" | "error" | "unknown";

/** One named readiness check from `status --json` → `configuration.checks[]`. */
export interface CheckResult {
  code?: string;
  status?: CheckStatus;
  message?: string | null;
}

/** Whether local archival is on. `unknown` is what an unregistered machine reports. */
export type CaptureState = "enabled" | "disabled" | "unknown";

export interface ConfigurationAssessment {
  /** The worst check status, rolled up. */
  status?: CheckStatus;
  /** False on an unregistered machine, and on a config the current workers cannot drive. */
  runtime_compatible?: boolean;
  capture_state?: CaptureState;
  delivery_state?: string;
  disabled_projects?: number;
  config_path?: string | null;
  hook_path?: string | null;
  claude_settings_path?: string | null;
  summarizer_executable?: string | null;
  output_directory?: string | null;
  summarizer_exhaust_home?: string | null;
  archive_git_history?: boolean | null;
  checks?: CheckResult[];
  [key: string]: unknown;
}

export interface StatusReport extends ContractEnvelope {
  state_directory: string;
  configuration: ConfigurationAssessment;
  sessions: SessionStateSummary;
  last_failure?: unknown;
}

/**
 * `archive-upload status --json` and `summary-delivery status --json`.
 *
 * Note that `enabled` lives under `settings`, while the counts are top level — both sinks are
 * opt-in and report `settings.enabled: false` rather than failing when they were never configured.
 * The per-session `items[]` array both commands also return is stripped before this reaches the
 * page: it is one row per session and would dwarf everything else the window carries.
 */
export interface SinkSettings {
  enabled?: boolean;
  /** False when the sink is enabled but its endpoint cannot be resolved. */
  addressable?: boolean;
  endpoint?: string | null;
  vault?: string | null;
  max_attempts?: number;
}

export interface SinkStatusReport extends ContractEnvelope {
  settings?: SinkSettings;
  total?: number;
  /** `archive-upload` only. */
  uploaded?: number;
  /** `summary-delivery` only. */
  delivered?: number;
  pending?: number;
  failed?: number;
  dead_letter?: number;
  blocked?: number;
  /** `archive-upload` only: bytes actually transferred, all attempts. */
  transfer_bytes_total?: number;
  [key: string]: unknown;
}

/** `show --json` → `session`. */
export interface ShowReport extends ContractEnvelope {
  found: boolean;
  session?: {
    source?: string;
    session_id?: string;
    lifecycle_state?: string;
    state?: string;
    revision?: number;
    summary_title?: string | null;
    archive_path?: string | null;
    completion_reason?: string | null;
    last_error_code?: string | null;
    project?: { identity?: string | null; origin_directory?: string | null } | null;
    delivery?: {
      state?: string;
      note_link?: string | null;
      endpoint?: string | null;
      attempts?: number;
      last_error?: string | null;
    } | null;
    [key: string]: unknown;
  } | null;
}

/** One invocation that failed, as `src-tauri/src/cli.rs` reports it. */
export interface CommandError {
  source: string;
  command: string[];
  message: string;
}

/** How the CLI backing this window was located. */
export type CliOrigin = "override" | "installed" | "path" | "bundled";

export interface CliInfo {
  path: string | null;
  origin: CliOrigin | null;
  version: string | null;
  bundled_path: string | null;
  bundled_version: string | null;
  install_target: string | null;
  installed: boolean;
  update_available: boolean;
  install_dir_on_path: boolean;
}

/** One collection round from the `snapshot` command. */
export interface Snapshot {
  generated_at_ms: number;
  cli: CliInfo;
  errors: CommandError[];
  status: StatusReport | null;
  sessions: SessionsReport | null;
  attempts: AttemptsReport | null;
  diagnostics: DiagnosticsReport | null;
  uploads: SinkStatusReport | null;
  deliveries: SinkStatusReport | null;
}
