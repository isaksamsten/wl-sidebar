//! The sidebar's program. The sidebar exits when the program does, with the same exit
//! code (or signal).
//!
//! The program runs in its own process group, and closing it (`--close`, or SIGINT,
//! SIGTERM, or SIGHUP sent to the sidebar) signals the whole group, so helpers started
//! by a wrapper script are closed too.

use {
    std::{
        io,
        mem::MaybeUninit,
        os::unix::process::{CommandExt, ExitStatusExt},
        process::{Child, Command, exit},
        sync::Mutex,
        thread,
        time::Duration,
    },
    uapi::c,
};

/// How long closing waits after the first signal before sending SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(3);

/// Signals that close the program when sent to the sidebar.
const CLOSE_SIGNALS: [c::c_int; 3] = [c::SIGINT, c::SIGTERM, c::SIGHUP];

/// State of the program. While running, the pid (= process group id) stays valid until
/// the state changes: it is set to `Exited` before the program is reaped, so a signal
/// never hits a reused pid.
enum State {
    NotStarted,
    Running(c::pid_t),
    Exited,
}

static STATE: Mutex<State> = Mutex::new(State::NotStarted);

fn close_sigset() -> c::sigset_t {
    let mut set = MaybeUninit::<c::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the set; the signals are valid.
    unsafe {
        c::sigemptyset(set.as_mut_ptr());
        for sig in CLOSE_SIGNALS {
            c::sigaddset(set.as_mut_ptr(), sig);
        }
        set.assume_init()
    }
}

/// Blocks [`CLOSE_SIGNALS`] in the calling thread, so that they are delivered to the
/// signal thread started by [`spawn`]. Must be called before any other thread is
/// spawned, since threads inherit the mask.
pub fn block_signals() -> io::Result<()> {
    let set = close_sigset();
    // SAFETY: `set` is initialized.
    match unsafe { c::pthread_sigmask(c::SIG_BLOCK, &set, std::ptr::null_mut()) } {
        0 => Ok(()),
        e => Err(io::Error::from_raw_os_error(e)),
    }
}

/// Spawns the program in its own process group and exits the process once it exits.
pub fn spawn(cmd: &mut Command) -> io::Result<()> {
    cmd.process_group(0);
    // The signal mask is inherited through exec; give the program the default one back.
    // SAFETY: pthread_sigmask is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            let set = close_sigset();
            match c::pthread_sigmask(c::SIG_UNBLOCK, &set, std::ptr::null_mut()) {
                0 => Ok(()),
                e => Err(io::Error::from_raw_os_error(e)),
            }
        });
    }
    let child = cmd.spawn()?;
    let pid = child.id() as c::pid_t;
    *STATE.lock().unwrap() = State::Running(pid);
    let res = thread::Builder::new()
        .name("child".into())
        .spawn(move || forward_exit(pid, child))
        .and_then(|_| {
            thread::Builder::new()
                .name("signals".into())
                .spawn(handle_signals)
        });
    if let Err(e) = res {
        let _ = uapi::kill(-pid, c::SIGKILL);
        return Err(e);
    }
    Ok(())
}

/// Closes the program when the sidebar receives one of [`CLOSE_SIGNALS`].
fn handle_signals() {
    let set = close_sigset();
    loop {
        let mut sig = 0;
        // SAFETY: `set` is initialized and `sig` is a valid out pointer.
        match unsafe { c::sigwait(&set, &mut sig) } {
            0 => {
                log::info!("received signal {sig}");
                if let Err(e) = close(sig) {
                    log::error!("{e}");
                }
            }
            c::EINTR => {}
            e => {
                log::error!("sigwait: {}", io::Error::from_raw_os_error(e));
                return;
            }
        }
    }
}

fn forward_exit(pid: c::pid_t, mut child: Child) -> ! {
    // Wait for the exit without reaping, so the pid stays valid until `STATE` changes.
    loop {
        let mut info: c::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid siginfo_t.
        let res = unsafe {
            c::waitid(
                c::P_PID,
                pid as c::id_t,
                &mut info,
                c::WEXITED | c::WNOWAIT,
            )
        };
        if res == 0 || io::Error::last_os_error().raw_os_error() != Some(c::EINTR) {
            break;
        }
    }
    *STATE.lock().unwrap() = State::Exited;
    match child.wait() {
        Ok(status) => {
            log::info!("program exited: {status}");
            if let Some(code) = status.code() {
                exit(code);
            }
            if let Some(signal) = status.signal() {
                // raise() targets this thread, so unblocking the close signals here is
                // enough for them to take effect.
                let set = close_sigset();
                // SAFETY: `set` is initialized.
                unsafe { c::pthread_sigmask(c::SIG_UNBLOCK, &set, std::ptr::null_mut()) };
                let _ = uapi::raise(signal);
            }
            exit(1);
        }
        Err(e) => {
            log::error!("could not wait for the program: {e}");
            exit(1);
        }
    }
}

/// Sends `sig` to the program's process group, and SIGKILL if the program is still
/// running after [`KILL_GRACE`]. The sidebar then exits with it.
pub fn close(sig: c::c_int) -> Result<&'static str, String> {
    // Hold the lock while signaling so the program can't be reaped in between.
    let state = STATE.lock().unwrap();
    let pid = match *state {
        State::Running(pid) => pid,
        State::NotStarted => return Err("the program has not been started yet".into()),
        State::Exited => return Ok("closing"),
    };
    log::info!("closing: sending signal {sig} to process group {pid}");
    uapi::kill(-pid, sig)
        .map_err(|e| format!("could not signal process group {pid}: {}", io::Error::from(e)))?;
    drop(state);
    thread::Builder::new()
        .name("close".into())
        .spawn(|| {
            thread::sleep(KILL_GRACE);
            if let State::Running(pid) = *STATE.lock().unwrap() {
                log::warn!("pid {pid} still running, sending SIGKILL to its process group");
                let _ = uapi::kill(-pid, c::SIGKILL);
            }
        })
        .map_err(|e| format!("could not spawn a thread: {e}"))?;
    Ok("closing")
}

