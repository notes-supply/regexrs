#!/usr/bin/env bash
# Time regexrs revisions side by side with the same runner adapter, lock,
# features, fixtures and definitions.
#
#   revision.sh build SOURCE LABEL   build this checkout's runner against the
#                                    regexrs source tree SOURCE
#   revision.sh verify LABEL [FILTER]
#                                    verify LABEL's rows matching FILTER
#                                    (default ^planning/), then check their
#                                    counts
#
# SOURCE can come from `git archive REV | tar -x -C DIR`. A build leaves the
# runner binary and a Rebar suite directory for it in target/planning/LABEL,
# for `target/release/rebar measure -d target/planning/LABEL/suite`.
# Revision builds share target/planning/cargo, apart from the suite runner's.
set -euo pipefail
cd -- "$(dirname -- "$0")"
here=$(pwd)
rebar=target/release/rebar

case "${1:-}" in
build)
    source=$(realpath -- "$2")
    out=target/planning/$3
    rm -rf -- "$out"
    mkdir -p -- "$out/adapter" "$out/suite"
    cp runner/Cargo.toml runner/Cargo.lock "$out/adapter/"
    cp -r runner/src "$out/adapter/"
    sed -i -e "s#path = \"../..\"#path = \"$source\"#" \
        -e "s#path = \"../.rebar/#path = \"$here/.rebar/#" "$out/adapter/Cargo.toml"
    CARGO_TARGET_DIR=$here/target/planning/cargo cargo build --locked --release \
        --features native --manifest-path "$out/adapter/Cargo.toml"
    cp target/planning/cargo/release/regexrs-rebar-runner "$out/"
    sed -e "s#\./target/release/regexrs-rebar-runner#$here/$out/regexrs-rebar-runner#" \
        rebar/engines.toml > "$out/suite/engines.toml"
    for part in definitions haystacks regexes; do
        ln -s "$here/rebar/$part" "$out/suite/$part"
    done
    echo "$out"
    ;;
verify)
    suite=target/planning/$2/suite
    runner=target/planning/$2/regexrs-rebar-runner
    filter=${3:-^planning/}
    rows=0
    while IFS=, read -r name _model engine _version; do
        "$rebar" klv -d "$suite" "$name" | "$runner" "$engine" --verify
        rows=$((rows + 1))
    done < <("$rebar" measure -d "$suite" --list -f "$filter")
    printf 'Verified spans and captures for %s rows.\n' "$rows"
    "$rebar" measure -d "$suite" -f "$filter" -t
    ;;
*)
    echo "usage: revision.sh build SOURCE LABEL | revision.sh verify LABEL [FILTER]" >&2
    exit 2
    ;;
esac
