#!/usr/bin/env bash
# Same-job, CPU-pinned comparison of frozen sources using one adapter and suite.
set -euo pipefail
cd -- "$(dirname -- "$0")"

check_batch() {
    local listed=$1 measured=$2 engine=$3
    awk -F, -v engine="$engine" '$3 == engine { print $1 "," $2 "," $3 }' "$listed" \
        | LC_ALL=C sort > "$measured.expected"
    test -s "$measured.expected"
    awk -F, '
        NR == 1 {
            columns = NF
            if ($1 != "name" || $2 != "model" || $4 != "engine" || $6 != "err" || $8 != "iters") exit 1
            next
        }
        {
            sub(/\r$/, "")
            if (NF != columns || $6 != "" || $8 !~ /^[1-9][0-9]*$/) exit 1
            print $1 "," $2 "," $4
        }
    ' "$measured" | LC_ALL=C sort > "$measured.keys"
    cmp "$measured.expected" "$measured.keys"
}

if [[ ${1:-} == --check-batch ]]; then
    [[ $# == 4 ]]
    check_batch "$2" "$3" "$4"
    exit
fi

[[ $# -ge 2 ]] || { echo 'usage: compare.sh LABEL=COMMIT LABEL=COMMIT ...' >&2; exit 2; }
[[ -z ${RUSTFLAGS:-} ]] || { echo 'Unexpected RUSTFLAGS' >&2; exit 2; }
mode=${BENCH_MODE:-quick}
case "$mode" in
quick) filter="^($(paste -sd '|' quick-cases.txt))$" ;;
full) filter='.' ;;
*) echo 'BENCH_MODE must be quick or full' >&2; exit 2 ;;
esac
labels=()
commits=()
for input in "$@"; do
    [[ $input =~ ^([a-z][a-z0-9-]*)=([0-9a-f]{40})$ ]] || { echo 'Expected LABEL=full-commit' >&2; exit 2; }
    label=${BASH_REMATCH[1]}
    commit=${BASH_REMATCH[2]}
    for existing in "${labels[@]}"; do [[ $existing != "$label" ]]; done
    git cat-file -e "$commit^{commit}"
    labels+=("$label")
    commits+=("$commit")
done

here=$PWD
project=$(git rev-parse --show-toplevel)
out=$project/.benchmark-results/comparison
mkdir -p "$(dirname -- "$out")"
mkdir "$out"
mkdir "$out/sources" "$out/logs"
trap 'printf "%s\n" "$?" > "$out/exit-code.txt"' EXIT
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
cpu=$(awk '/Cpus_allowed_list/ { split($2, parts, /[-,]/); print parts[1] }' /proc/self/status)
[[ $cpu =~ ^[0-9]+$ ]]
{
    printf 'source=%s\nmode=%s\nfilter=%s\ncpu=%s\nrepetitions=5\nmax-time=100ms\nwarmup=50ms\n' "$(git rev-parse HEAD)" "$mode" "$filter" "$cpu"
    printf 'runner=%s\n' "${RUNNER_NAME:-local}"
    printf 'hosted-VM results are relative comparisons, not historical Aeolus timings\n'
    rustc -Vv
    uname -srm
    lscpu
    uptime
    cat /proc/self/status | awk '/Cpus_allowed_list/ { print }'
} > "$out/environment.txt"
sha256sum runner/src/main.rs runner/Cargo.lock rebar/definitions/*.toml \
    rebar/haystacks/synthetic/*.txt > "$out/inputs.sha256"
git -C .rebar rev-parse HEAD > "$out/rebar-commit.txt"
printf 'label,commit\n' > "$out/sources.csv"

gate() {
    local name=$1 code=0 started=$SECONDS
    shift
    timeout --foreground 15m "$@" > "$out/logs/$name.stdout" 2> "$out/logs/$name.stderr" || code=$?
    printf '%s\t%s\t%s\n' "$name" "$code" "$((SECONDS - started))" >> "$out/gates.tsv"
    [[ $code == 0 ]] || { cat "$out/logs/$name.stderr" >&2; return "$code"; }
    printf '%s: passed\n' "$name"
}

gate rebar-build env CARGO_TARGET_DIR="$here/target" \
    cargo build --locked --release --manifest-path .rebar/Cargo.toml
for index in "${!labels[@]}"; do
    label=${labels[index]}
    commit=${commits[index]}
    printf '%s,%s\n' "$label" "$commit" >> "$out/sources.csv"
    mkdir "$out/sources/$label"
    git -C "$project" archive "$commit" | tar -x -C "$out/sources/$label"
    gate "$label-build" bash revision.sh build "$out/sources/$label" "$label"
    gate "$label-runner-tests" env CARGO_TARGET_DIR="$here/target/planning/cargo" \
        cargo test --locked --release --features native --manifest-path "target/planning/$label/adapter/Cargo.toml"
    gate "$label-verify" bash revision.sh verify "$label" "$filter"
    target/release/rebar measure -d "target/planning/$label/suite" --list -f "$filter" > "$out/$label-rows.csv"
    if [[ $mode == quick ]]; then
        LC_ALL=C sort quick-cases.txt > "$out/expected-workloads.txt"
        cut -d, -f1 "$out/$label-rows.csv" | LC_ALL=C sort -u > "$out/$label-workloads.txt"
        cmp "$out/expected-workloads.txt" "$out/$label-workloads.txt"
    fi
    cmp "$out/${labels[0]}-rows.csv" "$out/$label-rows.csv"
    sha256sum "target/planning/$label/regexrs-rebar-runner" >> "$out/binaries.sha256"
done

# All compilation and exact-result checks finish before any measurement.
engines=(regexrs regexrs-regular regexrs-searcher regexrs-seek regexrs-native regex fancy fancy-seek resharp)
for ((repetition = 0; repetition < 5; repetition++)); do
    order=("${engines[@]:repetition}" "${engines[@]:0:repetition}")
    if ((repetition % 2)); then
        reversed=()
        for ((index = ${#order[@]} - 1; index >= 0; index--)); do reversed+=("${order[index]}"); done
        order=("${reversed[@]}")
    fi
    start=$((repetition % ${#labels[@]}))
    revisions=("${labels[@]:start}" "${labels[@]:0:start}")
    for engine in "${order[@]}"; do
        selected=("${labels[0]}")
        [[ $engine != regexrs* ]] || selected=("${revisions[@]}")
        for label in "${selected[@]}"; do
            if pgrep -x 'cargo|rustc|rustdoc|valgrind|qemu-system-x86|qemu-system-aar' > "$out/contention.txt"; then
                echo 'Build/profile/emulator contention; refusing timings' >&2
                exit 1
            fi
            name="$((repetition + 1))-$label-$engine"
            measured="$out/$name.csv"
            gate "$name" taskset -c "$cpu" target/release/rebar measure \
                -d "target/planning/$label/suite" -e "^$engine$" \
                -f "$filter" --max-time 100ms --max-warmup-time 50ms --max-iters 10000000
            cp "$out/logs/$name.stdout" "$measured"
            check_batch "$out/$label-rows.csv" "$measured" "$engine"
            awk -v label="$label" -v repetition="$((repetition + 1))" \
                -v header="$([[ -s $out/results.csv ]] && echo 0 || echo 1)" '
                { sub(/\r$/, "") }
                NR == 1 { if (header) print $0 ",revision,repetition"; next }
                { print $0 "," label "," repetition }
            ' "$measured" >> "$out/results.csv"
        done
    done
    printf 'REPETITION_%s_COMPLETE\n' "$((repetition + 1))"
done
printf 'All five rotated comparisons passed.\n' > "$out/complete.txt"
cat "$out/complete.txt"
