#!/bin/sh
# Black-box tests of the ratch command line.
# Usage: sh tests/test.sh [path-to-ratch-binary]
# Runs in a temporary HOME, so it never touches real sessions.

RATCH="${1:-./target/release/ratch}"

# ── test framework ──────────────────────────────────────────────────────────

PASS=0
FAIL=0
T=0

ok() {
    T=$((T + 1)); PASS=$((PASS + 1))
    printf "ok %d - %s\n" "$T" "$1"
}

fail() {
    T=$((T + 1)); FAIL=$((FAIL + 1))
    printf "not ok %d - %s\n" "$T" "$1"
    [ -n "$2" ] && printf "  # expected : %s\n  # got      : %s\n" "$2" "$3"
}

assert_eq() {
    if [ "$2" = "$3" ]; then ok "$1"; else fail "$1" "$2" "$3"; fi
}

assert_contains() {
    case "$3" in *"$2"*) ok "$1" ;; *) fail "$1" "(contains '$2')" "$3" ;; esac
}

assert_not_contains() {
    case "$3" in *"$2"*) fail "$1" "(not contain '$2')" "$3" ;; *) ok "$1" ;; esac
}

assert_exit() {
    if [ "$2" = "$3" ]; then ok "$1"; else fail "$1" "exit $2" "exit $3"; fi
}

run() { out=$("$@" 2>&1); rc=$?; }

# ── isolation ────────────────────────────────────────────────────────────────

TESTDIR=$(mktemp -d)
export HOME="$TESTDIR"
trap 'rm -rf "$TESTDIR"' EXIT

printf "TAP version 13\n"

# ── helpers ──────────────────────────────────────────────────────────────────

# Wait until the session socket appears (up to 1 s).
wait_socket() {
    local i=0
    while [ $i -lt 20 ]; do
        [ -S "$HOME/.cache/ratch/$1" ] && return 0
        sleep 0.05
        i=$((i + 1))
    done
    return 1
}

# Kill a named session and wait for its socket to vanish.
tidy() { "$RATCH" kill "$1" >/dev/null 2>&1; sleep 0.05; }

# ── 1. help / version ────────────────────────────────────────────────────────

run "$RATCH" --help
assert_exit   "help: --help exits 0"      0 "$rc"
assert_contains "help: --help shows usage"  "Usage:" "$out"

run "$RATCH" -h
assert_exit   "help: -h exits 0"          0 "$rc"
assert_contains "help: -h shows usage"    "Usage:" "$out"

run "$RATCH" help
assert_exit   "help: help exits 0"        0 "$rc"
assert_contains "help: help shows usage"  "Usage:" "$out"

run "$RATCH" --version
assert_exit     "version: exits 0"        0 "$rc"
assert_contains "version: shows version"  "ratch " "$out"

# ── 2. start command ─────────────────────────────────────────────────────────

run "$RATCH" start
assert_exit     "start: no session → exit 1"        1 "$rc"
assert_contains "start: no session → message"       "No session was specified" "$out"

run "$RATCH" start s-basic sleep 999
assert_exit     "start: exits 0"                    0 "$rc"
assert_contains "start: prints confirmation"        "session 's-basic' started" "$out"
tidy s-basic

# quiet flag after session name
run "$RATCH" start s-quiet -q sleep 999
assert_exit     "start -q: exits 0"                 0 "$rc"
assert_eq       "start -q: no output"               "" "$out"
tidy s-quiet

# quiet flag before session name (new-style option position)
run "$RATCH" start -q s-qpre sleep 999
assert_exit     "start -q (pre-session): exits 0"   0 "$rc"
assert_eq       "start -q (pre-session): no output" "" "$out"
tidy s-qpre

# already running
run "$RATCH" start s-dup sleep 999
run "$RATCH" start s-dup sleep 999
assert_exit     "start: already running → exit 1"   1 "$rc"
assert_contains "start: already running message"    "already running" "$out"
tidy s-dup

