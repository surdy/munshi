# Khata planning handoff

Use this document to start a separate planning session for **Khata**, the remote history and backup
server paired with [Munshi](../README.md).

## Product relationship

- **Munshi** is the local client and session archivist.
- **Khata** is the central server that preserves session history.
- Munshi captures local coding-agent sessions, generates summaries, packages backup artifacts, and
  communicates with Khata.
- Khata accepts, indexes, stores, retrieves, and eventually helps restore those records.
- The two projects must remain independently deployable and versioned.

The name follows the same administrative/bookkeeping theme:

- *Munshi*: scribe, secretary, or record keeper.
- *Khata*: account, ledger, or account book.

A useful product phrase is: **Munshi writes the record; Khata keeps the ledger.**

## Current Munshi direction

The full Munshi plan is in [`README.md`](../README.md). Important decisions already made:

| Area | Decision |
| --- | --- |
| Munshi implementation | Rust |
| Initial session source | GitHub Copilot CLI |
| Later sources | Claude Code and OpenAI Codex CLI |
| Summary engine | Copilot CLI in noninteractive mode |
| Summary output | Markdown with YAML front matter |
| Session behavior | One report per logical session |
| Resumed sessions | Update the existing report |
| Initial platforms | macOS and Linux |
| Initial remote summary target | Notesmith-native sink |
| Full history backup | Deferred to a later phase |
| Full history server | Separate repository |

Munshi is intentionally local-first. A remote failure must never prevent local Markdown creation or
advance a backup cursor incorrectly.

## Khata's likely role

Khata should become the authoritative remote ledger for coding-agent session history. The initial
planning session should decide whether that includes both summaries and full backups from the
beginning or whether these are staged separately.

Potential responsibilities:

- Receive versioned session metadata.
- Receive Markdown summaries and summary revisions.
- Receive compressed raw session artifacts.
- Preserve immutable artifact versions and checksums.
- Index sessions by user, device, harness, repository, branch, project, and time.
- Support idempotent retries from intermittently connected clients.
- Return session manifests and artifacts to Munshi.
- Provide enough information for Munshi to stage and verify a restore.
- Enforce authentication, authorization, quotas, and retention policies.
- Expose machine-oriented APIs first; a web UI can come later.

Khata should not understand how to run Copilot CLI or generate summaries. That remains a Munshi
responsibility.

## Scope candidates

The planning session should evaluate these three possible first releases.

### Option A: Summary ledger

Khata initially stores only the rendered Markdown report and normalized metadata.

Advantages:

- Smallest server.
- Establishes authentication, identity, revisions, indexing, and sync.
- Gives Munshi a generic destination independent of Notesmith.
- Avoids handling sensitive raw transcripts initially.

Disadvantages:

- Overlaps with Notesmith.
- Does not yet satisfy full session preservation.
- May create an API that later needs substantial expansion for artifacts.

### Option B: Artifact backup only

Khata stores manifests and opaque compressed files while Notesmith continues to receive summaries.

Advantages:

- Clear separation: Notesmith is for knowledge, Khata is for backup.
- Khata does not need to interpret Markdown or summary structure.
- Focuses immediately on the unique requirement.

Disadvantages:

- Poor browsing and search experience without a metadata layer.
- Harder to validate the product incrementally.
- Session history becomes split across two servers.

### Option C: Unified session ledger

Khata stores normalized session metadata, Markdown summary revisions, and optional opaque backup
artifacts under one logical session record.

Advantages:

- One stable identity across summaries, transcripts, and restore artifacts.
- Cleanest long-term model.
- Supports search before full artifacts are uploaded.
- Notesmith can remain an optional downstream knowledge sink.

Disadvantages:

- Larger initial schema and API.
- Requires careful privacy and retention design earlier.

**Starting recommendation:** design the domain model for Option C, but implement the first vertical
slice as summary metadata plus one opaque artifact. This validates the complete identity and upload
model without building every feature.

## Core domain model

Khata should distinguish a logical session from its evolving summary and immutable backup
snapshots.

```text
Principal
  |
  +-- Device
        |
        +-- Session
              |
              +-- Summary revision 1
              +-- Summary revision 2
              |
              +-- Backup snapshot 1
                    |
                    +-- Artifact: events.jsonl.zst
                    +-- Artifact: session.db.zst
                    +-- Artifact: manifest.json
```

