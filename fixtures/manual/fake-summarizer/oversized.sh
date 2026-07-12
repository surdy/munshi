#!/bin/sh
cat >/dev/null
dd if=/dev/zero bs=4096 count=1 2>/dev/null | tr '\000' x
