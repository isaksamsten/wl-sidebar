use {
    crate::{SidebarError, config, sidebar},
    clap::{ArgGroup, Parser, ValueHint},
    std::{
        os::unix::process::CommandExt,
        path::PathBuf,
        process::{Command, Stdio},
    },
};

/// Run a regular Wayland application as a slide-out sidebar (a layer-shell surface
/// anchored to one screen edge that reserves space while shown).
///
/// Sidebars are defined as named tables in the config file
/// (default: `$XDG_CONFIG_HOME/wl-sidebar/config.toml`).
#[derive(Parser, Debug)]
#[command(version, group(ArgGroup::new("mode").required(true)))]
struct Args {
    /// Toggle the sidebar; start it (shown) if it isn't running.
    #[arg(long, group = "mode")]
    toggle: bool,
    /// Run the sidebar in the foreground.
    #[arg(long, group = "mode")]
    run: bool,
    /// With --run: start hidden.
    #[arg(long, requires = "run")]
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
        return sidebar::run(&sb.name, sb.config, &sb.command, args.hidden, sb.duration);
    }
    if sidebar::send_toggle(&sb.name)? {
        return Ok(());
    }
    log::info!("starting sidebar {}", sb.name);
    let exe = std::env::current_exe().map_err(SidebarError::SpawnDaemon)?;
    let mut cmd = Command::new(exe);
    cmd.arg("--run")
        .arg("--config")
        .arg(&path)
        .arg("--")
        .arg(&sb.name)
        .stdin(Stdio::null());
    // Detach from our session so that closing the terminal doesn't kill the sidebar.
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| uapi::setsid().map(drop).map_err(Into::into));
    }
    cmd.spawn().map_err(SidebarError::SpawnDaemon)?;
    Ok(())
}

