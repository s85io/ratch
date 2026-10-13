//! Layer 4: the command line.
//!
//! `ratch <session>` attaches to a session, creating it first if need be.
//! Everything else is `ratch <command> ...`.

#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::client::{self, Link, Options, Unreachable};
use crate::master::{self, format_age};
use crate::proto::{Redraw, Replay};
use crate::session::{self, Env, Info, Names, Probe, Session, State};
use crate::sys;

const DEFAULT_LOG_CAP: u64 = 1024 * 1024;
/// First argument of the hidden mode in which this binary is a master.
const MASTER_MODE: &str = "__master";

const COMMANDS: [&str; 16] = [
    "attach", "new", "start", "run", "push", "kill", "clear", "log", "tail", "wait", "list", "ls",
    "rm", "rename", "env", "current",
];

macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stdout(), $($arg)*);
    }};
}

/// The arguments not yet consumed.
struct Args {
    all: Vec<OsString>,
    at: usize,
}

impl Args {
    fn peek(&self) -> Option<&OsStr> {
        self.all.get(self.at).map(OsString::as_os_str)
    }

    fn next(&mut self) -> Option<OsString> {
        let arg = self.all.get(self.at).cloned();
        self.at += arg.is_some() as usize;
        arg
    }

    fn is_empty(&self) -> bool {
        self.at >= self.all.len()
    }

    fn rest(&mut self) -> Vec<OsString> {
        self.all.split_off(self.at)
    }

    /// Consume the next argument if it is exactly one of `names`.
    fn take(&mut self, names: &[&str]) -> bool {
        let hit = self
            .peek()
            .is_some_and(|arg| names.iter().any(|n| arg == *n));
        self.at += hit as usize;
        hit
    }
}

struct Cli {
    names: Names,
    opts: Options,
    log_cap: u64,
    /// Keep the session open with a shell after the command finishes.
    hold: bool,
}

/// A failed command: the message has been printed, this is the exit code.
type Failed = i32;

