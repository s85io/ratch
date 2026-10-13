//! Layer 2: a session as it exists on disk.
//!
//! A session is a Unix socket plus a few files beside it that share its name:
//! `.log` (raw output), `.info` (who is running it and how it ended). The
//! directory they live in also holds `.lock`, which serialises everything
//! that creates or removes sockets.

#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rustix::fs::{AtFlags, FlockOperation, Mode, OFlags};

use crate::sys;

/// What a program is called decides where its sessions live and what its
/// environment variable is named, so that a renamed copy keeps to itself.
#[derive(Clone, Debug)]
pub struct Names {
    /// `argv[0]` as given; used in messages.
    pub progname: String,
    base: String,
}

impl Names {
    pub fn new(argv0: &OsStr) -> Names {
        let progname = argv0.to_string_lossy().into_owned();
        let base = progname.rsplit('/').next().unwrap_or(&progname).to_owned();
        Names { progname, base }
    }

    fn upper(&self) -> String {
        self.base
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// Holds the chain of session sockets a process is nested in.
    pub fn session_var(&self) -> String {
        format!("{}_SESSION", self.upper())
    }

    /// Overrides the directory sessions are kept in.
    pub fn dir_var(&self) -> String {
        format!("{}_SESSION_DIR", self.upper())
    }

    pub fn session_dir(&self) -> PathBuf {
        if let Some(dir) = std::env::var_os(self.dir_var()).filter(|d| !d.is_empty()) {
            return PathBuf::from(dir);
        }
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .or_else(|| sys::passwd_field(5).map(OsString::from));
        match home.filter(|h| h != "/") {
            Some(home) => Path::new(&home).join(".cache").join(&self.base),
            None => PathBuf::from(format!("/tmp/.{}-{}", self.base, sys::uid())),
        }
    }
}

fn refuse(path: &Path, why: &str) -> io::Error {
    io::Error::other(format!("{}: {}", path.display(), why))
}

/// Create the session directory if needed and make sure it is ours alone.
fn private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    if let Some(parent) = path.parent() {
        let _ = builder.create(parent);
    }
    let _ = builder.create(path);
    let meta = std::fs::symlink_metadata(path).map_err(|e| refuse(path, &e.to_string()))?;
    if !meta.is_dir() {
        return Err(refuse(path, "not a directory"));
    }
    if meta.uid() != sys::uid() {
        return Err(refuse(
            path,
            "refusing to use a directory owned by someone else",
        ));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(refuse(
            path,
            "refusing to use a directory others can access (must be 0700)",
        ));
    }
    Ok(())
}

pub struct Session {
    dir: OwnedFd,
    dir_path: PathBuf,
    /// Last component of the socket path.
    file: OsString,
    /// Full socket path, as it appears in the environment of the program.
    pub sock: PathBuf,
}

/// Held while sockets are created or removed. Dropping it releases the lock.
pub struct Lock(#[allow(dead_code)] OwnedFd);

/// Identity of a socket file, to recognise it later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockId {
    dev: u64,
    ino: u64,
}

pub enum Probe {
    /// A master answered.
    Live(UnixStream),
    /// A socket is there and nobody listens: the master died.
    Stale,
    Missing,
    /// Something else has that name.
    NotASocket,
    Failed(io::Error),
}

impl Session {
    /// A bare name is a session in the managed directory, which is created
    /// and checked. A name with a slash is a socket path used as given.
    pub fn open(names: &Names, name: &OsStr) -> io::Result<Session> {
        let path = Path::new(name);
        let managed = !name.as_bytes().contains(&b'/');
        let (dir_path, file) = if managed {
            let dir = names.session_dir();
            private_dir(&dir)?;
            (dir, name.to_owned())
        } else {
            let file = path
                .file_name()
                .ok_or_else(|| refuse(path, "not a socket path"))?;
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
            (parent.unwrap_or(Path::new(".")).to_owned(), file.to_owned())
        };
        let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        if managed {
            flags |= OFlags::NOFOLLOW;
        }
        let dir = rustix::fs::open(&dir_path, flags, Mode::empty())
            .map_err(|e| refuse(&dir_path, &io::Error::from(e).to_string()))?;
        let sock = if managed {
            dir_path.join(&file)
        } else {
            path.to_owned()
        };
        Ok(Session {
            dir,
            dir_path,
            file,
            sock,
        })
    }

    /// The session's name as the user typed it, minus any directory.
    pub fn name(&self) -> String {
        self.file.to_string_lossy().into_owned()
    }

    fn sidecar(&self, ext: &str) -> OsString {
        let mut name = self.file.clone();
        name.push(ext);
        name
    }

