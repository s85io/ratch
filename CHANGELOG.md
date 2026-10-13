# Changelog

## 0.1.0

First release. `ratch` is a rewrite in Rust of
[atch](https://github.com/mobydeck/atch) and is not compatible with it: the
two cannot attach to each other's sessions, and the dtach-style mode flags
(`-a`, `-A`, `-c`, `-n`, `-N`, `-p`, `-k`) and one-letter command aliases
are gone.

Supported on Linux and macOS. On FreeBSD it builds, but the output of a
program that exits right after writing can be lost.

What is different from atch:

- A terminal that stops reading no longer stops the program. Each attached
  terminal reads the output at its own pace; one that falls too far behind
  skips ahead.
- Attaching sets the terminal up for the program (alternate screen, mouse,
  bracketed paste, keyboard protocol); detaching undoes it.
- Replayed history no longer makes the terminal answer old queries, which
  used to arrive as stray input.
- History is served by the session itself, the last 128 kB by default
  (`-n` to choose), rather than by replaying the whole log file.
- New commands: `log`, `wait`, `rename`, `env`.
- New options: `-n`, `--ro`, `-H`, `-R clear`, and defaults from `RATCH_OPTS`.
- `list` shows each session's process id and command, and how finished
  sessions ended; `list --names` is for scripts.
- `attach` to a session that is not running is an error and prints nothing
  else; `log` shows what it left.
- `kill` from inside the session it would kill asks for `-f`.
- A near miss of a command name (`ratch kil x`) is reported instead of
  creating a session by that name.
- Creating sessions is serialised by a lock; a dead session's socket is
  replaced rather than reported as running.
- The session directory must be private, and files in it are never opened
  through symbolic links.
- Shell completions for bash, zsh and fish.