fn parse_size(text: &str) -> Option<u64> {
    let digits = text.trim_end_matches(['k', 'K', 'm', 'M']);
    let unit = match &text[digits.len()..] {
        "" => 1,
        "k" | "K" => 1024,
        "m" | "M" => 1024 * 1024,
        _ => return None,
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(unit)
}

/// One character added, removed, changed, or two neighbours swapped.
fn one_edit_apart(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if a == b || long.len() - short.len() > 1 {
        return false;
    }
    let same = short.iter().zip(long).take_while(|(x, y)| x == y).count();
    if short.len() == long.len() {
        let swapped = same + 1 < short.len()
            && short[same] == long[same + 1]
            && short[same + 1] == long[same]
            && short[same + 2..] == long[same + 2..];
        swapped || short[same + 1..] == long[same + 1..]
    } else {
        short[same..] == long[same + 1..]
    }
}

impl Cli {
    fn hint(&self) -> Failed {
        say!("Try '{} --help' for more information.", self.names.progname);
        1
    }

    fn fail(&self, message: &str) -> Failed {
        say!("{}: {}", self.names.progname, message);
        self.hint()
    }

    /// Consume option arguments, up to the first argument that is not one.
    fn parse_options(&mut self, args: &mut Args) -> Result<(), Failed> {
        while let Some(arg) = args.peek() {
            let bytes = arg.as_bytes();
            if bytes.len() < 2 || bytes[0] != b'-' {
                break;
            }
            let letters = bytes[1..].to_vec();
            args.next();
            match letters.as_slice() {
                b"-" => break,
                b"-ro" | b"-readonly" => {
                    self.opts.readonly = true;
                    continue;
                }
                [b'-', ..] => {
                    let option = String::from_utf8_lossy(&letters).into_owned();
                    return Err(self.fail(&format!("Invalid option '-{option}'")));
                }
                _ => {}
            }
            for &letter in &letters {
                let mut value = |what: &str| {
                    args.next()
                        .ok_or_else(|| self.fail(&format!("No {what} specified.")))
                };
                match letter {
                    b'E' => self.opts.detach = None,
                    b'z' => self.opts.no_suspend = true,
                    b'q' => self.opts.quiet = true,
                    b't' => self.opts.plain = true,
                    b'H' => self.hold = true,
                    b'e' => {
                        let key = value("escape character")?;
                        self.opts.detach = Some(match key.as_bytes() {
                            [b'^', b'?', ..] => 0x7f,
                            [b'^', c, ..] => c & 0x1f,
                            [c, ..] => *c,
                            [] => return Err(self.fail("No escape character specified.")),
                        });
                        break;
                    }
                    b'r' => {
                        self.opts.redraw = match value("redraw method")?.to_str() {
                            Some("none") => Redraw::None,
                            Some("ctrl_l") => Redraw::CtrlL,
                            Some("winch") => Redraw::Winch,
                            _ => return Err(self.fail("Invalid redraw method specified.")),
                        };
                        break;
                    }
                    b'R' => {
                        self.opts.clear = match value("clear method")?.to_str() {
                            Some("none") => false,
                            Some("clear") => true,
                            _ => return Err(self.fail("Invalid clear method specified.")),
                        };
                        break;
                    }
                    b'n' => {
                        let lines = value("line count")?;
                        self.opts.replay = match lines.to_str().and_then(|t| t.parse::<u32>().ok())
                        {
                            Some(0) => Replay::None,
                            Some(n) => Replay::Lines(n),
                            None => return Err(self.fail("Invalid line count specified.")),
                        };
                        break;
                    }
                    b'C' => {
                        let size = value("log size")?;
                        let text = size.to_string_lossy();
                        self.log_cap = parse_size(&text)
                            .ok_or_else(|| self.fail(&format!("Invalid log size '{text}'.")))?;
                        break;
                    }
                    other => return Err(self.fail(&format!("Invalid option '-{}'", other as char))),
                }
            }
        }
        Ok(())
    }

    /// Options from the environment, applied before the command line's.
    fn parse_env_options(&mut self) -> Result<(), Failed> {
        let var = self.names.opts_var();
        let Some(value) = std::env::var_os(&var) else {
            return Ok(());
        };
        let words = value
            .to_string_lossy()
            .split_whitespace()
            .map(OsString::from)
            .collect();
        let mut args = Args { all: words, at: 0 };
        self.parse_options(&mut args)?;
        if !args.is_empty() {
            return Err(self.fail(&format!("{var} may only contain options")));
        }
        Ok(())
    }

    fn session(&self, name: &OsStr) -> Result<Session, Failed> {
        Session::open(&self.names, name).map_err(|e| {
            say!("{}: {}", self.names.progname, e);
            1
        })
    }

    fn take_session(&self, args: &mut Args) -> Result<Session, Failed> {
        match args.next() {
            Some(name) => self.session(&name),
            None => Err(self.fail("No session was specified.")),
        }
    }

    /// The session named next, or else the innermost one we are running in.
    fn session_or_current(&self, args: &mut Args) -> Result<Session, Failed> {
        if let Some(name) = args.next() {
            return self.session(&name);
        }
        let chain = std::env::var_os(self.names.session_var()).filter(|c| !c.is_empty());
        let Some(chain) = chain else {
            return Err(self.fail("No session was specified."));
        };
        let innermost = chain
            .as_bytes()
            .rsplit(|&b| b == b':')
            .next()
            .unwrap_or_default();
        self.session(OsStr::from_bytes(innermost))
    }

    fn no_more(&self, args: &Args) -> Result<(), Failed> {
        if args.is_empty() {
            Ok(())
        } else {
            Err(self.fail("Invalid number of arguments."))
        }
    }

    fn require_tty(&self) -> Result<(), Failed> {
        if sys::termios_of(io::stdin()).is_some() {
            return Ok(());
        }
        say!(
            "{}: attaching to a session requires a terminal.",
            self.names.progname
        );
        Err(1)
    }

    /// `[options] <session> [options]`, the shape most commands share.
    fn session_and_options(&mut self, args: &mut Args) -> Result<Session, Failed> {
        self.parse_options(args)?;
        let session = self.take_session(args)?;
        self.parse_options(args)?;
        Ok(session)
    }

    /// What to run: what was asked for, or else the user's shell.
    fn command(&self, args: &mut Args) -> Vec<OsString> {
        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .or_else(|| sys::passwd_field(6).map(OsString::from))
            .unwrap_or_else(|| "/bin/sh".into());
        let given = args.rest();
        if given.is_empty() {
            return vec![shell];
        }
        if !self.hold {
            return given;
        }
        // Run the command, say how it went, and leave a shell in its place.
        let script = format!(
            "\"$@\"; echo \"[{}: command exited with status $?]\"; exec \"$0\"",
            self.names.progname
        );
        let mut argv = vec!["/bin/sh".into(), "-c".into(), script.into(), shell];
        argv.extend(given);
        argv
    }

    fn master_config(
        &self,
        argv: Vec<OsString>,
        wait_attach: bool,
        announce: bool,
    ) -> master::Config {
        master::Config {
            names: self.names.clone(),
            argv,
            wait_attach,
            redraw: self.opts.redraw,
            log_cap: self.log_cap,
            announce,
        }
    }

    /// Start a master for `session` as a process of its own and wait until
    /// it is serving. Its failures are relayed as they were printed.
    fn spawn_master(
        &self,
        session: &Session,
        argv: Vec<OsString>,
        wait_attach: bool,
    ) -> Result<(), Failed> {
        let prog = &self.names.progname;
        let exe = std::env::current_exe().unwrap_or_else(|_| prog.into());
        let child = Command::new(exe)
            .arg0(prog)
            .arg(MASTER_MODE)
            .arg(&session.sock)
            .arg(if wait_attach { "1" } else { "0" })
            .arg((self.opts.redraw as u8).to_string())
            .arg(self.log_cap.to_string())
            .args(argv)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut report = String::new();
        match child {
            Ok(mut child) => {
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut report);
                }
            }
            Err(e) => report = format!("{prog}: could not start session: {}\n", sys::describe(&e)),
        }
        match report.as_str() {
            "ok\n" => Ok(()),
            "" => {
                say!("{prog}: could not start session");
                Err(1)
            }
            message => {
                let _ = io::stdout().write_all(message.as_bytes());
                Err(1)
            }
        }
    }

    fn attach(&self, session: &Session) -> i32 {
        match client::connect(&self.names, session) {
            Some(link) => client::attach(&self.names, session, link, &self.opts),
            None => 1,
        }
    }

    fn created(&self, session: &Session, how: &str) {
        if !self.opts.quiet {
            say!(
                "{}: session '{}' {how}",
                self.names.progname,
                session.name()
            );
        }
    }

    fn cmd_attach(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let session = self.session_and_options(args)?;
        self.no_more(args)?;
        self.require_tty()?;
        Ok(self.attach(&session))
    }

    fn cmd_new(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let session = self.session_and_options(args)?;
        let argv = self.command(args);
        self.require_tty()?;
        self.spawn_master(&session, argv, true)?;
        self.created(&session, "created");
        Ok(self.attach(&session))
    }

    fn cmd_start(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let session = self.session_and_options(args)?;
        let argv = self.command(args);
        self.spawn_master(&session, argv, false)?;
        self.created(&session, "started");
        Ok(0)
    }

    fn cmd_run(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let session = self.session_and_options(args)?;
        let argv = self.command(args);
        Ok(master::run(
            session,
            &self.master_config(argv, false, false),
        ))
    }

    /// Attach if the session exists, otherwise create it and attach.
    fn cmd_open(&mut self, name: &OsStr, args: &mut Args) -> Result<i32, Failed> {
        self.reject_typo(name)?;
        let session = self.session(name)?;
        self.parse_options(args)?;
        let argv = self.command(args);
        self.require_tty()?;
        match Link::open(&session) {
            Ok(link) => return Ok(client::attach(&self.names, &session, link, &self.opts)),
            Err(Unreachable::Missing | Unreachable::Stale) => {}
            // Let `attach` word the error.
            Err(_) => return Ok(self.attach(&session)),
        }
        if self.spawn_master(&session, argv, true).is_err() {
            // Someone else may have created it in the meantime.
            return Ok(match Link::open(&session) {
                Ok(link) => client::attach(&self.names, &session, link, &self.opts),
                Err(_) => 1,
            });
        }
        self.created(&session, "created");
        Ok(self.attach(&session))
    }

    /// A bare word is a session name, so a mistyped command would quietly
    /// create a session called `kil`. Catch the near misses, unless a
    /// session by that name is really there.
    fn reject_typo(&self, word: &OsStr) -> Result<(), Failed> {
        let Some(word) = word.to_str().filter(|w| w.len() >= 3) else {
            return Ok(());
        };
        let Some(command) = COMMANDS.iter().find(|c| one_edit_apart(word, c)) else {
            return Ok(());
        };
        let exists = self.session(OsStr::new(word)).is_ok_and(|session| {
            !matches!(session.probe(), Probe::Missing) || session.file_exists(".log")
        });
        if exists {
            return Ok(());
        }
        let prog = &self.names.progname;
        say!("{prog}: unknown command '{word}' (did you mean '{command}'?)");
        say!("To create a session with that name: {prog} new {word}");
        Err(1)
    }

    fn cmd_push(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let session = self.take_session(args)?;
        self.no_more(args)?;
        Ok(client::push(&self.names, &session))
    }

    fn cmd_kill(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let mut force = false;
        while args.take(&["-f", "--force"]) {
            force = true;
        }
        let session = self.take_session(args)?;
        while args.take(&["-f", "--force"]) {
            force = true;
        }
        self.no_more(args)?;
        Ok(client::kill(&self.names, &session, force, self.opts.quiet))
    }

    fn cmd_clear(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let session = self.session_or_current(args)?;
        self.no_more(args)?;
        Ok(client::clear(&self.names, &session, self.opts.quiet))
    }

    /// `log` and `tail` are one command with different defaults: everything,
    /// or the last ten lines.
    fn cmd_log(&mut self, args: &mut Args, default_lines: u32) -> Result<i32, Failed> {
        let mut follow = false;
        let mut lines = default_lines;
        let count = |cli: &Cli, text: &OsStr| {
            text.to_str()
                .and_then(|t| t.parse::<u32>().ok())
                .map(|n| n.max(1))
                .ok_or_else(|| {
                    cli.fail(&format!("Invalid line count '{}'", text.to_string_lossy()))
                })
        };
        while let Some(arg) = args
            .peek()
            .filter(|a| a.len() > 1 && a.as_bytes()[0] == b'-')
        {
            let arg = arg.to_owned();
            args.next();
            if arg == "-f" {
                follow = true;
            } else if arg == "-n" {
                let Some(value) = args.next() else {
                    return Err(self.fail("-n requires an argument"));
                };
                lines = count(self, &value)?;
            } else if let Some(value) = arg.as_bytes().strip_prefix(b"-n") {
                lines = count(self, OsStr::from_bytes(value))?;
            } else {
                return Err(self.fail(&format!("Invalid option '{}'", arg.to_string_lossy())));
            }
        }
        let session = self.take_session(args)?;
        self.no_more(args)?;
        Ok(client::tail(&self.names, &session, lines, follow))
    }

    fn cmd_wait(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let mut limit = None;
        if args.take(&["-t"]) {
            let seconds = args.next().and_then(|t| t.to_str()?.parse::<f64>().ok());
            match seconds.filter(|s| s.is_finite() && *s >= 0.0) {
                Some(seconds) => limit = Some(Duration::from_secs_f64(seconds)),
                None => return Err(self.fail("-t requires a number of seconds")),
            }
        }
        let session = self.take_session(args)?;
        self.no_more(args)?;
        Ok(client::wait(&self.names, &session, limit))
    }

    fn cmd_list(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let (mut all, mut names_only) = (false, false);
        loop {
            if args.take(&["-a"]) {
                all = true;
            } else if args.take(&["--names"]) {
                names_only = true;
            } else {
                break;
            }
        }
        self.no_more(args)?;
        let entries = session::list(&self.names, all).map_err(|e| {
            say!("{}: {}", self.names.progname, e);
            1
        })?;
        for entry in &entries {
            let name = entry.name.to_string_lossy();
            if names_only {
                say!("{name}");
                continue;
            }
            let age = format_age(entry.since.elapsed().map_or(0, |d| d.as_secs()));
            let mark = match entry.state {
                State::Running { attached: true } => " [attached]",
                State::Running { attached: false } => "",
                State::Stale => " [stale]",
                State::Exited => " [exited]",
            };
            // What the master recorded about it.
            let info = self.session(&entry.name).ok().and_then(|s| Info::read(&s));
            let detail = match (entry.state, info) {
                (State::Running { .. }, Some(info)) => {
                    format!("  pid {}  {}", info.pid, info.command)
                }
                (State::Exited, Some(info)) => match (info.exit, info.signal) {
                    (Some(code), _) => format!("  exit {code}"),
                    (None, Some(sig)) => format!("  signal {sig}"),
                    (None, None) => String::new(),
                },
                _ => String::new(),
            };
            say!("{name:<24} since {age} ago{mark}{detail}");
        }
        if entries.is_empty() && !names_only && !self.opts.quiet {
            say!("(no sessions)");
        }
        Ok(0)
    }

    /// Delete what a dead session left behind. A socket is removed only if
    /// nobody is listening on it. True if there was something to remove.
    fn remove_remains(session: &Session) -> bool {
        let Ok(_lock) = session.lock() else {
            return false;
        };
        let mut removed = session.remove_stale_socket();
        removed |= session.remove_file(".log").is_ok();
        for ext in [".info", ".env"] {
            let _ = session.remove_file(ext);
        }
        removed
    }

    fn cmd_rm(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let prog = self.names.progname.clone();
        if args.take(&["-a"]) {
            self.no_more(args)?;
            let entries = session::list(&self.names, true).map_err(|e| {
                say!("{prog}: {e}");
                1
            })?;
            let mut count = 0;
            for entry in entries
                .iter()
                .filter(|e| !matches!(e.state, State::Running { .. }))
            {
                let Ok(session) = self.session(&entry.name) else {
                    continue;
                };
                if Self::remove_remains(&session) {
                    count += 1;
                    if !self.opts.quiet {
                        say!("{prog}: removed {}", entry.name.to_string_lossy());
                    }
                }
            }
            if !self.opts.quiet {
                match count {
                    0 => say!("{prog}: nothing to remove"),
                    n => say!("{prog}: {n} session(s) removed"),
                }
            }
            return Ok(0);
        }
        let session = self.take_session(args)?;
        self.no_more(args)?;
        let name = session.name();
        match session.probe() {
            Probe::Live(_) => {
                say!("{prog}: session '{name}' is running (use '{prog} kill {name}' first)");
                Ok(1)
            }
            Probe::Failed(e) => {
                say!("{prog}: {}: {}", session.sock.display(), sys::describe(&e));
                Ok(1)
            }
            Probe::Stale | Probe::Missing | Probe::NotASocket => {
                if Self::remove_remains(&session) {
                    if !self.opts.quiet {
                        say!("{prog}: session '{name}' removed");
                    }
                    Ok(0)
                } else {
                    say!("{prog}: session '{name}' does not exist");
                    Ok(1)
                }
            }
        }
    }

    fn cmd_rename(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let session = self.take_session(args)?;
        let Some(new_name) = args.next() else {
            return Err(self.fail("No new name was specified."));
        };
        self.no_more(args)?;
        Ok(client::rename(
            &self.names,
            &session,
            &new_name,
            self.opts.quiet,
        ))
    }

    /// Print shell commands that bring a long-lived shell's environment up
    /// to date with the terminal now attached: `eval "$(ratch env)"`.
    fn cmd_env(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let mut fish = std::env::var("SHELL").is_ok_and(|s| s.ends_with("/fish"));
        loop {
            if args.take(&["--fish"]) {
                fish = true;
            } else if args.take(&["--sh"]) {
                fish = false;
            } else {
                break;
            }
        }
        let session = self.session_or_current(args)?;
        self.no_more(args)?;
        match Env::read(&session) {
            Some(env) => {
                let _ = io::stdout().write_all(env.shell_commands(fish).as_bytes());
                Ok(0)
            }
            None => {
                say!(
                    "{}: no terminal has attached to session '{}' yet",
                    self.names.progname,
                    session.name()
                );
                Ok(1)
            }
        }
    }

    /// Print the sessions this process is nested in, outermost first.
    fn cmd_current(&self) -> i32 {
        let Some(chain) = std::env::var_os(self.names.session_var()).filter(|c| !c.is_empty())
        else {
            return 1;
        };
        let names: Vec<String> = chain
            .as_bytes()
            .split(|&b| b == b':')
            .map(|path| {
                let base = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
                String::from_utf8_lossy(base).into_owned()
            })
            .collect();
        say!("{}", names.join(" > "));
        0
    }

    fn dispatch(&mut self, args: &mut Args) -> Result<i32, Failed> {
        self.parse_env_options()?;
        self.parse_options(args)?;
        let Some(first) = args.next() else {
            return Ok(usage(&self.names));
        };
        match first.to_str() {
            Some("attach") => self.cmd_attach(args),
            Some("new") => self.cmd_new(args),
            Some("start") => self.cmd_start(args),
            Some("run") => self.cmd_run(args),
            Some("push") => self.cmd_push(args),
            Some("kill") => self.cmd_kill(args),
            Some("clear") => self.cmd_clear(args),
            Some("log") => self.cmd_log(args, u32::MAX),
            Some("tail") => self.cmd_log(args, 10),
            Some("wait") => self.cmd_wait(args),
            Some("list" | "ls") => self.cmd_list(args),
            Some("rm") => self.cmd_rm(args),
            Some("rename") => self.cmd_rename(args),
            Some("env") => self.cmd_env(args),
            Some("current") => Ok(self.cmd_current()),
            _ => self.cmd_open(&first, args),
        }
    }
}

