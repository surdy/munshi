---
schema_version: 1
id: "copilot:11111111-1111-4111-8111-111111111111"
agent: "copilot-cli"
session_id: "11111111-1111-4111-8111-111111111111"
project: "munshi"
project_identity: "github.com/surdy/munshi"
repository: "surdy/munshi"
branch: "main"
started_at: "2026-07-12T00:00:00.000Z"
updated_at: "2026-07-12T00:00:05.000Z"
summary_revision: 1
source_cursor: 6
source_hash: "sha256:b1fa43b94746009f4be399b60dbb5c51d1117a8f46773cf8aa5e2d8644c0cae2"
tags:
  - "rust"
  - "copilot-cli"
---

# Implement manual archival

## Goal

Archive one synthetic Copilot session safely.

## Work completed

- Added defensive transcript normalization.
- Rendered one deterministic Markdown record.

## Decisions

- Use stable source identity instead of the title.

## Files changed

- crates/munshi/src/archive.rs

## Commands and validation

- cargo test --workspace

## Open items

- Add resumed revisions in issue #3.
