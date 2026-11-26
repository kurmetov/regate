#!/bin/sh
# The package is a skill and a guard hook over the regate binary: check both are well formed, and the
# binary answers if it is installed.
set -e
cd "$(dirname "$0")/.."
head -1 skills/regate/SKILL.md | grep -qx -- '---'
grep -q '^name: regate$' skills/regate/SKILL.md
grep -q '^description: ' skills/regate/SKILL.md
node -e 'JSON.parse(require("fs").readFileSync("hooks/hooks.json","utf8"))'
if command -v regate >/dev/null 2>&1; then regate --help | grep -q 'regate queue'; fi
echo ok
