#!/usr/bin/env bash
# Verify every engine row of the suite before timing: ordered match spans and
# every capture span against Fancy Regex, then Rebar's own count gate.
set -euo pipefail
cd -- "$(dirname -- "$0")"
rebar=target/release/rebar
runner=target/release/regexrs-rebar-runner
rows=0
while IFS=, read -r name model engine _version; do
    "$rebar" klv -d rebar "$name" | "$runner" "$engine" --verify
    rows=$((rows + 1))
done < <("$rebar" measure -d rebar --list)
printf 'Verified spans and captures for %s rows.\n' "$rows"
"$rebar" measure -d rebar -t