# bad command
run "$RATCH" start s-badcmd __atch_no_such_cmd__
assert_exit     "start: bad command → exit 1"       1 "$rc"
assert_contains "start: bad command message"        "could not execute" "$out"


# ── 3. list command ───────────────────────────────────────────────────────────

run "$RATCH" list
assert_exit  "list: empty → exits 0"                0 "$rc"
assert_eq    "list: empty → (no sessions)"          "(no sessions)" "$out"

# quiet suppresses the empty message
run "$RATCH" -q list
assert_exit  "list -q: empty → exits 0"             0 "$rc"
assert_eq    "list -q: empty → no output"           "" "$out"

# with a running session
"$RATCH" start s-list sleep 999
run "$RATCH" list
assert_exit     "list: shows running session → exits 0"    0 "$rc"
assert_contains "list: shows session name"                 "s-list" "$out"
assert_not_contains "list: no [stale] for live session"    "[stale]" "$out"

# quiet still shows session data (only suppresses meta-messages)
run "$RATCH" -q list
assert_contains "list -q: still shows session data"        "s-list" "$out"

# short alias
run "$RATCH" ls
assert_contains "list alias 'ls': works"  "s-list" "$out"


tidy s-list

run "$RATCH" list
assert_eq "list: back to empty after kill" "(no sessions)" "$out"

