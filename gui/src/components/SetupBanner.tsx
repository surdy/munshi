import { useState } from "react";
import { installCli } from "../lib/api";
import type { CliInfo, StatusReport } from "../lib/contracts";
import { isRegistered } from "../lib/derive";

/**
 * The one place the app asks the user to do something before it is useful.
 *
 * Three distinct situations, deliberately not collapsed into one message, because the fix differs:
 * no CLI resolved at all, a CLI that is resolved but not installed on `PATH`, and an installed CLI
 * that has never been registered. Registration itself stays in the terminal — it carries a
 * disclosure the user must read and accept, and `register` has no `--json` contract to drive it
 * through (ADR 0007), so the app shows the exact command rather than pretending to own the step.
 */
export function SetupBanner({
  cli,
  status,
  onInstalled,
  onError,
}: {
  cli: CliInfo;
  status: StatusReport | null;
  onInstalled: (path: string) => void;
  onError: (message: string) => void;
}) {
  const [busy, setBusy] = useState(false);

  async function install() {
    setBusy(true);
    try {
      onInstalled(await installCli());
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(false);
    }
  }

  if (!cli.path) {
    return (
      <div className="banner">
        <div className="banner-row">
          <div>
            <strong>No munshi command found.</strong> This window reads everything from the{" "}
            <code>munshi</code> executable, and none was found on your <code>PATH</code>.
          </div>
          <div className="spacer" />
          {cli.bundled_path ? (
            <button type="button" className="primary" disabled={busy} onClick={install}>
              {busy ? "Installing…" : "Install the bundled command"}
            </button>
          ) : null}
        </div>
      </div>
    );
  }

  if (!cli.installed && cli.bundled_path) {
    return (
      <div className="banner setup">
        <div className="banner-row">
          <div>
            <strong>Install the munshi command.</strong> Copies the CLI shipped in this app to{" "}
            <code>{cli.install_target}</code>, where Munshi's harness hooks and the scheduled sweep
            can find it — and where it keeps working if this app is ever moved or deleted.
            {cli.install_dir_on_path ? null : (
              <>
                {" "}
                That directory is not on your <code>PATH</code>; add it to use <code>munshi</code>{" "}
                from a terminal too.
              </>
            )}
          </div>
          <div className="spacer" />
          <button type="button" className="primary" disabled={busy} onClick={install}>
            {busy ? "Installing…" : "Install"}
          </button>
        </div>
      </div>
    );
  }

  if (cli.update_available) {
    return (
      <div className="banner setup">
        <div className="banner-row">
          <div>
            <strong>The installed command is a different version.</strong> This app ships munshi{" "}
            <code>{cli.bundled_version}</code>; <code>{cli.path}</code> is{" "}
            <code>{cli.version}</code>. The installed copy is the one your hooks actually run.
          </div>
          <div className="spacer" />
          <button type="button" className="primary" disabled={busy} onClick={install}>
            {busy ? "Updating…" : "Update installed command"}
          </button>
        </div>
      </div>
    );
  }

  if (!isRegistered(status)) {
    return (
      <div className="banner">
        <strong>Munshi is not registered on this machine.</strong> Nothing is being captured yet.
        Registration installs the harness hooks and asks you to accept a disclosure about transcript
        processing, so it runs in a terminal:
        <ul>
          <li>
            <code>
              munshi register --accept-transcript-processing --output-dir ~/munshi-archives
              --summarizer /path/to/summarizer.sh
            </code>
          </li>
        </ul>
      </div>
    );
  }

  return null;
}
