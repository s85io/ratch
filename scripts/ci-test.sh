#!/bin/sh
# Run `cargo test` with the given arguments. When it fails, repeat the lines
# that say why as one GitHub annotation, so that the reason is visible on the
# run's summary page without opening the log.

log=$(mktemp)
trap 'rm -f "$log"' EXIT

# Run every test binary even after one fails: a run should say all it can.
cargo test --no-fail-fast "$@" >"$log" 2>&1
status=$?
cat "$log"

if [ "$status" -ne 0 ] && [ -n "$GITHUB_ACTIONS" ]; then
    why=$(grep -E -A3 'panicked at|^error|FAILED|test result: FAILED' "$log" |
        grep -v -E '^--$|RUST_BACKTRACE' | cut -c1-400 | head -60 |
        sed -e 's/%/%25/g' | awk '{ printf "%s%%0A", $0 }')
    echo "::error title=cargo test $* on $(uname -s)::$why"
fi
exit "$status"
