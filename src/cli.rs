//! Layer 4: the command line.
//!
//! Two syntaxes are accepted. The current one is `atch <command> ...`, with
//! a bare `atch <session>` meaning "attach, or create and attach". The older
//! one, inherited from dtach, selects the mode with a flag: `atch -a <session>`.

#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use crate::client::{self, Clear, Link, Options, Unreachable};
use crate::master::{self, format_age};
use crate::proto::{Redraw, Replay};
use crate::session::{self, Names, Probe, Session, State};
use crate::sys;

const DEFAULT_LOG_CAP: u64 = 1024 * 1024;
/// First argument of the hidden mode in which this binary is a master.
const MASTER_MODE: &str = "__master";

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

impl Cli {
    fn hint(&self) -> Failed {
        say!("Try '{} --help' for more information.", self.names.progname);
        1
    }

    fn fail(&self, message: &str) -> Failed {
        say!("{}: {}", self.names.progname, message);
        self.hint()
    }

    /// Consume option arguments. With `global_only`, stop quietly at the
    /// first argument that is not an option, such as a legacy mode flag.
    fn parse_options(&mut self, args: &mut Args, global_only: bool) -> Result<(), Failed> {
        while let Some(arg) = args.peek() {
            let bytes = arg.as_bytes();
            if bytes.first() != Some(&b'-') {
                break;
            }
            if global_only && !bytes.get(1).is_some_and(|c| b"eErRzqtC".contains(c)) {
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
                            Some("none") => Clear::None,
                            Some("move") => Clear::Move,
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
        self.parse_options(args, false)?;
        let session = self.take_session(args)?;
        self.parse_options(args, false)?;
        Ok(session)
    }

    /// What to run: what was asked for, or else the user's shell.
    fn command(args: &mut Args) -> Vec<OsString> {
        let given = args.rest();
        if !given.is_empty() {
            return given;
        }
        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .or_else(|| sys::passwd_field(6).map(OsString::from))
            .unwrap_or_else(|| "/bin/sh".into());
        vec![shell]
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

    /// Attach if the session exists, otherwise create it and attach.
    fn open(&self, session: &Session, argv: Vec<OsString>, announce: bool) -> i32 {
        match Link::open(session) {
            Ok(link) => return client::attach(&self.names, session, link, &self.opts),
            Err(Unreachable::Missing | Unreachable::Stale) => {}
            Err(why) => {
                // Let `connect` word the error.
                drop(why);
                return self.attach(session);
            }
        }
        if self.spawn_master(session, argv, true).is_err() {
            // Someone else may have created it in the meantime.
            return match Link::open(session) {
                Ok(link) => client::attach(&self.names, session, link, &self.opts),
                Err(_) => 1,
            };
        }
        if announce && !self.opts.quiet {
            say!(
                "{}: session '{}' created",
                self.names.progname,
                session.name()
            );
        }
        self.attach(session)
    }

    fn cmd_attach(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let session = self.session_and_options(args)?;
        self.no_more(args)?;
        self.require_tty()?;
        Ok(self.attach(&session))
    }

    fn cmd_new(&mut self, args: &mut Args, announce: bool) -> Result<i32, Failed> {
        let session = self.session_and_options(args)?;
        let argv = Self::command(args);
        self.require_tty()?;
        self.spawn_master(&session, argv, true)?;
        if announce && !self.opts.quiet {
            say!(
                "{}: session '{}' created",
                self.names.progname,
                session.name()
            );
        }
        Ok(self.attach(&session))
    }

    fn cmd_start(&mut self, args: &mut Args, announce: bool) -> Result<i32, Failed> {
        let session = self.session_and_options(args)?;
        let argv = Self::command(args);
        self.spawn_master(&session, argv, false)?;
        if announce && !self.opts.quiet {
            say!(
                "{}: session '{}' started",
                self.names.progname,
                session.name()
            );
        }
        Ok(0)
    }

    fn cmd_run(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let session = self.session_and_options(args)?;
        let argv = Self::command(args);
        Ok(master::run(
            &session,
            &self.master_config(argv, false, false),
        ))
    }

    fn cmd_open(&mut self, name: &OsStr, args: &mut Args, announce: bool) -> Result<i32, Failed> {
        let session = self.session(name)?;
        self.parse_options(args, false)?;
        let argv = Self::command(args);
        self.require_tty()?;
        Ok(self.open(&session, argv, announce))
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
        let session = match args.next() {
            Some(name) => self.session(&name)?,
            None => {
                // Inside a session, no name means this one: the innermost.
                let chain = std::env::var_os(self.names.session_var()).filter(|c| !c.is_empty());
                let Some(chain) = chain else {
                    return Err(self.fail("No session was specified."));
                };
                let innermost = chain
                    .as_bytes()
                    .rsplit(|&b| b == b':')
                    .next()
                    .unwrap_or_default();
                self.session(OsStr::from_bytes(innermost))?
            }
        };
        self.no_more(args)?;
        Ok(client::clear(&self.names, &session, self.opts.quiet))
    }

    /// `tail` and `log` are one command with different defaults: the last
    /// ten lines, or everything.
    fn cmd_tail(&mut self, args: &mut Args, default_lines: u32) -> Result<i32, Failed> {
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
                Some(seconds) => limit = Some(std::time::Duration::from_secs_f64(seconds)),
                None => return Err(self.fail("-t requires a number of seconds")),
            }
        }
        let session = self.take_session(args)?;
        self.no_more(args)?;
        Ok(client::wait(&self.names, &session, limit))
    }

    fn cmd_list(&mut self, args: &mut Args) -> Result<i32, Failed> {
        let mut all = false;
        while args.take(&["-a"]) {
            all = true;
        }
        let entries = session::list(&self.names, all).map_err(|e| {
            say!("{}: {}", self.names.progname, e);
            1
        })?;
        for entry in &entries {
            let age = format_age(entry.since.elapsed().map_or(0, |d| d.as_secs()));
            let mark = match entry.state {
                State::Running { attached: true } => " [attached]",
                State::Running { attached: false } => "",
                State::Stale => " [stale]",
                State::Exited => " [exited]",
            };
            // What the master recorded, if this is one of ours.
            let info = self
                .session(&entry.name)
                .ok()
                .and_then(|s| session::Info::read(&s));
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
            say!(
                "{:<24} since {} ago{}{}",
                entry.name.to_string_lossy(),
                age,
                mark,
                detail
            );
        }
        if entries.is_empty() && !self.opts.quiet {
            say!("(no sessions)");
        }
        Ok(0)
    }

    /// Delete what a dead session left behind. The caller has checked that
    /// it is dead and holds no lock; a socket is removed only if still stale.
    fn remove_remains(session: &Session) -> bool {
        let Ok(_lock) = session.lock() else {
            return false;
        };
        let mut removed = session.remove_stale_socket();
        for ext in [".log", ".info", ".env"] {
            let was_log = ext == ".log";
            removed |= session.remove_file(ext).is_ok() && was_log;
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

    /// The dtach-style syntax: a mode flag, then the session.
    fn legacy(&mut self, mode: u8, args: &mut Args) -> Result<i32, Failed> {
        match mode {
            b'?' | b'h' => return Ok(usage()),
            b'l' => return self.cmd_list(args),
            b'i' => return Ok(self.cmd_current()),
            b'a' | b'A' | b'c' | b'n' | b'N' | b'p' | b'k' => {}
            other => return Err(self.fail(&format!("Invalid mode '-{}'", other as char))),
        }
        if args.is_empty() {
            return Err(self.fail("No session was specified."));
        }
        match mode {
            b'p' => self.cmd_push(args),
            b'k' => self.cmd_kill(args),
            b'a' => self.cmd_attach(args),
            b'n' => self.cmd_start(args, false),
            b'N' => self.cmd_run(args),
            b'c' => self.cmd_new(args, false),
            _ => {
                let name = args.next().unwrap_or_default();
                self.cmd_open(&name, args, false)
            }
        }
    }

    fn dispatch(&mut self, args: &mut Args) -> Result<i32, Failed> {
        self.parse_options(args, true)?;
        let Some(first) = args.next() else {
            return Ok(usage());
        };
        let bytes = first.as_bytes();
        if bytes.len() > 1 && bytes[0] == b'-' && bytes[1] != b'-' {
            return self.legacy(bytes[1], args);
        }
        match first.to_str() {
            Some("list" | "l" | "ls") => self.cmd_list(args),
            Some("current") => Ok(self.cmd_current()),
            Some("attach" | "a") => self.cmd_attach(args),
            Some("new" | "n") => self.cmd_new(args, true),
            Some("start" | "s") => self.cmd_start(args, true),
            Some("run") => self.cmd_run(args),
            Some("push" | "p") => self.cmd_push(args),
            Some("kill" | "k") => self.cmd_kill(args),
            Some("clear") => self.cmd_clear(args),
            Some("tail") => self.cmd_tail(args, 10),
            Some("log") => self.cmd_tail(args, u32::MAX),
            Some("wait") => self.cmd_wait(args),
            Some("rm") => self.cmd_rm(args),
            _ => {
                self.reject_typo(&first)?;
                self.cmd_open(&first, args, true)
            }
        }
    }
}

const COMMANDS: [&str; 13] = [
    "attach", "new", "start", "run", "push", "kill", "clear", "tail", "log", "wait", "list", "rm",
    "current",
];

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

fn version() -> String {
    format!("atch - version {}", env!("CARGO_PKG_VERSION"))
}

fn usage() -> i32 {
    say!(
        "{}.
Usage:
  atch [<session> [command...]]\t\tAttach to session or create it
  atch <command> [options] ...

Commands:
  attach  <session>\t\t\tStrict attach (fail if session missing)
  new     <session> [command...]\tCreate session and attach
  start   <session> [command...]\tCreate session, detached
  run     <session> [command...]\tCreate session, master in foreground
  push    <session>\t\t\tPipe stdin into session
  kill    [-f] <session>\t\tStop session (SIGTERM then SIGKILL)
    -f, --force\t\t\tSkip grace period, send SIGKILL immediately
  clear   [<session>]\t\t\tForget the session history
  tail    [-f] [-n N] <session>\tPrint last N lines of session output
    -f\t\t\t\tFollow output
    -n <lines>\t\t\tNumber of lines (default 10)
  log     [-f] [-n N] <session>\tPrint all session output (or last N lines)
  wait    [-t secs] <session>\t\tWait for session to end, exit with its code
  list    [-a]\t\t\t\tList sessions (-a includes exited)
  rm      <session> | -a\t\tRemove a stale or exited session (-a: all)
  current\t\t\t\tPrint current session name

Options:
  -e <char>\tSet detach character (default: ^\\)
  -E\t\tDisable detach character
  -r <method>\tRedraw method: none | ctrl_l | winch
  -R <method>\tClear method:  none | move
  -z\t\tDisable suspend key
  -q\t\tSuppress messages
  -t\t\tDisable VT100 assumptions
  -C <size>\tLog cap: 0=disable, e.g. 128k, 4m (default 1m)
  -n <lines>\tHistory to show on attach (default: last 128k; 0=none)
  --ro\t\tAttach read-only: watch without typing

URL: {}
",
        version(),
        env!("CARGO_PKG_REPOSITORY")
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
    master::run(&session, &cfg)
}

/// Run the program; the return value is its exit code.
pub fn run(argv: Vec<OsString>) -> i32 {
    let names = Names::new(argv.first().map_or(OsStr::new("atch"), |a| a.as_os_str()));
    let mut args = Args { all: argv, at: 1 };
    if args.take(&[MASTER_MODE]) {
        return master_mode(names, &mut args);
    }
    if args.is_empty() || args.take(&["--help", "-h", "?"]) {
        return usage();
    }
    if args.take(&["--version"]) {
        say!("{}.", version());
        return 0;
    }
    let mut cli = Cli {
        names,
        opts: Options::default(),
        log_cap: DEFAULT_LOG_CAP,
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
