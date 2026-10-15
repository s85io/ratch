# Changelog

## 0.2.0

For programs such as Claude Code and omp:

- Attaching at the size the session already has now gets the program to
  repaint. Programs on Node or Bun ignore a window change that keeps the
  size, so the screen stayed blank until the window was resized; the window
  is now made a column narrower for a moment, then put back.
- A read-only attach gets the program to repaint too, at the size it has.
- The program is asked to repaint even in a session created without a
  terminal (`start` from a script).
- The window's size in pixels reaches the program, for those that draw
  images.
- The reports a terminal sends unasked (window size, colour scheme, kitty
  paste events: modes 2048, 2031, 5522) are switched on again on attach and
  off on detach, so that they no longer arrive in the shell as junk.
- Detaching in the middle of a synchronized update no longer leaves the
  terminal's screen frozen.
- Attaching no longer flickers: the terminal holds its picture still while
  history and the program's repaint go in, and shows them at once. Until
  then it says that the session is being restored.
- The window has the size of the terminal used last (attached, typed or
  clicked in), not of the last one to attach or resize. When that terminal
  detaches, the one used before it gets its size back. A terminal that is
  not in use, such as a phone's in the background, no longer changes the
  size, and its in-band size reports no longer reach the program.
- The detach and suspend keys work when the program has switched on the
  kitty keyboard protocol or modifyOtherKeys, as omp does.

The protocol is now version 4. A new client still attaches to a session
started by 0.1.0, without the pixel size or the held picture; the master
side of these changes needs the session to be restarted.

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