fn version(names: &Names) -> String {
    format!("{} {}", names.progname, env!("CARGO_PKG_VERSION"))
}

fn usage(names: &Names) -> i32 {
    let text = "\
Usage:
  ratch [options] <session> [command...]   Attach to a session, creating it if needed
  ratch [options] <command> ...

Commands:
  attach <session>              Attach; fail if the session is not running
  new    <session> [command...] Create a session and attach
  start  <session> [command...] Create a session, detached
  run    <session> [command...] Create a session, master in the foreground
  push   <session>              Send stdin to the session as typed input
  kill   [-f] <session>         Stop a session (SIGTERM, then SIGKILL; -f: at once)
  wait   [-t secs] <session>    Wait for a session to end; exit with its status
  log    [-f] [-n N] <session>  Print the session's output (-n: last N lines; -f: follow)
  tail   [-f] [-n N] <session>  The same, last 10 lines by default
  clear  [<session>]            Forget the session's history
  list   [-a] [--names]         List sessions (-a: also finished ones)
  rm     <session> | -a         Remove what a finished or dead session left behind
  rename <session> <new-name>   Rename a running session
  env    [--sh|--fish] [<session>]  Print commands to refresh a shell's environment
  current                       Print the session this shell is running in

Options:
  -e <char>    Detach character (default: ^\\)
  -E           No detach character
  -n <lines>   History to show on attach (default: last 128k; 0: none)
  --ro         Attach read-only: watch without typing
  -R <method>  Before attaching: none | clear (wipe screen and scrollback)
  -r <method>  Ask the program to redraw by: none | ctrl_l | winch
  -H           Keep the session open with a shell after the command ends
  -C <size>    Log size limit: 0 disables, e.g. 128k, 4m (default 1m)
  -z           Do not interpret the suspend key
  -q           Suppress messages
  -t           Assume nothing about the terminal: send it no escape sequences
";
    say!(
        "{}\n{}",
        version(names),
        text.replace("ratch", &names.progname)
    );
    0
}