    /// Open one of the files that sit beside the socket. Symlinks are never
    /// followed: these names are predictable.
    pub fn open_file(&self, ext: &str, flags: OFlags) -> io::Result<File> {
        let fd = rustix::fs::openat(
            &self.dir,
            self.sidecar(ext),
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        Ok(File::from(fd))
    }

    pub fn remove_file(&self, ext: &str) -> io::Result<()> {
        Ok(rustix::fs::unlinkat(
            &self.dir,
            self.sidecar(ext),
            AtFlags::empty(),
        )?)
    }

    pub fn file_exists(&self, ext: &str) -> bool {
        rustix::fs::statat(&self.dir, self.sidecar(ext), AtFlags::SYMLINK_NOFOLLOW).is_ok()
    }

    /// Replace a sidecar file in one step, so a reader never sees half of it.
    pub fn replace_file(&self, ext: &str, contents: &[u8]) -> io::Result<()> {
        let tmp = format!("{ext}.tmp{}", std::process::id());
        let _ = self.remove_file(&tmp);
        let mut file = self.open_file(&tmp, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL)?;
        let written = file.write_all(contents);
        drop(file);
        let renamed = written.and_then(|()| {
            Ok(rustix::fs::renameat(
                &self.dir,
                self.sidecar(&tmp),
                &self.dir,
                self.sidecar(ext),
            )?)
        });
        if renamed.is_err() {
            let _ = self.remove_file(&tmp);
        }
        renamed
    }

    /// Take the directory's lock, waiting for whoever holds it.
    pub fn lock(&self) -> io::Result<Lock> {
        let open = || {
            rustix::fs::openat(
                &self.dir,
                ".lock",
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
        };
        // When several processes create the file at the same moment, macOS
        // can tell one of them it does not exist. It does by the next try.
        let mut fd = open();
        for _ in 0..16 {
            if !matches!(fd, Err(rustix::io::Errno::NOENT)) {
                break;
            }
            fd = open();
        }
        let fd = fd?;
        rustix::fs::flock(&fd, FlockOperation::LockExclusive)?;
        Ok(Lock(fd))
    }

    fn sock_stat(&self) -> io::Result<rustix::fs::Stat> {
        Ok(rustix::fs::statat(
            &self.dir,
            &self.file,
            AtFlags::SYMLINK_NOFOLLOW,
        )?)
    }

    // The stat field types differ from one system to the next.
    #[allow(clippy::unnecessary_cast)]
    pub fn sock_id(&self) -> Option<SockId> {
        let st = self.sock_stat().ok()?;
        Some(SockId {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        })
    }

    /// When the socket was created; a session's age is counted from here.
    pub fn started(&self) -> Option<SystemTime> {
        std::fs::symlink_metadata(&self.sock).ok()?.modified().ok()
    }

    /// Socket paths have a short fixed limit. When ours is over it, work
    /// from inside the directory with the last component alone.
    fn with_socket_path<T>(&self, f: impl Fn(&Path) -> io::Result<T>) -> io::Result<T> {
        match f(&self.sock) {
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                sys::in_dir(&self.dir, || f(Path::new(&self.file)))
            }
            other => other,
        }
    }

    pub fn probe(&self) -> Probe {
        match self.with_socket_path(|path| UnixStream::connect(path)) {
            Ok(stream) => Probe::Live(stream),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Probe::Missing,
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => match self.sock_stat() {
                Ok(st) if rustix::fs::FileType::from_raw_mode(st.st_mode).is_socket() => {
                    Probe::Stale
                }
                Ok(_) => Probe::NotASocket,
                Err(_) => Probe::Missing,
            },
            Err(e) => match self.sock_stat() {
                Ok(st) if !rustix::fs::FileType::from_raw_mode(st.st_mode).is_socket() => {
                    Probe::NotASocket
                }
                _ => Probe::Failed(e),
            },
        }
    }

    /// Create the socket, private from the instant it exists. The caller
    /// holds the lock and has dealt with whatever had the name before.
    pub fn bind(&self) -> io::Result<(UnixListener, SockId)> {
        let listener =
            sys::with_private_umask(|| self.with_socket_path(|path| UnixListener::bind(path)))?;
        listener.set_nonblocking(true)?;
        let id = self
            .sock_id()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok((listener, id))
    }

    /// Remove the socket if nobody listens on it. The caller holds the lock.
    pub fn remove_stale_socket(&self) -> bool {
        match self.probe() {
            Probe::Stale => self.remove_file("").is_ok(),
            _ => false,
        }
    }

    /// Remove the socket if it is still the one we created.
    pub fn remove_socket(&self, id: SockId) {
        if self.sock_id() == Some(id) {
            let _ = self.remove_file("");
        }
    }

    /// The owner-execute bit on the socket says "a client is attached". It
    /// costs nothing to read, which is what makes `list` cheap.
    pub fn set_attached(&self, attached: bool) {
        let mode = if attached { 0o700 } else { 0o600 };
        let _ = rustix::fs::chmodat(
            &self.dir,
            &self.file,
            Mode::from_raw_mode(mode),
            AtFlags::empty(),
        );
    }

    pub fn dir_path(&self) -> &Path {
        &self.dir_path
    }
}

/// What the master records about its session, for `list` and for whoever
/// asks later how it ended. A cache: the socket and the log are the truth.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    pub id: u64,
    pub pid: u32,
    pub child: u32,
    pub started: u64,
    pub command: String,
    pub ended: Option<u64>,
    pub exit: Option<u8>,
    pub signal: Option<u8>,
}

