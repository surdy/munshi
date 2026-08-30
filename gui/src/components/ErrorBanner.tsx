import type { CommandError } from "../lib/contracts";

/**
 * The degraded-sources banner.
 *
 * One failed contract degrades its own panel and lands here; it never blanks the window. Each
 * entry carries the argument vector so the failure can be reproduced in a terminal verbatim.
 */
export function ErrorBanner({
  errors,
  schemaWarnings,
}: {
  errors: CommandError[];
  schemaWarnings: number[];
}) {
  if (errors.length === 0 && schemaWarnings.length === 0) return null;
  return (
    <div className="banner">
      {schemaWarnings.length > 0 ? (
        <div>
          <strong>Unfamiliar contract version.</strong> This app was written against{" "}
          <code>schema_version: 1</code> but saw {schemaWarnings.join(", ")}. Some panels may be
          incomplete; the installed <code>munshi</code> is likely newer than this app.
        </div>
      ) : null}
      {errors.length > 0 ? (
        <>
          <strong>Degraded sources.</strong> These panels could not be filled:
          <ul>
            {errors.map((error) => (
              <li key={error.source}>
                <code>{error.command.join(" ")}</code> — {error.message}
              </li>
            ))}
          </ul>
        </>
      ) : null}
    </div>
  );
}
