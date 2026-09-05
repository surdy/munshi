# Shipping sessions to Patwari

Munshi's local archive is complete on its own: one Markdown record per session, in a folder you
own. *Archive upload* is the opt-in step that also ships each session's **full snapshot** to a
[Patwari](https://github.com/surdy/patwari) archive server, so the verbatim transcripts survive a
disk wipe and a read-side tool like [Qanungo](https://github.com/surdy/qanungo) has something to
read.

This page is the whole capture side of that seam: what travels, what it costs you in trust, the
four commands, how uploads actually get drained, the bridge from a manually archived session into
upload state, how a machine names itself, and a checklist for standing up a second machine.

- Server side (running a Patwari, its API): the [Patwari repo](https://github.com/surdy/patwari).
- Day-to-day command reference: [`user-guide.md`](user-guide.md#archive-upload-patwari-in-brief).
- Every setting: [`configuration.md`](configuration.md).

## 1. What gets shipped

Each **successful summary revision** uploads one immutable snapshot. Snapshots are *full*, never
deltas, and self-contained by contract ([ADR 0009](adr/0009-archive-full-snapshots-to-patwari.md)):

| Logical path | What it is |
| --- | --- |
| `summary.md` | This revision's rendered summary — **required** |
| `transcript.jsonl` | The verbatim harness transcript — **required** |
| `outputs/<sha256>` | Every oversized tool output the summary elided as a claim ticket ([ADR 0010](adr/0010-elide-with-claim-tickets-retrieve-on-demand.md)) |
| `sidecar/<relative-path>` | Copilot only: an allowlisted set of small session-state files (workspace, plan, checkpoints). Claude Code and Codex contribute none |

Because both required artifacts must be present, a session whose transcript Munshi cannot read is
reported `skipped` rather than uploaded as a summary-only snapshot.

Artifacts are zstd-compressed (level 3), content-addressed by sha256 of the original bytes, and
transferred in resumable chunks; Patwari deduplicates blobs by content hash, so re-uploading an
unchanged artifact moves nothing on the wire. Nothing is encrypted — Patwari verifies original
content hashes, and encryption would blind that verification (ADR 0009).

`artifact_set_version` says which artifact kinds a snapshot may carry: `1` is
`summary.md` + `transcript.jsonl` + `outputs/`, and `2` (what new snapshots record) adds the
optional `sidecar/` prefix. Sets are additive and consumers must tolerate absent kinds.

Alongside the artifacts, the manifest's `capture` block carries `captured_at`, source cursors,
project/repository/branch, the harness version, the Munshi version, and an opaque
**`source_metadata`** map that Patwari stores and returns verbatim without interpreting:

| Key | Value |
| --- | --- |
| `utc_offset` | The capture machine's UTC offset at `captured_at`, RFC3339-spelled (`+05:30`, `-08:00`). A pure function of the timestamp, so it is stable across retries of one attempt |
| `hostname` | The capture machine's sanitized label — see [§6](#6-device-identity). Read fresh on every attempt, because it is ambient machine state |
| `claude_md` | sha256 of the session project root's `CLAUDE.md` at capture time, or the literal `absent` when the root was readable and the file provably was not there |
| `agents_md` | sha256 of the project root's `AGENTS.md`, same rules |
| `origin` | A marker present only when the session's project identity came from recorded evidence rather than a live directory |

Only the *digest* of an instruction file ever travels; the file's content is never uploaded. An
**omitted** key means Munshi did not look (a scheduler-descended upload takes no filesystem contact
with the origin directory, and files over the instruction-size cap are skipped) — deliberately
distinct from `absent`, which means it looked and found nothing. Qanungo's activity heatmap reads
`utc_offset`, its per-device scope reads `hostname`, and its instructions doctor anchors "an
instruction edit landed" on `claude_md`/`agents_md` changing between two captures.

These fields accrue value only from the moment they ship: a capture taken before they existed
carries no record of what the clock or the instructions said then, and nothing can reconstruct it.

## 2. Trust: the upload carries no credential

**The snapshot upload sends no credential of any kind** — no bearer token, no API key. Patwari has
no authentication; it trusts the network it sits on. So point
`munshi archive-upload configure --endpoint` only at a Patwari on a private network you control:
localhost, a LAN address, a tailnet name, or an `https://` name that only resolves inside it.
Never at anything reachable from the open internet; put your own authenticating proxy in front of
it if you need one. The full posture is in the README's
[Trust and authentication](../README.md#trust-and-authentication) section — read it before you
configure an endpoint.

Two related facts worth stating plainly:

- **The archive learns your machine's name.** `source_metadata.hostname` rides every capture, and
  client registration additionally sends the `HOSTNAME` environment variable when it is set. If
  the archive is shared, so is that label. [§6](#6-device-identity) is how you choose it.
- **Notesmith delivery is different.** `munshi summary-delivery configure` accepts a credential
  *source* (an environment variable or an OS keychain entry) and reads the token at delivery time.
  Archive upload has no such option, because there is nothing on the other end to authenticate to.

## 3. Configure and enable

Four commands, in this order:

```bash
munshi archive-upload configure --endpoint http://127.0.0.1:8080
munshi archive-upload enable
munshi archive-upload status
munshi archive-upload backfill        # only if sessions accumulated while upload was off
```

`configure` records the endpoint **without** turning upload on, and mints this machine's persistent
`client_id` the first time an upload needs one. `enable` requires a configured, addressable server.
`disable` stops future uploads while keeping upload history.

Endpoints are `http://` or `https://` with `host[:port]`; both are fully supported. `https://`
(ADR 0013) goes through rustls verifying against the **operating system's** trust store, and Munshi
deliberately grows no TLS policy surface — there is no `--insecure` escape hatch and no CA-bundle
flag, so a private CA must be installed in the system store like any other trusted root.

```bash
munshi archive-upload configure --endpoint http://127.0.0.1:8080       # local or LAN
munshi archive-upload configure --endpoint https://patwari.example.net  # published, private DNS
```

Every subcommand takes `--state-dir`; without it, Munshi resolves `$MUNSHI_HOME`, then `~/.munshi`.
Every read command takes `--json` for the machine contract.

Enabling does **not** retroactively upload. `backfill` is what picks up the archived sessions that
accumulated while upload was off.

## 4. How uploads actually happen

The normal path is automatic and needs none of the commands above after setup:

1. A session ends; the harness hook fires; Munshi summarizes and writes the Markdown.
2. The same pass creates an upload row and attempts the upload immediately.
3. A failure records a bounded retry with exponential backoff (`max_attempts`, default 5, then the
   row parks as `dead-letter`).
4. **`munshi tick`** — the sweep a launchd/systemd timer should run — drains what is left.

**`tick` is the drain.** Its recovery sweep retries every upload row in state `pending` *or*
`failed` whose backoff has elapsed, independent of any new revision, up to 32 rows a pass. That is
how a transient Patwari outage recovers without the session having to change. The same sweep runs
inside `munshi hook recover` and on ordinary hook events.

The operator commands, and what each honestly does:

| Command | Picks up | Does **not** |
| --- | --- | --- |
| `backfill` | Archived sessions with **no** upload row for the configured endpoint; `uploaded` rows whose snapshot is not proven to carry both required artifacts; `uploaded` rows whose Markdown hash has drifted; rows `reconcile --repair-missing` reset | Touch `pending`, `failed`, or `dead-letter` rows — those belong to the retry paths |
| `retry <session-id>` | That one session in any ordinary state, `pending` included | Resume a parked fingerprint-preserving rearchive |
| `retry --all` | Every `failed` row (plus `dead-letter` rows with `--force`) | Touch `pending` rows |
| `reconcile` | Fills `patwari_session_id` onto uploaded rows recorded before that id was stored, from one server listing. Idempotent, endpoint-scoped, never overwrites | Upload anything |
| `reconcile --repair-missing` | Additionally resets uploaded rows whose recorded snapshot no longer exists, so `backfill` re-uploads them with a fresh capture identity | Upload anything itself |
| `rearchive <id> --source <s> --snapshot-file <f>` | Re-archives one repaired row from the JSON `GET /api/v1/snapshots/{snapshot_id}` returned *before* the snapshot was tombstoned, preserving every fingerprint-bearing field and byte (`--abandon` gives that up and returns the row to ordinary uploads) | Apply to anything but a parked rearchive row |

Three sharp edges, found by running this end to end, each tracked in munshi issues:

- **`backfill` and `retry --all` ignore `pending` rows** — only the `tick`/`hook recover` sweep
  drains them, and `tick` deliberately prints nothing when it has quiet sections, so you cannot
  tell from its output that it just moved a hundred snapshots. Run `archive-upload status`
  afterwards. ([#87](https://github.com/surdy/munshi/issues/87))
- **`backfill` trusts local bookkeeping, not the server.** It compares against this machine's
  upload rows, so if the archive is wiped while the rows still say `uploaded`, `backfill` reports
  `candidates=0`. Use `reconcile --repair-missing` (which checks the server's listing and resets
  rows whose snapshot is gone) and then `backfill`. ([#89](https://github.com/surdy/munshi/issues/89))
- **A `skipped` line carries no reason in human output** — just `<session-id> -> skipped`. The
  reason *is* in the machine contract: run the same command with `--json` and read each item's
  `reason` (`missing-transcript.jsonl`, `no-artifacts`, `not-archived`, `dead-letter`,
  `retry-not-due`, `worker-busy`, …). ([#86](https://github.com/surdy/munshi/issues/86))

## 5. The manual-archive bridge

`munshi archive` writes a **standalone** record: it never uploads to Patwari and never opens the
registered hook database. Its `--state-dir` flag only supplies the extraction threshold from your
registration — it does not write upload state. So the obvious demo path ("archive some transcripts
by hand, then upload them") silently does nothing until you bridge the two.

This is the exact sequence that works. `munshi hook` is listed in `munshi --help` and
`munshi archive --help` points here, so the bridge is reachable without knowing it exists
(issue #85):

```bash
# 1. Archive each session standalone (writes Markdown, no upload state)
munshi archive <session-id> --source claude-code \
  --events /path/to/<session-id>.jsonl \
  --project-dir /path/to/project \
  --output-dir ~/munshi-archives \
  --summarizer /path/to/summarizer

# 2. Import those Markdown records into operational state
munshi hook recover --state-dir ~/.munshi --rebuild-state

# 3. Put the transcripts where the harness-home lookup expects them (see below)

# 4. Configure, enable, and scan for candidates
munshi archive-upload configure --endpoint http://127.0.0.1:8080
munshi archive-upload enable
munshi archive-upload backfill

# 5. Drain the pending rows — this is the step that actually uploads
munshi tick --state-dir ~/.munshi

# 6. Confirm
munshi archive-upload status
```

**Step 3 is the one that traps people.** A row rebuilt by `--rebuild-state` was reconstructed from
its Markdown alone, and Markdown records no transcript path. Before skipping such a session, Munshi
re-derives the path inside the **registered** harness home — and nowhere else:

- Claude Code: `<claude-home>/projects/*/<session-id>.jsonl` (symlinked project directories and
  transcripts are rejected)
- Copilot: `<copilot-home>/session-state/<session-id>/events.jsonl`
- Codex: no lookup at all — rollout files are not named after the session, so a Codex session
  stays `skipped`

The candidate must also match that harness's pinned event envelope, so an unrelated file merely
occupying the expected path is rejected rather than trusted. Once it matches, the path is recorded
on the session and the full snapshot uploads in the same run. If your transcripts live anywhere
else, copy them into that layout before step 4, or every session reports
`skipped: missing-transcript.jsonl`.

`hook recover --rebuild-state` also backs the existing `munshi.db` aside and recreates it from your
validated Munshi-owned Markdown. Existing Markdown is never deleted or invalidated. As a side
effect it resets upload rows, which is what makes it the recovery path when an archive was wiped.

## 6. Device identity

`source_metadata.hostname` is how the read side tells your machines apart, so two machines must
never resolve to the same label. The rule, in order:

1. `memory_sync.machine_label`, if it is set and non-empty.
2. Otherwise the OS hostname (`gethostname`), **sanitized**: trimmed, a trailing `.local` dropped,
   lowercased, and every character outside `[a-z0-9._-]` mapped to `-`. So
   `Alices-MacBook-Pro.local` becomes `alices-macbook-pro`.
3. If the OS declines to answer or the result sanitizes away to nothing, the key is **omitted**
   rather than sent empty.

Check what your machine will stamp before you rely on it — run `hostname` and apply the rule. If
that is already a clean, distinct slug, you are done. If it is generic, ugly, or could collide with
another machine, set an explicit label:

```bash
munshi memory-sync configure \
  --endpoint http://127.0.0.1:27183 \
  --vault some-vault \
  --machine studio          # the label capture will use
```

Two things are surprising here and worth knowing
([#90](https://github.com/surdy/munshi/issues/90) tracks both):

- **The label lives in the `memory-sync` section**, which is the Notesmith auto-memory mirror — an
  unrelated feature. `--endpoint` and `--vault` are required by that command even when the label is
  all you want.
- **`configure` records; it does not enable.** Capture reads `machine_label` regardless of whether
  memory-sync is enabled, so do **not** run `munshi memory-sync enable` unless you actually want
  memory mirroring.

The sanitizer and the hostname lookup are shared between capture provenance and memory-sync
deliberately: one physical machine must present one spelling of itself everywhere, or it appears in
the archive as two devices.

## 7. Verifying it landed

Start locally:

```bash
munshi archive-upload status
```

```text
archive uploads total=35 uploaded=35 pending=0 failed=0 dead-letter=0
archive transfer lifetime-bytes=… latest-snapshots-stored-bytes=…
<session-id>  uploaded  <snapshot-id> patwari=<patwari-session-id> rev=1
```

`--json` gives the same as a stable contract. `lifetime-bytes` is what actually moved on the wire
per Patwari's upload receipts (blob dedup makes it far smaller than the artifact sizes); rows
recorded before that accounting existed contribute 0, so both totals are floors.

Then prove the snapshot is really in the archive, with the capture provenance the read side needs:

```bash
curl -fsS http://127.0.0.1:8080/api/v1/snapshots/<snapshot-id> \
  | python3 -c "import json,sys; print(json.load(sys.stdin).get('source_metadata', {}))"
```

Pass criteria: `utc_offset` is present and looks like your zone, and `hostname` is present and is
the slug you expect — distinct from every other machine uploading to this archive. If both hold,
per-device scope on the read side has a real device.

## 8. Second-machine checklist

Standing up capture on another machine, generically:

1. **Preconditions.** The harness you want captured (Copilot CLI and/or Claude Code) is installed
   and working. Note the OS: macOS needs step 3b, Linux does not.
2. **Reach the archive.** `curl -fsS https://patwari.example.net/readyz` (or `/healthz`) from this
   machine before doing anything else. There is no credential to copy — if the machine can reach
   the archive, it can upload to it.
3. **Build and install munshi** somewhere stable, because registration records the binary's
   absolute path in your hook files.
   - **3a. Linux:** `cargo build --release`, then
     `install -m755 target/release/munshi ~/.local/bin/munshi`; make sure `~/.local/bin` is on
     `PATH`.
   - **3b. macOS:** use `./contrib/dev-deploy.sh`, which builds, **codesigns with a stable local
     identity**, and installs. This matters: background `munshi tick` runs where macOS TCC keys
     folder access to the binary's designated requirement. An ad-hoc signature has none, so TCC
     falls back to the cdhash, which changes on every rebuild — orphaning the grant and
     re-prompting. The one-time certificate recipe is in the script's header comment.
4. **Pick a summarizer.** `contrib/copilot-summarizer.sh` or `contrib/claude-summarizer.sh`; the
   choice is independent of which harness is captured. See [`summarizers.md`](summarizers.md).
5. **Register**, accepting the disclosure — read
   [getting started](getting-started.md) first:
   ```bash
   munshi register --accept-transcript-processing \
     --harness copilot \
     --output-dir ~/munshi-archives \
     --summarizer /path/to/munshi/contrib/copilot-summarizer.sh \
     --summarizer-exhaust-home ~/.copilot-summarizer \
     --summarizer-exhaust-retention-days 14
   ```
   The two `--summarizer-exhaust-*` flags let `tick` prune the isolated summarizer home. Leave them
   off and nothing is ever pruned — measured growth on a real machine was **about 5.6 GB in a
   month** (see [`user-guide.md`](user-guide.md#summarizer-exhaust-retention)). Both are
   registration-owned: re-registering without `--summarizer-exhaust-home` turns retention off
   again.
6. **Guarantee a distinct device label.** Apply [§6](#6-device-identity) — check `hostname`, and
   set `memory-sync configure --machine <label>` if it could collide. Do this *before* the first
   upload; it is what makes the machine distinguishable on the read side.
7. **Configure and enable upload:**
   ```bash
   munshi archive-upload configure --endpoint https://patwari.example.net
   munshi archive-upload enable
   munshi archive-upload status
   ```
8. **Schedule the drain.** macOS: edit the absolute paths in
   `contrib/launchd/com.munshi.tick.plist` (launchd expands no `~`), copy it to
   `~/Library/LaunchAgents/`, and `launchctl bootstrap gui/$UID …`. Linux: a systemd **user** timer
   running `munshi tick` every 15 minutes.
9. **Restart the harness and run one real session.** Hooks load at session start, so a session
   already open when you registered is not captured. Do a little real work in some project
   directory and exit cleanly.
10. **Accept.** `munshi status`, `munshi sessions`, then `munshi archive-upload status --json` to
    find the fresh `snapshot_id`, and the curl from [§7](#7-verifying-it-landed) to confirm
    `utc_offset` and a `hostname` distinct from every other machine.

Only new sessions accrue capture provenance. Transcripts already on disk from before the install
are not retroactively summarized or uploaded unless you archive them by hand and walk the bridge in
[§5](#5-the-manual-archive-bridge).

## 9. Troubleshooting pointers

- **"My session archived but never uploaded"** — the ordered checklist, the failure categories, and
  what a `transcript-changed` or `skipped` outcome means:
  [`troubleshooting.md`](troubleshooting.md#my-session-archived-but-never-uploaded-to-patwari).
- **`munshi retrieve` exit codes** (1–7, one per failure class): the table in
  [`troubleshooting.md`](troubleshooting.md). Before a snapshot uploads, or with the server
  unreachable, `munshi retrieve <sha256> --local --session <id>` redeems the same claim ticket
  straight from the on-disk transcript, no network involved.
- **A `pending` pile that never moves** — see [§4](#4-how-uploads-actually-happen): run
  `munshi tick`, then `archive-upload status`. `backfill` and `retry --all` will not help.
- **`archive-upload status` failing with "malformed or was not created by this version"** on a
  state directory reached through a **symlink**: use the real path. `munshi status` works either
  way, which makes this look like file corruption when it is path identity.
  ([#88](https://github.com/surdy/munshi/issues/88))
- **Restoring a machine from the archive**: [`user-guide.md`](user-guide.md) covers
  `munshi restore`. Note `restore --session` filters on the **Patwari** session id, not the harness
  one — read it as `patwari_session_id` from `munshi sessions --json` or as the `patwari=` field of
  `archive-upload status`, and run `archive-upload reconcile` first if older rows lack it.
- **Everything Munshi settings-wise**: [`configuration.md`](configuration.md#archive_upload--opt-in-patwari-upload-of-full-session-snapshots).
