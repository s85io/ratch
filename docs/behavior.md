# Behaviour matrix: C (`c-final`) to Rust

What the C implementation does today, and whether the Rust implementation
keeps it. `tests/test.sh` is the executable form of the "keep" rows; this file
is where a deliberate change gets written down before the test is changed.

## Invocation

| Form | C behaviour | Rust |
|---|---|---|
| `atch` (no arguments), `--help`, `-h`, `?` | usage, exit 0 | keep |
| `--version` | `atch - version V, compiled on ...` | keep the prefix; drop the build date |
| `atch <session> [cmd...]` | attach, or create and attach | keep |
| options before the command or after the session name | accepted in both places | keep |
| `--` | ends option parsing | keep |
| unknown option | `Invalid option '-x'`, hint, exit 1 | keep |

## Commands

| Command | Aliases | Legacy flag | C behaviour | Rust |
|---|---|---|---|---|
| `attach <s>` | `a` | `-a` | attach; fail if missing | keep, see change 1 |
| `new <s> [cmd]` | `n` | `-c` | create and attach | keep |
| `start <s> [cmd]` | `s` | `-n` | create detached | keep |
| `run <s> [cmd]` | | `-N` | create; master stays in the foreground, same pid | keep |
| (default) | | `-A` | attach or create | keep |
| `push <s>` | `p` | `-p` | stdin to the session | keep |
| `kill [-f] <s>` | `k` | `-k` | TERM, 5 s grace, then KILL; `-f` KILL at once | keep |
| `clear [<s>]` | | | truncate the log; without a name, the current session | keep, see change 4 |
| `tail [-f] [-n N] <s>` | | | last N lines of the log (default 10) | keep, see change 5 |
| `list [-a]` | `l`, `ls` | `-l` | sessions; `-a` adds exited ones (a log without a socket) | keep, see change 3 |
| `rm <s>`, `rm -a` | | | remove a stale or exited session; refuse a running one | keep |
| `current` | | `-i` | ancestry `outer > inner`; exit 1 outside a session | keep |

Messages and exit codes of all of the above are pinned by `tests/test.sh`.

## Options

| Option | Meaning | Rust |
|---|---|---|
| `-e <char>` | detach character, `^X` notation, default `^\` | keep |
| `-E` | no detach character | keep |
| `-r none\|ctrl_l\|winch` | redraw method; default `winch` with a tty, `none` without | keep, including the termios check before sending `^L` |
| `-R none\|move` | what to do with the screen on attach | keep; `clear` is added later |
| `-z` | do not interpret the suspend key | keep |
| `-q` | suppress messages | keep |
| `-t` | no VT100 assumptions | keep; also turns off mode restore and reset |
| `-C <size>` | log cap, `0` disables the log, `k`/`m` suffixes, default 1 MB | keep, see change 2 |

## Environment and files

| Item | C behaviour | Rust |
|---|---|---|
| session directory | `$HOME/.cache/<binary name>`, else `/tmp/.<binary name>-<uid>` | keep; `ATCH_SESSION_DIR` overrides |
| a name containing `/` | used as the socket path as is | keep |
| paths longer than `sun_path` | handled by changing directory | keep |
| `<BINARY>_SESSION` in the program's environment | colon-separated chain of socket paths, outermost first | keep; a parallel chain of session ids is added |
| attaching to a session from inside itself, directly or through nesting | refused | keep |
| socket mode | `0600`; the owner execute bit means "a client is attached" | keep |
| `<session>.log` | raw output, capped, ends with `[atch: session 'x' ended after ...]` | keep |
| program to run when none is given | `$SHELL`, else the passwd shell, else `/bin/sh` | keep |

## Deliberate changes

1. **`attach` to a session that is not running prints an error and nothing
   else.** C replays the old log first (upstream issue #20). `atch log` shows it.
2. **`-C` no longer sizes the in-memory history.** The ring is a fixed 1 MB and
   independent of the log; `-C 0` only turns off the file.
3. **`list` gains columns** (pid, command). The name stays first and the
   `[attached]`, `[stale]`, `[exited]` markers stay.
4. **`clear` on a running session goes through the master**, so the cleared
   history cannot come back from memory.
5. **`tail -f` on a running session reads from the master** and ends when the
   session ends.
6. **History on attach comes from the master only**, capped at the last
   128 KB by default. C has the client read the whole log from disk.
7. **A C client cannot talk to a Rust master.** A Rust client can attach to,
   push to and kill a session held by a C master.
8. **Creating a session over a dead socket just works.** C refuses with
   "already running" unless the default attach-or-create form is used.
9. **Typing during history replay is kept.** Only what a terminal sends on
   its own, in reply to a query found in the history, is dropped.
10. **Detaching leaves the terminal as it was found**: the modes the program
    switched on (alternate screen, mouse, bracketed paste, keyboard protocol)
    are undone before the parting message. C resets colours and the cursor only.
11. **The master closes descriptors it inherits**, other than the standard
    three. A program started with `start` or `new` no longer sees files the
    invoking shell had open; `run` passes them on as before.
12. **`-q` silences `kill` too.** C prints "stopped" regardless.
13. **No blank line is printed on attach**; `-R move` clears the screen and
    its scrollback instead of resetting the terminal.
14. **Without `$HOME` or `$SHELL`**, the fallback is the caller's line in
    `/etc/passwd`, where C asks the system's user database.