impl Info {
    pub fn write(&self, session: &Session) -> io::Result<()> {
        let command = escape(&self.command);
        let mut text = format!(
            "id={:016x}\nproto={}\npid={}\nchild={}\nstarted={}\ncmd={}\n",
            self.id,
            crate::proto::VERSION,
            self.pid,
            self.child,
            self.started,
            command
        );
        for (key, value) in [
            ("ended", self.ended),
            ("exit", self.exit.map(u64::from)),
            ("signal", self.signal.map(u64::from)),
        ] {
            if let Some(value) = value {
                text.push_str(&format!("{key}={value}\n"));
            }
        }
        session.replace_file(".info", text.as_bytes())
    }

    pub fn read(session: &Session) -> Option<Info> {
        let mut text = String::new();
        session
            .open_file(".info", OFlags::RDONLY)
            .ok()?
            .take(65536)
            .read_to_string(&mut text)
            .ok()?;
        let mut info = Info::default();
        for line in text.lines() {
            let (key, value) = line.split_once('=')?;
            match key {
                "id" => info.id = u64::from_str_radix(value, 16).ok()?,
                "pid" => info.pid = value.parse().ok()?,
                "child" => info.child = value.parse().ok()?,
                "started" => info.started = value.parse().ok()?,
                "cmd" => info.command = unescape(value),
                "ended" => info.ended = value.parse().ok(),
                "exit" => info.exit = value.parse().ok(),
                "signal" => info.signal = value.parse().ok(),
                _ => {}
            }
        }
        Some(info)
    }
}

/// Keep a value on one line: backslash and newline become `\\` and `\n`.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running {
        attached: bool,
    },
    Stale,
    /// Only a log is left.
    Exited,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: OsString,
    pub state: State,
    pub since: SystemTime,
}

