//! Control socket used by `--toggle` and `--close` to talk to the running instance.
//!
//! The running instance holds an exclusive flock on `$XDG_RUNTIME_DIR/wl-sidebar-<name>.pid`
//! (which contains its pid, for error messages) for its whole lifetime and listens on
//! `$XDG_RUNTIME_DIR/wl-sidebar-<name>.sock`. Whoever holds the lock owns the socket path,
//! so a stale socket left behind by a dead instance can be replaced safely.
//!
//! Protocol: the client sends one command line; the server answers with one line,
//! `ok <state>` or `error <message>`, and closes the connection.

use {
    crate::SidebarError,
    error_reporter::Report,
    std::{
        fs::{self, File, OpenOptions},
        io::{self, BufRead, BufReader, ErrorKind, Read, Write},
        os::{
            fd::AsRawFd,
            unix::net::{UnixListener, UnixStream},
        },
        path::{Path, PathBuf},
        thread,
        time::{Duration, Instant},
    },
    uapi::c,
};

/// How long either side waits for the other.
const TIMEOUT: Duration = Duration::from_secs(1);
/// Upper bound on the length of a command or reply line.
const MAX_LINE: u64 = 256;

fn runtime_path(name: &str, ext: &str) -> Result<PathBuf, SidebarError> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or(SidebarError::NoRuntimeDir)?;
    Ok(PathBuf::from(dir).join(format!("wl-sidebar-{name}.{ext}")))
}

// ---------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------

/// Becomes the running instance of sidebar `name` and serves commands with `handle` on a
/// background thread. `handle` returns the reply on success or an error message.
///
/// The returned file holds the instance lock and must be kept alive for the lifetime of
/// the process.
pub fn listen<F>(name: &str, handle: F) -> Result<File, SidebarError>
where
    F: Fn(&str) -> Result<String, String> + Send + 'static,
{
    let lock = lock_pidfile(name)?;
    let path = runtime_path(name, "sock")?;
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(SidebarError::Socket(path, e)),
    }
    let listener = UnixListener::bind(&path).map_err(|e| SidebarError::Socket(path, e))?;
    thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Err(e) = serve(&stream, &handle) {
                            log::warn!("control connection: {}", Report::new(e));
                        }
                    }
                    Err(e) => {
                        log::error!("could not accept a control connection: {}", Report::new(e));
                        // Don't spin on persistent errors such as EMFILE.
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        })
        .map_err(SidebarError::SpawnThread)?;
    Ok(lock)
}

fn serve(stream: &UnixStream, handle: &impl Fn(&str) -> Result<String, String>) -> io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_LINE)).read_line(&mut line)?;
    let cmd = line.trim();
    log::debug!("control command `{cmd}`");
    let reply = match handle(cmd) {
        Ok(state) => format!("ok {state}\n"),
        Err(e) => format!("error {e}\n"),
    };
    let mut stream = stream;
    stream.write_all(reply.as_bytes())
}

/// Locks the pid file of sidebar `name` and writes our pid into it.
fn lock_pidfile(name: &str) -> Result<File, SidebarError> {
    let path = runtime_path(name, "pid")?;
    let err = |e| SidebarError::Pidfile(path.clone(), e);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(err)?;
    if !try_lock(&file, c::LOCK_EX, &path)? {
        return Err(SidebarError::AlreadyRunning(name.to_owned()));
    }
    file.set_len(0).map_err(err)?;
    file.write_all(uapi::getpid().to_string().as_bytes())
        .map_err(err)?;
    Ok(file)
}

// ---------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------

/// Sends `cmd` to the running instance of sidebar `name` and returns its reply, or `None`
/// if no instance is running.
///
/// Fails with [`SidebarError::Unresponsive`] if an instance holds the lock but doesn't
/// answer, rather than pretending the command went through.
pub fn send(name: &str, cmd: &str) -> Result<Option<String>, SidebarError> {
    let path = runtime_path(name, "sock")?;
    let unresponsive = |e: io::Error| SidebarError::Unresponsive(name.to_owned(), pid(name), e);
    let stream = match connect(name, &path)? {
        Some(s) => s,
        None => return Ok(None),
    };
    let exchange = || -> io::Result<String> {
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        (&stream).write_all(format!("{cmd}\n").as_bytes())?;
        let mut line = String::new();
        if BufReader::new((&stream).take(MAX_LINE)).read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "connection closed without a reply",
            ));
        }
        Ok(line)
    };
    let line = exchange().map_err(|e| unresponsive(timeout_msg(e)))?;
    let line = line.trim_end();
    if let Some(state) = line.strip_prefix("ok") {
        Ok(Some(state.trim_start().to_owned()))
    } else if let Some(msg) = line.strip_prefix("error ") {
        Err(SidebarError::Remote(name.to_owned(), msg.to_owned()))
    } else {
        Err(unresponsive(io::Error::new(
            ErrorKind::InvalidData,
            format!("invalid reply `{line}`"),
        )))
    }
}

/// Connects to the control socket. `None` if no instance is running.
fn connect(name: &str, path: &Path) -> Result<Option<UnixStream>, SidebarError> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let e = match UnixStream::connect(path) {
            Ok(s) => return Ok(Some(s)),
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => e,
            Err(e) => return Err(SidebarError::Socket(path.to_owned(), e)),
        };
        if !running(name)? {
            return Ok(None);
        }
        // Locked but not listening: either it is just starting up, or it's broken.
        if Instant::now() >= deadline {
            return Err(SidebarError::Unresponsive(name.to_owned(), pid(name), e));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until the running instance of sidebar `name` (if any) has exited. Fails with
/// [`SidebarError::StillRunning`] after `timeout`.
pub fn wait_exit(name: &str, timeout: Duration) -> Result<(), SidebarError> {
    let deadline = Instant::now() + timeout;
    while running(name)? {
        if Instant::now() >= deadline {
            return Err(SidebarError::StillRunning(name.to_owned(), pid(name)));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Whether some process holds the instance lock of sidebar `name`.
pub fn running(name: &str) -> Result<bool, SidebarError> {
    let path = runtime_path(name, "pid")?;
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(SidebarError::Pidfile(path, e)),
    };
    // If we can lock it, nobody else holds it.
    Ok(!try_lock(&file, c::LOCK_SH, &path)?)
}

/// The pid of the running instance, for error messages.
fn pid(name: &str) -> Option<c::pid_t> {
    fs::read_to_string(runtime_path(name, "pid").ok()?)
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn timeout_msg(e: io::Error) -> io::Error {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            io::Error::new(ErrorKind::TimedOut, "timed out waiting for a reply")
        }
        _ => e,
    }
}

/// Non-blocking flock. `Ok(false)` if someone else holds a conflicting lock.
fn try_lock(file: &File, op: c::c_int, path: &Path) -> Result<bool, SidebarError> {
    match uapi::flock(file.as_raw_fd(), op | c::LOCK_NB) {
        Ok(()) => Ok(true),
        Err(e) if e.0 == c::EWOULDBLOCK => Ok(false),
        Err(e) => Err(SidebarError::Pidfile(path.to_owned(), e.into())),
    }
}