# list -a: exited sessions (log without socket) appear with [exited]
# Clear any logs left over from earlier tidy'd sessions so they don't pollute.
rm -f "$HOME/.cache/ratch"/*.log 2>/dev/null || true
# Simulate an exited session by planting an orphaned log file (no socket).
mkdir -p "$HOME/.cache/ratch"
printf "some output\n" > "$HOME/.cache/ratch/s-exited.log"
run "$RATCH" list
assert_not_contains "list: exited session absent without -a" "s-exited" "$out"
run "$RATCH" list -a
assert_exit     "list -a: exits 0"                    0 "$rc"
assert_contains "list -a: shows exited session"       "s-exited" "$out"
assert_contains "list -a: marks as [exited]"          "[exited]" "$out"

# clean up the exited log before the next checks so it doesn't pollute [exited]
rm -f "$HOME/.cache/ratch/s-exited.log"

# running session is NOT shown as [exited] under -a
"$RATCH" start s-listrun sleep 999
run "$RATCH" list -a
assert_contains     "list -a: running session still listed"  "s-listrun" "$out"
assert_not_contains "list -a: running session not [exited]"  "[exited]" "$out"
tidy s-listrun
rm -f "$HOME/.cache/ratch/s-listrun.log"

# list -a with no sessions and no logs → (no sessions)
run "$RATCH" list -a
assert_exit "list -a: empty → exits 0"           0 "$rc"
assert_eq   "list -a: empty → (no sessions)"     "(no sessions)" "$out"

# ── 4. stale session ─────────────────────────────────────────────────────────
# Use 'run' (dontfork master stays in foreground) so we get the master PID
# directly via $!.  kill -9 leaves the socket file on disk but removes the
# listener → ECONNREFUSED → list shows [stale].

"$RATCH" run s-stale sleep 99999 &
STALE_MASTER_PID=$!
wait_socket s-stale
kill -9 "$STALE_MASTER_PID" 2>/dev/null
sleep 0.1

run "$RATCH" list
assert_contains "list: stale session shows [stale]" "[stale]" "$out"

# clean up orphaned child and leftover socket
pkill -f "sleep 99999" 2>/dev/null
rm -f "$HOME/.cache/ratch/s-stale"

# ── 5. kill command ───────────────────────────────────────────────────────────

run "$RATCH" kill
assert_exit     "kill: no session → exit 1"          1 "$rc"
assert_contains "kill: no session → message"         "No session was specified" "$out"

run "$RATCH" kill s-noexist
assert_exit     "kill: nonexistent → exit 1"         1 "$rc"
assert_contains "kill: nonexistent → message"        "does not exist" "$out"

"$RATCH" start s-kill sleep 999
run "$RATCH" kill s-kill
assert_exit     "kill: running → exit 0"             0 "$rc"
assert_contains "kill: prints stopped"               "stopped" "$out"

run "$RATCH" kill s-kill
assert_exit     "kill: already gone → exit 1"        1 "$rc"
assert_contains "kill: already gone → message"       "does not exist" "$out"


# extra args rejected
"$RATCH" start s-killextra sleep 999
run "$RATCH" kill s-killextra extra
assert_exit     "kill: extra arg → exit 1"           1 "$rc"
assert_contains "kill: extra arg → message"          "Invalid number of arguments" "$out"
tidy s-killextra

# kill -f / --force (skip SIGTERM grace period, send SIGKILL directly)
"$RATCH" start s-killf sleep 999
run "$RATCH" kill -f s-killf
assert_exit     "kill -f: exits 0"                   0 "$rc"
assert_contains "kill -f: prints killed"             "killed" "$out"

"$RATCH" start s-killf2 sleep 999
run "$RATCH" kill --force s-killf2
assert_exit     "kill --force: exits 0"              0 "$rc"
assert_contains "kill --force: prints killed"        "killed" "$out"

# -f after session name
"$RATCH" start s-killf3 sleep 999
run "$RATCH" kill s-killf3 -f
assert_exit     "kill -f (after session): exits 0"   0 "$rc"
assert_contains "kill -f (after session): killed"    "killed" "$out"

# -f on nonexistent session still reports the error
run "$RATCH" kill -f s-noexist-force
assert_exit     "kill -f: nonexistent → exit 1"      1 "$rc"
assert_contains "kill -f: nonexistent → message"     "does not exist" "$out"

# ── 6. clear command ─────────────────────────────────────────────────────────

run "$RATCH" clear
assert_exit     "clear: no session, no env → exit 1"         1 "$rc"
assert_contains "clear: no session, no env → message"        "No session was specified" "$out"

# clear a nonexistent log is silent success
run "$RATCH" clear s-noexist-clear
assert_exit "clear: no log → exits 0"                0 "$rc"
assert_eq   "clear: no log → no output"              "" "$out"

# start a session (creates the log)
"$RATCH" start s-clear sleep 999

run "$RATCH" clear s-clear
assert_exit     "clear: exits 0"                     0 "$rc"
assert_contains "clear: confirmation message"        "log cleared" "$out"

# quiet suppresses confirmation
"$RATCH" start s-clearq sleep 999
run "$RATCH" -q clear s-clearq
assert_exit "clear -q: exits 0"                      0 "$rc"
assert_eq   "clear -q: no output"                    "" "$out"

tidy s-clear
tidy s-clearq

# extra args rejected
run "$RATCH" clear s-noexist-clear extra
assert_exit     "clear: extra arg → exit 1"          1 "$rc"
assert_contains "clear: extra arg → message"         "Invalid number of arguments" "$out"

# atch clear with RATCH_SESSION set uses current session
"$RATCH" start s-clear-cur sleep 999
wait_socket s-clear-cur
run env RATCH_SESSION="$HOME/.cache/ratch/s-clear-cur" "$RATCH" clear
assert_exit     "clear: current session → exit 0"    0 "$rc"
assert_contains "clear: current session → message"   "log cleared" "$out"

# clear with no arg uses innermost session from nested chain
"$RATCH" start s-clear-inner sleep 999
wait_socket s-clear-inner
run env RATCH_SESSION="$HOME/.cache/ratch/s-clear-cur:$HOME/.cache/ratch/s-clear-inner" \
        "$RATCH" clear
assert_exit     "clear: nested env no arg → exit 0"  0 "$rc"
assert_contains "clear: nested env no arg → message" "s-clear-inner" "$out"

tidy s-clear-cur
tidy s-clear-inner

# ── 7. current command ───────────────────────────────────────────────────────

run "$RATCH" current
assert_exit "current: outside session → exit 1"      1 "$rc"
assert_eq   "current: outside session → no output"   "" "$out"

# RATCH_SESSION holds the chain; a single session has no colon
run env RATCH_SESSION="$HOME/.cache/ratch/mywork" "$RATCH" current
assert_exit     "current: single session → exit 0"         0 "$rc"
assert_eq       "current: single session → basename"       "mywork" "$out"

# deep path
run env RATCH_SESSION="/tmp/deep/path/proj" "$RATCH" current
assert_exit     "current: deep path → exit 0"        0 "$rc"
assert_eq       "current: deep path basename"        "proj" "$out"

# two levels of nesting: RATCH_SESSION = outer:inner
run env RATCH_SESSION="$HOME/.cache/ratch/outer:$HOME/.cache/ratch/inner" \
        "$RATCH" current
assert_exit     "current: nested 2 levels → exit 0"  0 "$rc"
assert_eq       "current: nested 2 levels → chain"   "outer > inner" "$out"

# three levels of nesting
run env RATCH_SESSION="/s/a:/s/b:/s/c" "$RATCH" current
assert_exit     "current: nested 3 levels → exit 0"  0 "$rc"
assert_eq       "current: nested 3 levels → chain"   "a > b > c" "$out"


# verify that RATCH_SESSION is set in the child and contains the socket path
"$RATCH" start envname-test sh -c \
    "printf '%s\\n' \"\$RATCH_SESSION\" > $TESTDIR/envname.txt"
sleep 0.1
run grep -q "envname-test" $TESTDIR/envname.txt
assert_exit "current: RATCH_SESSION set in child and contains socket path" 0 "$rc"
rm -f $TESTDIR/envname.txt

# verify that dashes (and other non-alphanumeric chars) in the binary name
# are replaced with underscores in the env var name:
# binary 'my-ratch' → env var 'MY_RATCH_SESSION'
DASH_RATCH="$TESTDIR/bin/my-ratch"
mkdir -p "$TESTDIR/bin"
ln -s "$RATCH" "$DASH_RATCH" 2>/dev/null || cp "$RATCH" "$DASH_RATCH"
"$DASH_RATCH" start envdash-test sh -c \
    "printf '%s\\n' \"\$MY_RATCH_SESSION\" > $TESTDIR/envdash.txt"
sleep 0.1
run grep -q "envdash-test" $TESTDIR/envdash.txt
assert_exit "current: dash in binary name → underscore in env var name" 0 "$rc"
"$DASH_RATCH" kill envdash-test >/dev/null 2>&1
rm -f $TESTDIR/envdash.txt

# ── 8. push command ───────────────────────────────────────────────────────────

run "$RATCH" push
assert_exit     "push: no session → exit 1"          1 "$rc"
assert_contains "push: no session → message"         "No session was specified" "$out"

run "$RATCH" push s-noexist-push
assert_exit     "push: nonexistent session → exit 1" 1 "$rc"

# push to a running session (cat echoes input back to pty → appears in log)
"$RATCH" start s-push cat
sleep 0.05
printf 'atch-push-marker\n' | "$RATCH" push s-push
sleep 0.1
run grep -q "atch-push-marker" "$HOME/.cache/ratch/s-push.log"
assert_exit "push: data appears in session log"       0 "$rc"
tidy s-push

# extra args rejected
run "$RATCH" push s-noexist extra
assert_exit     "push: extra arg → exit 1"           1 "$rc"
assert_contains "push: extra arg → message"          "Invalid number of arguments" "$out"


# ── 9. attach / new / open — TTY requirement ─────────────────────────────────
# All three require a TTY. Running in a non-TTY environment they must fail
# with a clear message (not a crash or silent hang).

run "$RATCH" attach s-notty
assert_exit     "attach: no tty → exit 1"            1 "$rc"
assert_contains "attach: no tty → message"           "requires a terminal" "$out"


run "$RATCH" new s-notty
assert_exit     "new: no tty → exit 1"               1 "$rc"
assert_contains "new: no tty → message"              "requires a terminal" "$out"


# default open (ratch <session>) with no tty
run "$RATCH" s-notty-open
assert_exit     "open: no tty → exit 1"              1 "$rc"
assert_contains "open: no tty → message"             "requires a terminal" "$out"

# strict attach to nonexistent session still reports tty requirement first
run "$RATCH" attach s-noexist-notty
assert_exit     "attach: nonexistent + no tty → exit 1"     1 "$rc"
assert_contains "attach: nonexistent + no tty → message"    "requires a terminal" "$out"


# ── 10. -q global option (before subcommand) ────────────────────────────────

"$RATCH" start gq-s sleep 999

run "$RATCH" -q list
assert_exit     "-q before list: exits 0"            0 "$rc"
assert_contains "-q before list: still shows data"   "gq-s" "$out"

"$RATCH" kill gq-s > /dev/null
run "$RATCH" -q list
assert_exit     "-q before list: empty → exits 0"    0 "$rc"
assert_eq       "-q before list: empty → no output"  "" "$out"

run "$RATCH" -q start gq-s2 sleep 999
assert_exit     "-q before start: exits 0"           0 "$rc"
assert_eq       "-q before start: no output"         "" "$out"
tidy gq-s2

run "$RATCH" -q clear gq-noexist
assert_exit     "-q before clear: exits 0"           0 "$rc"
assert_eq       "-q before clear: no output"         "" "$out"

# ── 11. -e / -E flag ─────────────────────────────────────────────────────────

# -e with ^X notation
run "$RATCH" start -e "^A" e-test sleep 999
assert_exit     "-e ^A: exits 0"                     0 "$rc"
assert_contains "-e ^A: confirmation"                "started" "$out"
tidy e-test

# -e with literal char
run "$RATCH" start -e "~" e-test2 sleep 999
assert_exit     "-e ~: exits 0"                      0 "$rc"
tidy e-test2

# -e with ^? (DEL)
run "$RATCH" start -e "^?" e-test3 sleep 999
assert_exit     "-e ^?: exits 0"                     0 "$rc"
tidy e-test3

# -e missing argument
run "$RATCH" start -e
assert_exit     "-e missing arg: exit 1"             1 "$rc"
assert_contains "-e missing arg: message"            "No escape character" "$out"

# -E disables detach char
run "$RATCH" start -E e-nodetach sleep 999
assert_exit     "-E: exits 0"                        0 "$rc"
assert_contains "-E: confirmation"                   "started" "$out"
tidy e-nodetach

# ── 12. -r / -R flag ─────────────────────────────────────────────────────────

for method in none ctrl_l winch; do
    run "$RATCH" start -r "$method" r-test sleep 999
    assert_exit     "-r $method: exits 0"            0 "$rc"
    tidy r-test
done

run "$RATCH" start -r badmethod r-test sleep 999
assert_exit     "-r badmethod: exit 1"               1 "$rc"
assert_contains "-r badmethod: message"              "Invalid redraw method" "$out"

run "$RATCH" start -r
assert_exit     "-r missing arg: exit 1"             1 "$rc"
assert_contains "-r missing arg: message"            "No redraw method" "$out"

for method in none clear; do
    run "$RATCH" start -R "$method" R-test sleep 999
    assert_exit     "-R $method: exits 0"            0 "$rc"
    tidy R-test
done

run "$RATCH" start -R badmethod R-test sleep 999
assert_exit     "-R badmethod: exit 1"               1 "$rc"
assert_contains "-R badmethod: message"              "Invalid clear method" "$out"

run "$RATCH" start -R
assert_exit     "-R missing arg: exit 1"             1 "$rc"
assert_contains "-R missing arg: message"            "No clear method" "$out"

# ── 13. -z / -t flags ────────────────────────────────────────────────────────

run "$RATCH" start -z z-test sleep 999
assert_exit     "-z: exits 0"                        0 "$rc"
assert_contains "-z: confirmation"                   "started" "$out"
tidy z-test

run "$RATCH" start -t t-test sleep 999
assert_exit     "-t: exits 0"                        0 "$rc"
assert_contains "-t: confirmation"                   "started" "$out"
tidy t-test

# ── 14. combined / stacked flags ─────────────────────────────────────────────

# -qEzt combined in one flag
run "$RATCH" start -qEzt combo-test sleep 999
assert_exit     "combined -qEzt: exits 0"            0 "$rc"
assert_eq       "combined -qEzt: no output"          "" "$out"
tidy combo-test

# multiple separate flags before session
run "$RATCH" start -q -E -z -t combo2 sleep 999
assert_exit     "separate -q -E -z -t: exits 0"     0 "$rc"
assert_eq       "separate -q -E -z -t: no output"   "" "$out"
tidy combo2

# -e and -r together (e before r)
run "$RATCH" start -e "^B" -r none combo3 sleep 999
assert_exit     "-e + -r together: exits 0"          0 "$rc"
tidy combo3

# ── 15. -- separator ─────────────────────────────────────────────────────────

# A command that starts with - must be separated with --
run "$RATCH" start sep-test -- sleep 999
assert_exit     "-- separator: exits 0"              0 "$rc"
assert_contains "-- separator: confirmation"         "started" "$out"
tidy sep-test

# ── 16. invalid option ───────────────────────────────────────────────────────

run "$RATCH" start -x s-badopt sleep 999
assert_exit     "invalid option -x: exit 1"          1 "$rc"
assert_contains "invalid option -x: message"         "Invalid option" "$out"

# ── 17. custom socket path (name with /) ─────────────────────────────────────

CUSTOM_SOCK="$TESTDIR/custom-session"
run "$RATCH" start "$CUSTOM_SOCK" sleep 999
assert_exit     "custom path: exits 0"               0 "$rc"
assert_contains "custom path: confirmation"          "started" "$out"
[ -S "$CUSTOM_SOCK" ] && ok "custom path: socket created at given path" \
                        || fail "custom path: socket created at given path" "socket file" "not found"

run "$RATCH" list
assert_not_contains "custom path: not in default list" "custom-session" "$out"

run "$RATCH" kill "$CUSTOM_SOCK"
assert_exit     "custom path kill: exits 0"          0 "$rc"
assert_contains "custom path kill: stopped"          "stopped" "$out"

# ── 18. RATCH_SESSION env in child process ────────────────────────────────────

"$RATCH" start env-test sh -c "echo \"SESSION=\$RATCH_SESSION\" > $TESTDIR/env-test.txt"
sleep 0.1
run grep -q "SESSION=" $TESTDIR/env-test.txt
assert_exit "RATCH_SESSION set in child process"      0 "$rc"
rm -f $TESTDIR/env-test.txt

# ── 19. session log written on start ─────────────────────────────────────────

"$RATCH" start log-test sh -c 'printf "hello-log-test\n"; sleep 999'
sleep 0.1
run grep -q "hello-log-test" "$HOME/.cache/ratch/log-test.log"
assert_exit "session output written to log"          0 "$rc"
tidy log-test

# ── 20. -C log cap flag ──────────────────────────────────────────────────────

# valid sizes: 0 (disable), k/K suffix, m/M suffix, bare bytes
run "$RATCH" start -C 0 C-test0 sleep 999
assert_exit     "-C 0: exits 0"                      0 "$rc"
assert_contains "-C 0: confirmation"                 "started" "$out"
# log file must NOT exist when logging is disabled
if [ -e "$HOME/.cache/ratch/C-test0.log" ]; then
    fail "-C 0: no log file created" "no file" "file exists"
else
    ok "-C 0: no log file created"
fi
tidy C-test0

# -C before the subcommand (global pre-pass position)
run "$RATCH" -C 0 start C-global sleep 999
assert_exit     "-C 0 (global position): exits 0"    0 "$rc"
if [ -e "$HOME/.cache/ratch/C-global.log" ]; then
    fail "-C 0 (global position): no log file" "no file" "file exists"
else
    ok "-C 0 (global position): no log file"
fi
tidy C-global

run "$RATCH" start -C 128k C-128k sleep 999
assert_exit     "-C 128k: exits 0"                   0 "$rc"
assert_contains "-C 128k: confirmation"              "started" "$out"
tidy C-128k

run "$RATCH" start -C 4m C-4m sleep 999
assert_exit     "-C 4m: exits 0"                     0 "$rc"
assert_contains "-C 4m: confirmation"                "started" "$out"
tidy C-4m

run "$RATCH" start -C 65536 C-bytes sleep 999
assert_exit     "-C <bytes>: exits 0"                0 "$rc"
tidy C-bytes

# missing argument
run "$RATCH" start -C
assert_exit     "-C missing arg: exit 1"             1 "$rc"
assert_contains "-C missing arg: message"            "No log size" "$out"

# invalid value
run "$RATCH" start -C foo C-bad sleep 999
assert_exit     "-C invalid: exit 1"                 1 "$rc"
assert_contains "-C invalid: message"                "Invalid log size" "$out"

# ── 21. tail command ─────────────────────────────────────────────────────────

# tail with no session
run "$RATCH" tail
assert_exit     "tail: no session → exit 1"          1 "$rc"
assert_contains "tail: no session → message"         "No session was specified" "$out"

# tail on nonexistent session (no log file)
run "$RATCH" tail s-noexist-tail
assert_exit     "tail: no log → exit 1"              1 "$rc"
assert_contains "tail: no log → message"             "no log" "$out"

# tail basic: start session that writes 20 numbered lines then sleeps
# Use zero-padded numbers so e.g. "line01" is not a substring of "line11"
"$RATCH" start s-tail sh -c \
    'i=1; while [ $i -le 20 ]; do printf "line%02d\n" $i; i=$((i+1)); done; sleep 999'
sleep 0.2
# default 10 lines: should see line11..line20 but not line01
run "$RATCH" tail s-tail
assert_exit         "tail: exits 0"                       0 "$rc"
assert_contains     "tail: shows recent line"             "line20" "$out"
assert_not_contains "tail: omits early lines"             "line01" "$out"

# -n 5: should see line16..line20
run "$RATCH" tail -n 5 s-tail
assert_exit         "tail -n 5: exits 0"                  0 "$rc"
assert_contains     "tail -n 5: shows line in range"      "line20" "$out"
assert_not_contains "tail -n 5: omits earlier lines"      "line10" "$out"

# -n with combined flag style (-n5)
run "$RATCH" tail -n5 s-tail
assert_exit         "tail -n5 (compact): exits 0"         0 "$rc"
assert_contains     "tail -n5 (compact): shows line20"    "line20" "$out"

# extra args rejected
run "$RATCH" tail s-tail extra
assert_exit     "tail: extra arg → exit 1"           1 "$rc"
assert_contains "tail: extra arg → message"          "Invalid number of arguments" "$out"

# invalid option
run "$RATCH" tail -x s-tail
assert_exit     "tail: invalid option → exit 1"      1 "$rc"
assert_contains "tail: invalid option → message"     "Invalid option" "$out"

# -n missing argument
run "$RATCH" tail -n
assert_exit     "tail -n missing arg: exit 1"        1 "$rc"
assert_contains "tail -n missing arg: message"       "-n requires an argument" "$out"

tidy s-tail

# ── 22. rm command ───────────────────────────────────────────────────────────

# rm with no session and no -a
run "$RATCH" rm
assert_exit     "rm: no session → exit 1"            1 "$rc"
assert_contains "rm: no session → message"           "No session was specified" "$out"

# rm nonexistent session (no socket, no log)
run "$RATCH" rm s-noexist-rm
assert_exit     "rm: nonexistent → exit 1"           1 "$rc"
assert_contains "rm: nonexistent → message"          "does not exist" "$out"

# rm a running session must be refused
"$RATCH" start s-rm-live sleep 999
run "$RATCH" rm s-rm-live
assert_exit     "rm: running session → exit 1"       1 "$rc"
assert_contains "rm: running session → message"      "is running" "$out"
tidy s-rm-live
rm -f "$HOME/.cache/ratch/s-rm-live.log"

# rm a stale session: simulate stale via run+kill-9
"$RATCH" run s-rm-stale sleep 99999 &
RM_STALE_PID=$!
wait_socket s-rm-stale
kill -9 "$RM_STALE_PID" 2>/dev/null
sleep 0.1
printf "stale-output\n" > "$HOME/.cache/ratch/s-rm-stale.log"

run "$RATCH" rm s-rm-stale
assert_exit     "rm: stale → exit 0"                 0 "$rc"
assert_contains "rm: stale → message"                "removed" "$out"
# socket and log must be gone
if [ -e "$HOME/.cache/ratch/s-rm-stale" ]; then
    fail "rm: stale socket removed"   "gone" "still exists"
else
    ok   "rm: stale socket removed"
fi
if [ -e "$HOME/.cache/ratch/s-rm-stale.log" ]; then
    fail "rm: stale log removed"      "gone" "still exists"
else
    ok   "rm: stale log removed"
fi

# rm an exited session (log only, no socket)
printf "exited-output\n" > "$HOME/.cache/ratch/s-rm-exited.log"
run "$RATCH" rm s-rm-exited
assert_exit     "rm: exited → exit 0"                0 "$rc"
assert_contains "rm: exited → message"               "removed" "$out"
if [ -e "$HOME/.cache/ratch/s-rm-exited.log" ]; then
    fail "rm: exited log removed"     "gone" "still exists"
else
    ok   "rm: exited log removed"
fi

# rm: extra arg rejected
run "$RATCH" rm s-noexist-rm extra
assert_exit     "rm: extra arg → exit 1"             1 "$rc"
assert_contains "rm: extra arg → message"            "Invalid number of arguments" "$out"

# rm -a: nothing to remove (clean state)
rm -f "$HOME/.cache/ratch"/*.log 2>/dev/null || true
run "$RATCH" rm -a
assert_exit     "rm -a: nothing → exit 0"            0 "$rc"
assert_contains "rm -a: nothing → message"           "nothing to remove" "$out"

# rm -a: removes stale and exited sessions, leaves live ones alone
"$RATCH" run s-rma-stale sleep 99999 &
RMA_STALE_PID=$!
wait_socket s-rma-stale
kill -9 "$RMA_STALE_PID" 2>/dev/null
sleep 0.1
printf "stale\n" > "$HOME/.cache/ratch/s-rma-stale.log"
printf "exited\n" > "$HOME/.cache/ratch/s-rma-exited.log"
"$RATCH" start s-rma-live sleep 999

run "$RATCH" rm -a
assert_exit     "rm -a: exits 0"                     0 "$rc"
assert_contains "rm -a: reports removed sessions"    "removed" "$out"
assert_contains "rm -a: removes stale"               "s-rma-stale" "$out"
assert_contains "rm -a: removes exited"              "s-rma-exited" "$out"
assert_not_contains "rm -a: skips live session"      "s-rma-live" "$out"

# live session still present after rm -a
run "$RATCH" list
assert_contains "rm -a: live session still listed"   "s-rma-live" "$out"

tidy s-rma-live
rm -f "$HOME/.cache/ratch/s-rma-live.log"

# rm -a with extra arg rejected
run "$RATCH" rm -a extra
assert_exit     "rm -a: extra arg → exit 1"          1 "$rc"
assert_contains "rm -a: extra arg → message"         "Invalid number of arguments" "$out"

# ── 23. no-args → usage ──────────────────────────────────────────────────────

# Invoking with zero arguments calls usage() (exits 0, prints help).
# We already consumed the binary name in main, so argc < 1 → usage().
run "$RATCH"
assert_exit     "no args: exits 0 (usage)"           0 "$rc"
assert_contains "no args: shows Usage:"              "Usage:" "$out"

# tail and rm commands appear in help
run "$RATCH" --help
assert_contains "help: shows tail command"           "tail" "$out"
assert_contains "help: shows rm command"             "rm" "$out"

# ── summary ──────────────────────────────────────────────────────────────────

printf "\n1..%d\n" "$T"
printf "# %d passed, %d failed\n" "$PASS" "$FAIL"

[ "$FAIL" -eq 0 ]