/// Every session in the managed directory, in directory order.
/// A missing directory is an empty list.
pub fn list(names: &Names, include_exited: bool) -> io::Result<Vec<Entry>> {
    use std::os::unix::fs::FileTypeExt;
    let dir = names.session_dir();
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(refuse(&dir, &e.to_string())),
    };
    let mut running = Vec::new();
    let mut logs = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        if name.as_bytes().starts_with(b".") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let since = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if meta.file_type().is_socket() {
            let state = match UnixStream::connect(entry.path()) {
                Ok(_) => State::Running {
                    attached: meta.permissions().mode() & 0o100 != 0,
                },
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => State::Stale,
                Err(_) => continue,
            };
            running.push(Entry { name, state, since });
        } else if let Some(stem) = name
            .as_bytes()
            .strip_suffix(b".log")
            .filter(|s| !s.is_empty())
        {
            logs.push(Entry {
                name: OsString::from_vec(stem.to_vec()),
                state: State::Exited,
                since,
            });
        }
    }
    if include_exited {
        logs.retain(|log| !dir.join(&log.name).exists());
        running.append(&mut logs);
    }
    Ok(running)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh private directory, as `HOME` would give us, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            // Short on purpose: socket paths must fit in sun_path.
            let path = PathBuf::from(format!("/tmp/atch-t{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Scratch(path)
        }

        fn session(&self, name: &str) -> Session {
            Session::open(
                &Names::new(OsStr::new("atch")),
                self.0.join(name).as_os_str(),
            )
            .unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn names_follow_the_binary() {
        let names = Names::new(OsStr::new("/usr/local/bin/ssh2incus-atch"));
        assert_eq!(names.session_var(), "SSH2INCUS_ATCH_SESSION");
        assert_eq!(names.dir_var(), "SSH2INCUS_ATCH_SESSION_DIR");
        assert_eq!(names.progname, "/usr/local/bin/ssh2incus-atch");
        assert_eq!(Names::new(OsStr::new("atch")).session_var(), "ATCH_SESSION");
    }

    #[test]
    fn a_socket_is_private_from_the_start_and_probes_live() {
        let scratch = Scratch::new("bind");
        let session = scratch.session("s");
        assert!(matches!(session.probe(), Probe::Missing));
        let (_listener, id) = session.bind().unwrap();
        let mode = std::fs::metadata(&session.sock)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(matches!(session.probe(), Probe::Live(_)));
        assert_eq!(session.sock_id(), Some(id));
        session.set_attached(true);
        assert_eq!(
            std::fs::metadata(&session.sock)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn a_dead_socket_is_stale_and_only_then_removed() {
        let scratch = Scratch::new("stale");
        let session = scratch.session("s");
        let (listener, id) = session.bind().unwrap();
        assert!(
            !session.remove_stale_socket(),
            "a live socket must be left alone"
        );
        drop(listener);
        assert!(matches!(session.probe(), Probe::Stale));
        // A master that outlived its socket must not remove its successor's.
        assert!(session.remove_stale_socket());
        let (_second, second_id) = session.bind().unwrap();
        session.remove_socket(id);
        if id != second_id {
            assert!(session.sock.exists(), "removed a socket that was not ours");
        }
        session.remove_socket(second_id);
        assert!(!session.sock.exists());
    }

    #[test]
    fn other_things_with_the_name_are_not_sessions() {
        let scratch = Scratch::new("notsock");
        let session = scratch.session("s");
        std::fs::write(&session.sock, b"just a file").unwrap();
        assert!(matches!(session.probe(), Probe::NotASocket));
        assert!(!session.remove_stale_socket());
        assert!(session.sock.exists());
    }

    #[test]
    fn long_socket_paths_work() {
        let scratch = Scratch::new("long");
        let deep = scratch.0.join("d".repeat(120));
        std::fs::create_dir(&deep).unwrap();
        // Tests share a process, and another one may have the umask tightened.
        std::fs::set_permissions(&deep, std::fs::Permissions::from_mode(0o700)).unwrap();
        let session =
            Session::open(&Names::new(OsStr::new("atch")), deep.join("s").as_os_str()).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let (_listener, _) = session.bind().unwrap();
        assert!(matches!(session.probe(), Probe::Live(_)));
        assert_eq!(
            std::env::current_dir().unwrap(),
            cwd,
            "working directory not restored"
        );
    }

    #[test]
    fn sidecar_files_do_not_follow_symlinks() {
        let scratch = Scratch::new("symlink");
        let session = scratch.session("s");
        let victim = scratch.0.join("victim");
        std::fs::write(&victim, b"precious").unwrap();
        std::os::unix::fs::symlink(&victim, scratch.0.join("s.log")).unwrap();
        assert!(session
            .open_file(".log", OFlags::RDWR | OFlags::CREATE)
            .is_err());
        assert!(session
            .open_file(".log", OFlags::WRONLY | OFlags::TRUNC)
            .is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    }

    #[test]
    fn info_round_trips_and_is_replaced_whole() {
        let scratch = Scratch::new("info");
        let session = scratch.session("s");
        assert_eq!(Info::read(&session), None);
        let mut info = Info {
            id: 0xdead_beef,
            pid: 10,
            child: 11,
            started: 1_700_000_000,
            command: "sh -c printf 'a\\nb'\nsecond line".into(),
            ..Info::default()
        };
        info.write(&session).unwrap();
        assert_eq!(Info::read(&session), Some(info.clone()));
        info.ended = Some(1_700_000_005);
        info.exit = Some(3);
        info.write(&session).unwrap();
        assert_eq!(Info::read(&session), Some(info));
        let leftovers: Vec<_> = std::fs::read_dir(&scratch.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().contains("tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn the_lock_excludes_and_releases() {
        let scratch = Scratch::new("lock");
        let session = scratch.session("s");
        let lock = session.lock().unwrap();
        let other = scratch.session("s");
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _lock = other.lock().unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "lock not held"
        );
        drop(lock);
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("lock never released");
        waiter.join().unwrap();
    }

    #[test]
    fn a_shared_directory_is_refused() {
        let scratch = Scratch::new("perm");
        let dir = scratch.0.join("open");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&dir).is_err());
        let link = scratch.0.join("link");
        std::os::unix::fs::symlink(&scratch.0, &link).unwrap();
        assert!(private_dir(&link).is_err());
        assert!(private_dir(&scratch.0.join("fresh")).is_ok());
    }
}
