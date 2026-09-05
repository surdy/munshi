#!/bin/sh
# A no-op reference summarizer: the smallest program that satisfies the contract in
# docs/summarizers.md. It calls no backend and reads nothing out of the request — it just
# drains stdin and prints one valid `StructuredSummary` — so it is a copyable skeleton for
# your own summarizer and a control when you are debugging a failing one: if Munshi rejects
# even this, the problem is the wiring, not your model's output.
#
#   cat fixtures/summarizer/sample-request.json | fixtures/summarizer/no-op.sh | python3 -m json.tool
#
# Replace the `printf` with a call to your backend, and keep the two rules that cause most
# rejections: exactly one JSON object on stdout and nothing else (logs go to stderr), and
# `work_completed` never empty.
set -eu
cat >/dev/null
printf '%s' '{"title":"No-op summary","goal":"Demonstrate the summarizer stdout contract without calling a backend.","work_completed":["none"],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["no-op"]}'