### Session

A logical coding-agent conversation that may be resumed over time.

Candidate identity:

```text
<principal>/<agent>/<source-session-id>
```

Do not use repository or timestamps as identity components. Repositories can move, branches can
change, and a resumed session can have several end timestamps.

Candidate fields:

```text
id
principal_id
source_agent
source_session_id
project
repository
branch
started_at
last_activity_at
created_at
updated_at
source_metadata
```

### Summary revision

A complete rendered report for a session at a particular source cursor.

Candidate fields:

```text
session_id
revision
source_cursor
source_hash
markdown
metadata
content_hash
created_at
```

Summary revisions should be append-only on Khata even if Munshi and Notesmith present only the
latest revision. This preserves history and simplifies conflict investigation.

### Backup snapshot

An immutable, internally consistent capture of the source agent's restorable session state.

Candidate fields:

```text
id
session_id
source_agent_version
client_version
captured_at
manifest_version
total_bytes
manifest_hash
encryption
compression
status
```

### Artifact

One file within a backup snapshot.

Candidate fields:

```text
snapshot_id
logical_path
media_type
size_bytes
stored_size_bytes
sha256
compression
encryption
storage_key
created_at
```

Artifacts should be immutable and content-addressable where practical.

## Proposed manifest

Munshi may eventually upload a manifest resembling:

```json
{
  "schema_version": 1,
  "session": {
    "agent": "copilot-cli",
    "source_session_id": "abc123",
    "repository": "surdy/munshi",
    "branch": "main"
  },
  "snapshot": {
    "captured_at": "2026-07-11T19:42:00Z",
    "agent_version": "1.0.70",
    "client_version": "0.1.0"
  },
  "artifacts": [
    {
      "logical_path": "events.jsonl.zst",
      "media_type": "application/x-ndjson",
      "size_bytes": 504321,
      "stored_size_bytes": 91234,
      "sha256": "..."
    },
    {
      "logical_path": "session.db.zst",
      "media_type": "application/vnd.sqlite3",
      "size_bytes": 1048576,
      "stored_size_bytes": 183210,
      "sha256": "..."
    }
  ]
}
```

The server must validate the manifest but should treat agent-specific artifacts as opaque. Restore
interpretation belongs in the matching Munshi source adapter.

## API properties

The API should be:

- Versioned from the first release.
- Idempotent for every client retry.
- Streaming for large artifacts.
- Able to resume interrupted uploads.
- Explicit about content hashes and expected sizes.
- Safe when a session is updated concurrently from more than one device.
- Independent of Notesmith's note API.
- Usable by a CLI without browser interaction.

Candidate resource layout:

```text
POST   /api/v1/sessions
GET    /api/v1/sessions
GET    /api/v1/sessions/{session_id}
POST   /api/v1/sessions/{session_id}/summaries
GET    /api/v1/sessions/{session_id}/summaries
GET    /api/v1/sessions/{session_id}/summaries/latest

POST   /api/v1/sessions/{session_id}/snapshots
GET    /api/v1/sessions/{session_id}/snapshots
GET    /api/v1/snapshots/{snapshot_id}
POST   /api/v1/snapshots/{snapshot_id}/artifacts
PUT    /api/v1/snapshots/{snapshot_id}/artifacts/{artifact_id}
POST   /api/v1/snapshots/{snapshot_id}/complete
GET    /api/v1/snapshots/{snapshot_id}/artifacts/{artifact_id}
```

The planning session should compare:

- Direct streaming upload through Khata.
- Pre-signed object-store uploads.
- TUS or another resumable-upload protocol.
- Multipart upload managed by Khata.

For a personal self-hosted deployment, direct streaming may be the simplest first implementation.
The storage abstraction should not prevent an S3-compatible backend later.

## Idempotency and conflict handling

Every mutating request should carry an idempotency key generated by Munshi.

Examples:

```text
session upsert:
sha256(principal + agent + source_session_id)

summary revision:
sha256(session_id + source_cursor + content_hash)

snapshot:
sha256(session_id + manifest_hash)

artifact:
sha256(snapshot_id + logical_path + sha256)
```

