# ratch

`ratch` keeps a terminal program running after you leave, and lets you come
back to it later, from the same terminal or another one. It does what the
detach feature of `screen` and `tmux` does, and nothing else: no windows, no
panes, no status line, no terminal emulation.

```sh
ratch work            # attach to "work", creating it (with your shell) if needed
                      # ... Ctrl-\ detaches; the shell keeps running
ratch work            # back where you left off
```

It is one static binary with no configuration file.

## Why not screen or tmux

Those are terminal emulators: they read what your program draws, keep their
own picture of the screen, and redraw it on your terminal. Whatever your
terminal can do that they have not learned yet — true colour, mouse
protocols, the kitty keyboard protocol, images, new escape sequences — is
lost or needs configuring.

`ratch` does not sit in between. Bytes from the program reach your terminal
unchanged, and your keys reach the program unchanged. `$TERM` stays what it
was. Mouse, scrolling, colours and graphics work the way they do without it.

The price of having no picture of the screen is that `ratch` cannot repaint
it for you. When you attach it does two things instead: it puts your terminal
into the modes the program switched on (alternate screen, mouse reporting,
bracketed paste, keyboard protocol), and it tells the program that the window
changed, which makes full-screen programs repaint themselves. For programs
that just print, it replays the recent output.

## Install

Download a binary from the [releases page](https://github.com/s85io/ratch/releases),
or build it:

```sh
cargo build --release      # target/release/ratch
make install               # into /usr/local, with man page and completions
```

Linux binaries are fully static. On macOS and FreeBSD the only dependency is
the system C library.

## Usage

```
ratch [options] <session> [command...]   attach to a session, creating it if needed
ratch [options] <command> ...
```

A session name is a plain word. A name containing `/` is taken as the path of
the session's socket, for sessions kept somewhere of your choosing.

## Commands

| Command | |
|---|---|
| `attach <session>` | Attach; fail if the session is not running |
| `new <session> [command...]` | Create a session and attach |
| `start <session> [command...]` | Create a session without attaching |
| `run <session> [command...]` | Create a session and stay in the foreground as its master (for service managers) |
| `push <session>` | Send stdin to the session as if typed |
| `kill [-f] <session>` | Stop a session: SIGTERM, then SIGKILL after 5 s. `-f` skips the wait |
| `wait [-t secs] <session>` | Wait for a session to end and exit with the program's status |
| `log [-f] [-n N] <session>` | Print the session's output; `-n` the last N lines, `-f` keep following |
| `tail [-f] [-n N] <session>` | The same, last 10 lines by default |
| `clear [<session>]` | Forget the session's history |
| `list [-a] [--names]` | List sessions; `-a` includes finished ones |
| `rm <session>` / `rm -a` | Remove what a finished or dead session left behind |
| `rename <session> <new-name>` | Rename a running session |
| `env [--sh\|--fish] [<session>]` | Print commands that refresh a shell's environment |
| `current` | Print the session this shell is running in |

Without a command, a session runs your `$SHELL`.

`wait` exits with the program's own exit code, `128 + N` if signal N killed
it, `124` if `-t` ran out first, and `255` if the session ended without
saying how (its master was killed).

## Options

Options go before the command, or after the session name.

| Option | |
|---|---|
| `-e <char>` | Detach character, as a character or `^X` (default `^\`) |
| `-E` | No detach character |
| `-n <lines>` | How much history to show on attach (default: the last 128 kB; `0`: none) |
| `--ro` | Attach read-only: watch without typing |
| `-R none\|clear` | `clear` wipes the screen and its scrollback before attaching |
| `-r none\|ctrl_l\|winch` | How to ask the program to redraw (default `winch`) |
| `-H` | Keep the session open with a shell after the command ends |
| `-C <size>` | Log size limit, such as `128k` or `4m`; `0` keeps no log (default `1m`) |
| `-z` | Do not interpret the suspend key (`^Z`) |
| `-q` | Suppress messages |
| `-t` | Assume nothing about the terminal: send it no escape sequences |

Options you always want can go in `RATCH_OPTS`:

```sh
export RATCH_OPTS="-e ^A -R clear"
```

## Examples

```sh
# A long build you can walk away from
ratch start build make -j8
ratch log -f build            # watch it
ratch wait build && notify-send "build done"

# A server, kept after it exits so you can read what it said
ratch start -H api ./server --port 8080

# Look in on something without the risk of typing into it
ratch attach --ro api

# Send a command to a shell running in a session
echo "git pull" | ratch push work

# Tidy up
ratch list -a
ratch rm -a
```

After attaching from a different terminal, a long-lived shell still has the
environment of the terminal it was started from. `env` prints what changed:

```sh
eval "$(ratch env)"           # bash, zsh
ratch env --fish | source     # fish
```

It covers `TERM`, `COLORTERM`, `DISPLAY`, `WAYLAND_DISPLAY`, the `SSH_*`
variables including `SSH_AUTH_SOCK`, and kitty's; name more in
`RATCH_ENV_VARS`.

## How it behaves

**Nothing waits for a slow terminal.** The program's output goes into a 1 MB
buffer in memory and each attached terminal reads from it at its own pace. A
terminal that falls more than the buffer behind (a hung SSH connection, a
paused screen) skips ahead; the program and the other terminals never notice.

**History.** Output is also appended to `<session>.log`, trimmed to the size
limit. Attaching replays the recent part; `log` prints all of it. A new
session with the name of an old one starts with the old one's history.
Recorded output often contains questions a program asked its terminal; these
are taken out of the replay, so your terminal does not answer them again.

**Leaving.** Detaching, or the program ending, switches off whatever terminal
modes the program switched on, so you are not left with a terminal that
reports mouse clicks into your shell.

**Limits.** With no picture of the screen, what you see after attaching to a
full-screen program is what the program repaints. One that ignores window
changes stays blank until it draws again; `-r ctrl_l` may suit it better.
Several terminals attached to one session share one window size: the last
one to attach or resize sets it.

## Files and environment

Sessions live in `~/.cache/ratch` (or `$RATCH_SESSION_DIR`; without a home
directory, `/tmp/.ratch-<uid>`). The directory must be yours and mode 0700.

| | |
|---|---|
| `<session>` | the session's socket |
| `<session>.log` | its output |
| `<session>.info` | its process ids, command, and how it ended |
| `<session>.env` | the environment of the terminal that attached last |

Inside a session, `RATCH_SESSION` holds the socket path, and for a session
started within a session, the chain of them separated by `:`. `ratch current`
prints it readably. A session cannot be attached, or killed without `-f`,
from inside itself.

A copy of the binary under another name keeps to itself: `foo` uses
`~/.cache/foo`, `FOO_SESSION`, `FOO_OPTS`.

**Who can do what.** Any process running as you can attach to, type into and
stop your sessions; nobody else can. That is the socket's permissions and
nothing more.

**systemd.** Where logind is configured with `KillUserProcesses=yes`, sessions
die when you log out like everything else of yours. `loginctl enable-linger`
or starting them with `systemd-run --user --scope ratch start ...` keeps them.

## Building and testing

```sh
make            # cargo build --release
make check      # unit, integration and black-box tests
```

The source is in layers, each using only those below it: `cli`; `client` and
`master`; `session` (what is on disk); `proto`, `ring` and `vt` (pure logic,
no I/O); `sys` (the operating system, and the only place with `unsafe`).

## License

GPL-2.0-or-later. `ratch` is a rewrite in Rust of
[atch](https://github.com/mobydeck/atch), itself based on
[dtach](https://github.com/crigler/dtach) by Ned T. Crigler.
