import type { SinkStatusReport, StatusReport } from "../lib/contracts";
import { failingChecks } from "../lib/derive";
import { bytes } from "../lib/format";

/**
 * Where Munshi is writing, and anything about this machine's setup that is not `ok`.
 *
 * Read-only by design: the config file is Munshi's, changed through `munshi register` and the
 * per-sink `configure` commands, none of which have a `--json` contract to drive them through
 * (ADR 0007). Showing the paths and the failing checks is the useful half, and it needs no
 * writable surface.
 */
export function ConfigPanel({
  status,
  uploads,
  deliveries,
}: {
  status: StatusReport | null;
  uploads: SinkStatusReport | null;
  deliveries: SinkStatusReport | null;
}) {
  const configuration = status?.configuration;
  const problems = failingChecks(status);

  return (
    <div className="card">
      <h2>
        Configuration
        {problems.length > 0 ? <span className="count">{problems.length} to look at</span> : null}
      </h2>

      {configuration ? (
        <dl>
          <div className="field">
            <dt>Capture</dt>
            <dd>{configuration.capture_state ?? "unknown"}</dd>
          </div>
          {configuration.output_directory ? (
            <div className="field">
              <dt>Archive</dt>
              <dd className="mono">{configuration.output_directory}</dd>
            </div>
          ) : null}
          {configuration.summarizer_executable ? (
            <div className="field">
              <dt>Summarizer</dt>
              <dd className="mono">{configuration.summarizer_executable}</dd>
            </div>
          ) : null}
          <div className="field">
            <dt>Uploads</dt>
            <dd>
              {uploads?.settings?.enabled
                ? `${uploads.settings.endpoint ?? "enabled"} · ${bytes(uploads.transfer_bytes_total)} transferred`
                : "disabled"}
            </dd>
          </div>
          <div className="field">
            <dt>Deliveries</dt>
            <dd>
              {deliveries?.settings?.enabled
                ? `${deliveries.settings.endpoint ?? "enabled"}${
                    deliveries.settings.vault ? ` · ${deliveries.settings.vault}` : ""
                  }`
                : "disabled"}
            </dd>
          </div>
          {configuration.disabled_projects ? (
            <div className="field">
              <dt>Opted out</dt>
              <dd>{configuration.disabled_projects} project(s)</dd>
            </div>
          ) : null}
        </dl>
      ) : (
        <div className="empty">Configuration could not be read.</div>
      )}

      {problems.length > 0 ? (
        <div className="checks">
          {problems.map((check) => (
            <div className="check" key={check.code}>
              <i className={`dot ${check.status === "error" ? "critical" : "warning"}`} />
              <div>
                <span className="mono">{check.code}</span>
                {check.message ? <div className="check-msg">{check.message}</div> : null}
              </div>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  );
}
