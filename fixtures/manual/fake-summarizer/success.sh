#!/bin/sh
set -eu
input=$(cat)
case "$input" in
  *"unknown event content"*|*"malformed known event content"*|*"ZmFrZQ=="*) exit 10 ;;
esac
printf '%s' "$input" | grep -q '"required_schema"'
printf '%s' "$input" | grep -q '"kind":"user"'
printf '%s' "$input" | grep -q '"kind":"assistant"'
printf '%s' "$input" | grep -q '"kind":"tool"'
printf '%s' '{"title":"Implement manual archival","goal":"Archive one synthetic Copilot session safely.","work_completed":["Added defensive transcript normalization.","Rendered one deterministic Markdown record."],"decisions":["Use stable source identity instead of the title."],"files_changed":["crates/munshi/src/archive.rs"],"commands_and_validation":["cargo test --workspace"],"open_items":["Add resumed revisions in issue #3."],"tags":["rust","copilot-cli"]}'
