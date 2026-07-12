#!/bin/sh
set -eu
input=$(cat)
case "$input" in
  *FAIL_REQUEST*)
    printf '%s' '{"title":'
    ;;
  *)
    printf '%s' '{"title":"Contract summary title","goal":"Capture stable operational contracts.","work_completed":["Produced a deterministic summary."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["contract"]}'
    ;;
esac
