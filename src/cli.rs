use {
    crate::{SidebarError, child, config, ipc, sidebar},
    clap::{ArgGroup, Parser, ValueHint},
    error_reporter::Report,
    std::{
        fs::{self, File},
        io::ErrorKind,
        os::unix::process::CommandExt,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::Duration,
    },
};

/// Run a regular Wayland application as a slide-out sidebar (a layer-shell surface
/// anchored to one screen edge that reserves space while shown).
///
/// Sidebars are defined as named tables in the config file
/// (default: `$XDG_CONFIG_HOME/wl-sidebar/config.toml`).
#[derive(Parser, Debug)]
#[command(
    version,
    group(ArgGroup::new("mode").required(true)),
    group(ArgGroup::new("start_or_run").args(["start", "run"]))
)]
struct Args {
    /// Toggle the sidebar; start it (shown) in the background if it isn't running.
    #[arg(long, group = "mode")]
    toggle: bool,
    /// Start the sidebar in the background. Does nothing if it is already running.
    #[arg(long, group = "mode")]
    start: bool,
    /// Close the running sidebar: ask its program to exit (SIGTERM), and kill it
    /// (SIGKILL) if it hasn't exited after a few seconds.
    #[arg(long, group = "mode")]
    close: bool,
    /// Run the sidebar in the foreground (for debugging or service managers such as
    /// systemd). SIGINT, SIGTERM and SIGHUP close it like --close.
    #[arg(long, group = "mode")]
    run: bool,
    /// With --start or --run: start hidden.
    #[arg(long, requires = "start_or_run")]
    hidden: bool,
    /// Config file to use.
    #[arg(long, value_hint = ValueHint::FilePath)]
    config: Option<PathBuf>,
    /// Name of the sidebar in the config file. May be omitted if it defines only one.
    name: Option<String>,
}

pub fn main() -> Result<(), SidebarError> {
    let args = Args::parse();
    let path = match args.config {
        Some(p) => p,
        None => config::default_path()?,
    };
    let sb = config::load(&path, args.name.as_deref())?;
    if args.run {
        return sidebar::run(&sb.name, sb.config, &sb.command, args.hidden, sb.animations);
    }
    if args.close {
        return close(&sb.name);
    }
    if args.start {
        if ipc::running(&sb.name)? {
            log::info!("sidebar {} is already running", sb.name);
            return Ok(());
        }
        return spawn_daemon(&path, &sb.name, args.hidden);
    }
    if let Some(state) = ipc::send(&sb.name, "toggle")? {
        log::debug!("sidebar {}: {state}", sb.name);
        return Ok(());
    }
    spawn_daemon(&path, &sb.name, false)
}

/// Starts sidebar `name` from config file `path` as a detached `--run` process.
fn spawn_daemon(path: &Path, name: &str, hidden: bool) -> Result<(), SidebarError> {
    log::info!("starting sidebar {name}");
    let exe = std::env::current_exe().map_err(SidebarError::SpawnDaemon)?;
    let mut cmd = Command::new(exe);
    cmd.arg("--run");
    if hidden {
        cmd.arg("--hidden");
    }
    cmd.arg("--config")
        .arg(path)
        .arg("--")
        .arg(name)
        .stdin(Stdio::null());
    // The daemon outlives us and our stderr, so keep its output (including panics).
    match open_log(name) {
        Ok((path, out, err)) => {
            log::info!("logging to {}", path.display());
            cmd.stdout(out).stderr(err);
        }
        Err(e) => log::warn!("{}", Report::new(e)),
    }
    // Detach from our session so that closing the terminal doesn't kill the sidebar.
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| uapi::setsid().map(drop).map_err(Into::into));
    }
    cmd.spawn().map_err(SidebarError::SpawnDaemon)?;
    Ok(())
}

/// Closes the running sidebar `name` and waits for it to exit, so that a following
/// `--toggle` starts a fresh instance.
fn close(name: &str) -> Result<(), SidebarError> {
    if ipc::send(name, "close")?.is_none() {
        log::info!("sidebar {name} is not running");
        return Ok(());
    }
    ipc::wait_exit(name, child::KILL_GRACE + Duration::from_secs(2))?;
    log::debug!("sidebar {name} closed");
    Ok(())
}

/// Opens `$XDG_STATE_HOME/wl-sidebar/<name>.log` for a new daemon, keeping the previous
/// log (which may explain why the previous instance died) as `<name>.log.old`.
fn open_log(name: &str) -> Result<(PathBuf, File, File), SidebarError> {
    let dir = match std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h).join(".local/state"),
            None => PathBuf::from("."),
        },
    }
    .join("wl-sidebar");
    let path = dir.join(format!("{name}.log"));
    let err = |e| SidebarError::LogFile(path.clone(), e);
    fs::create_dir_all(&dir).map_err(err)?;
    match fs::rename(&path, dir.join(format!("{name}.log.old"))) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(err(e)),
    }
    let out = File::create(&path).map_err(err)?;
    let err = out.try_clone().map_err(err)?;
    Ok((path, out, err))
}