Recommended behavior:

- Repeating the same request returns the existing resource.
- Reusing a key with different content returns a conflict.
- Summary revisions are immutable after acceptance.
- Snapshots remain incomplete until all declared artifacts are uploaded and verified.
- Incomplete snapshots expire or are garbage-collected after a configurable period.
- Artifact completion checks size and checksum before making the snapshot restorable.

## Authentication and authorization

The first deployment is expected to be self-hosted, but authentication should not be postponed.

The planning session should decide between:

- Long-lived per-device bearer tokens.
- Personal access tokens with scopes.
- OAuth device flow.
- Mutual TLS for managed devices.

Suggested first release:

- One account or principal.
- Independently revocable per-device tokens.
- Tokens stored hashed on the server.
- Scopes such as `sessions:read`, `sessions:write`, `artifacts:read`, and `artifacts:write`.
- Optional token expiration.
- Audit records for upload, download, token creation, and token revocation.

Munshi must load credentials from an environment variable or operating-system credential store,
never from committed project configuration.

## Privacy, encryption, and trust model

Raw coding-agent history can contain:

- Source code.
- Shell output.
- File paths and usernames.
- Secrets accidentally printed by tools.
- Proprietary repository information.
- Personal conversations and prompts.

The planning session must choose and document the trust model:

### Server-readable storage

Khata terminates TLS and stores plaintext artifacts, potentially encrypted at rest by the host or
storage backend.

Benefits:

- Server-side search and inspection.
- Simpler restore and browser features.

Costs:

- Khata and its operators can read all history.
- A server compromise exposes content.

### Client-side encrypted artifacts

Munshi compresses and encrypts artifacts before upload. Khata stores ciphertext and metadata.

Benefits:

- Stronger privacy.
- Storage providers cannot inspect content.

Costs:

- Search is limited to unencrypted metadata.
- Key backup and recovery become critical.
- Browser-based inspection is harder.

Suggested direction:

- Keep normalized, explicitly selected index metadata server-readable.
- Support client-side encryption for raw artifacts.
- Decide separately whether Markdown summaries are encrypted.
- Never invent a custom encryption construction; use an established envelope format and audited
  libraries.

## Storage architecture

Separate metadata from blobs.

Candidate starting architecture:

```text
PostgreSQL or SQLite
    sessions
    summary revisions
    snapshot manifests
    artifact metadata
    devices and tokens
    audit events

Filesystem or object storage
    compressed/encrypted artifact blobs
```

Planning tradeoff:

- SQLite plus filesystem is ideal for a simple single-node personal deployment.
- PostgreSQL plus S3-compatible object storage is better for multi-user scale.

The first implementation can use SQLite and filesystem storage if storage interfaces and migration
paths are explicit. Avoid pretending local filesystem writes provide distributed object-store
semantics.

## Retention and garbage collection

Khata should define:

- Whether all summary revisions are retained forever.
- Whether duplicate artifacts are deduplicated.
- Snapshot retention by count, age, or pinned state.
- Cleanup of abandoned uploads.
- Account and device deletion semantics.
- Export-before-delete behavior.
- Whether deleting a session is soft, delayed, or immediate.
- How content-addressed blobs are reference-counted.

Destructive operations should require explicit authorization and produce audit records.

## Restore flow

Khata serves records and artifacts; Munshi performs the agent-specific restore.

Proposed flow:

1. Munshi queries sessions and snapshots.
2. The user selects a snapshot.
3. Munshi downloads the manifest and artifacts to a staging directory.
4. Munshi verifies sizes and checksums.
5. Munshi decrypts and decompresses locally.
6. The source adapter validates compatibility with the installed agent version.
7. Munshi shows the planned file/database changes.
8. Munshi creates a local safety backup.
9. Munshi restores atomically where possible.
10. Munshi verifies that the source harness can discover or resume the session.

Khata should never claim a snapshot is restorable merely because upload completed. Restorability
depends on a matching Munshi adapter and harness version.

## Relationship with Notesmith

Notesmith and Khata serve different primary purposes:

- **Notesmith** stores curated knowledge in human-owned Markdown vaults.
- **Khata** stores versioned coding-session records and backup artifacts.

Possible integration:

- Munshi sends the latest readable summary to Notesmith.
- Munshi sends every summary revision and backup snapshot to Khata.
- A Notesmith note may contain the Khata session ID or URL.
- Khata may expose the latest Markdown summary, but it should not adopt Notesmith's vault,
  routing, or note-editing semantics.

The planning session should decide whether Khata stores Markdown summaries in Phase 1 or begins with
artifact backup only. It should not make Notesmith a runtime dependency.

## Relationship with Madari

Madari may eventually browse Khata history through Munshi or directly through a read-only Khata
client.

Potential features:

- Show whether a local session has a remote backup.
- Show latest backup time and snapshot count.
- Trigger backup or retry.
- Browse remote-only sessions.
- Download and stage a restore.
- Open the latest Markdown summary.

The initial architecture should not require Madari. Prefer Munshi as the local orchestration layer
so authentication, encryption keys, and agent-specific restore behavior are not duplicated in the
GUI.

## Technology choice

The Khata planning session should evaluate the server stack rather than inherit Rust automatically.

Rust is a strong candidate because:

- Munshi and Madari are Rust.
- Shared protocol types can be generated or reused carefully.
- Streaming, checksums, compression, and bounded resource use are central.
- Axum, Tokio, SQLx, and object-store crates fit the workload.

Reasons to consider Go:

- Simpler service development and operations.
- Strong standard HTTP stack.
- Straightforward static deployment.
- Mature object-storage clients.

Regardless of language:

- Use an OpenAPI or JSON Schema contract as the cross-repository source of truth.
- Avoid sharing internal database models with Munshi.
- Generate clients or protocol types where useful.
- Keep server storage and authentication details out of the client domain model.

## Deployment assumptions to verify

Known preferences:

- Self-hosted deployment is likely.
- Privacy and local control matter.
- Notesmith already runs as a Podman Quadlet.
- macOS and Linux clients are in scope.

Questions for the planning session:

- Is the first Khata deployment single-user only?
- Should it run beside Notesmith on quadhost?
- Is local filesystem storage sufficient initially?
- Should artifact blobs live on local disk or NFS?
- Is off-site replication required?
- What reverse proxy and authentication boundary will be used?
- What is the expected number and size of sessions per day?
- What retention period is desired?
- Must remote browsing work before restore is implemented?
- Are raw artifacts client-side encrypted from the first release?

Do not assume Notesmith's current deployment volume choices are appropriate for Khata. Backup data
has different durability requirements from disposable caches.

## Planning deliverables

The next session should produce:

1. A concise product scope and non-goals for Khata v1.
2. A decision between summary ledger, artifact backup, or unified session ledger.
3. A threat model and encryption decision.
4. An initial domain schema.
5. A versioned HTTP API proposal.
6. An upload and resume protocol.
7. A storage-backend decision for the first deployment.
8. Authentication and device-token design.
9. Retention and deletion semantics.
10. A restore responsibility boundary between Khata and Munshi.
11. A phased implementation roadmap.
12. Testing, migration, backup, and disaster-recovery plans.
13. A recommendation for implementation language and framework.
14. A list of decisions that require user confirmation before implementation.

The planning session should inspect the current Munshi repository rather than relying only on this
handoff:

```text
https://github.com/surdy/munshi
```

The expected eventual repository is:

```text
surdy/khata
```

It should be private initially.

## Suggested prompt for the next session

```text
Plan Khata, the central session-history and backup server paired with Munshi.

Start by reading:
- /Users/surdy/repos/munshi/README.md
- /Users/surdy/repos/munshi/docs/khata-handoff.md

Khata should receive versioned coding-agent session metadata, Markdown summary revisions, and
eventually compressed full-session artifacts from Munshi. It must support idempotent and resumable
uploads, retrieval, and a safe future restore flow. It is a separate repository and must not depend
on Notesmith or Madari, though both may integrate with it.

Research comparable self-hosted backup, artifact, and event-history systems. Then propose the v1
scope, threat model, domain schema, API, authentication, storage architecture, retention behavior,
deployment model, implementation stack, and phased roadmap. Clearly distinguish decisions from
open questions. Ask me about choices that materially change the architecture.

Do not implement the server until the architecture questions are resolved.
```
