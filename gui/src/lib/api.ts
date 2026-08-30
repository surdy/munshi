/**
 * The invoke boundary. Every call here lands on a `#[tauri::command]` in `src-tauri/src/lib.rs`,
 * which is in turn the only place that runs `munshi`.
 *
 * Nothing in this app talks to Munshi any other way: there is no `fetch`, no HTTP server, and no
 * shell plugin exposed to the webview.
 */

import { invoke } from "@tauri-apps/api/core";
import type { CliInfo, ShowReport, Snapshot } from "./contracts";

/** One collection round over the six read-only contracts. */
export function snapshot(): Promise<Snapshot> {
  return invoke<Snapshot>("snapshot");
}

/** `munshi show <id> --source <source> --json`. */
export function showSession(source: string, sessionId: string): Promise<ShowReport> {
  return invoke<ShowReport>("show_session", { source, sessionId });
}

/** `munshi retry <id> --json` — "summarize now" for a pending session, "retry" for a failed one. */
export function retrySession(source: string, sessionId: string, force = false): Promise<unknown> {
  return invoke("retry_session", { source, sessionId, force });
}

/** `munshi retry-all --json`. */
export function retryAll(force = false): Promise<unknown> {
  return invoke("retry_all", { force });
}

/** `munshi tick --json` — the same sweep the scheduler runs. */
export function runTick(): Promise<unknown> {
  return invoke("run_tick");
}

/** `munshi doctor --json`. */
export function doctor(): Promise<unknown> {
  return invoke("doctor");
}

/** Refresh just the CLI picture, without a full collection round. */
export function cliInfo(): Promise<CliInfo> {
  return invoke<CliInfo>("cli_info");
}

/** Copy the bundled CLI to `~/.local/bin/munshi`; resolves with the path written. */
export function installCli(): Promise<string> {
  return invoke<string>("install_cli");
}

/** Read one archive Markdown file, by the `archive_path` Munshi reported. */
export function readArchive(path: string): Promise<string> {
  return invoke<string>("read_archive", { path });
}

/** Open a local path or an allowed URL in the desktop's default handler. */
export function openTarget(target: string): Promise<void> {
  return invoke("open_target", { target });
}
