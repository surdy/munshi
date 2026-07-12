#!/bin/sh
cat >/dev/null
printf '%s' 'synthetic private stderr must not be echoed' >&2
exit 7
