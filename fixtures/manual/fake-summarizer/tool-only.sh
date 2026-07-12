#!/bin/sh
set -eu
input=$(cat)
printf '%s' "$input" | grep -q '"kind":"user"'
printf '%s' "$input" | grep -q '"kind":"tool"'
printf '%s' '{"title":"Validate tool-only activity","goal":"Archive a valid tool completion without a result.","work_completed":["Validated result-free tool activity."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["rust"]}'