/// `__master <socket> <wait> <redraw> <log cap> <command...>`: become the
/// master of a session. Not for people; see [`Cli::spawn_master`].
fn master_mode(names: Names, args: &mut Args) -> i32 {
    sys::close_inherited_fds();
    let mut field = || args.next().unwrap_or_default();
    let (sock, wait, redraw, log_cap) = (field(), field(), field(), field());
    let argv = args.rest();
    let number = |text: &OsStr| text.to_str().and_then(|t| t.parse::<u64>().ok());
    let (Some(redraw), Some(log_cap), false) = (number(&redraw), number(&log_cap), argv.is_empty())
    else {
        return 1;
    };
    let session = match Session::open(&names, &sock) {
        Ok(session) => session,
        Err(e) => {
            say!("{}: {}", names.progname, e);
            return 1;
        }
    };
    let cfg = master::Config {
        names,
        argv,
        wait_attach: wait == "1",
        redraw: match redraw {
            1 => Redraw::None,
            2 => Redraw::CtrlL,
            3 => Redraw::Winch,
            _ => Redraw::Unspec,
        },
        log_cap,
        announce: true,
    };
    master::run(session, &cfg)
}

/// Run the program; the return value is its exit code.
pub fn run(argv: Vec<OsString>) -> i32 {
    let names = Names::new(argv.first().map_or(OsStr::new("ratch"), |a| a.as_os_str()));
    let mut args = Args { all: argv, at: 1 };
    if args.take(&[MASTER_MODE]) {
        return master_mode(names, &mut args);
    }
    if args.is_empty() || args.take(&["--help", "-h", "help"]) {
        return usage(&names);
    }
    if args.take(&["--version", "-V"]) {
        say!("{}", version(&names));
        return 0;
    }
    let mut cli = Cli {
        names,
        opts: Options::default(),
        log_cap: DEFAULT_LOG_CAP,
        hold: false,
    };
    cli.dispatch(&mut args).unwrap_or_else(|code| code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_misses_of_commands() {
        for (typo, command) in [
            ("kil", "kill"),
            ("lsit", "list"),
            ("atach", "attach"),
            ("starr", "start"),
            ("logs", "log"),
            ("rnu", "run"),
        ] {
            assert!(one_edit_apart(typo, command), "{typo} / {command}");
        }
        for (word, command) in [
            ("kill", "kill"),
            ("work", "kill"),
            ("ki", "kill"),
            ("build", "kill"),
            ("lits2", "list"),
            ("tial5", "tail"),
        ] {
            assert!(!one_edit_apart(word, command), "{word} / {command}");
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size("65536"), Some(65536));
        assert_eq!(parse_size("128k"), Some(128 * 1024));
        assert_eq!(parse_size("4M"), Some(4 * 1024 * 1024));
        for bad in [
            "",
            "k",
            "foo",
            "12x",
            "1kk",
            "-1",
            "1.5m",
            "99999999999999999999",
            "18446744073709551615k",
        ] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }
}
