#!/bin/sh
set -eu
input=$(cat)
printf '%s' "$input" | grep -q '"kind":"user"'
printf '%s' "$input" | grep -q '"kind":"tool"'
printf '%s' "$input" | grep -q 'Contents-only validation passed.'
case "$input" in
  *"YmluYXJ5LW11c3Qtbm90LWxlYWs="*|*"private.invalid"*) exit 10 ;;
esac
printf '%s' '{"title":"Validate contents-only completion","goal":"Archive safe text from a contents-only tool result.","work_completed":["Validated contents-only tool output."],"decisions":[],"files_changed":[],"commands_and_validation":["Synthetic contents-only fixture passed."],"open_items":[],"tags":["rust"]}'
