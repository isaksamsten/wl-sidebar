mod anim;
mod cli;
mod config;
mod fade;
mod proxy;
mod remap;
mod sidebar;

use {
    error_reporter::Report,
    log::LevelFilter,
    std::{io, path::PathBuf},
    thiserror::Error,
    wl_proxy::{acceptor::AcceptorError, state::StateError},
};

#[derive(Debug, Error)]
enum SidebarError {
    #[error("could not create the wayland socket")]
    CreateAcceptor(#[source] AcceptorError),
    #[error("could not accept a connection")]
    Accept(#[source] AcceptorError),
    #[error("could not spawn child")]
    SpawnChild(#[source] io::Error),
    #[error("could not spawn a thread")]
    SpawnThread(#[source] io::Error),
    #[error("could not set up signal handling")]
    Signals(#[source] io::Error),
    #[error("could not create an eventfd")]
    Eventfd(#[source] io::Error),
    #[error("could not create the proxy state")]
    CreateState(#[source] StateError),
    #[error("could not dispatch the proxy state")]
    Dispatch(#[source] StateError),
    #[error("poll failed")]
    Poll(#[source] io::Error),
    #[error("XDG_RUNTIME_DIR is not set")]
    NoRuntimeDir,
    #[error("neither XDG_CONFIG_HOME nor HOME is set")]
    NoConfigDir,
    #[error("could not read config file {}", .0.display())]
    ReadConfig(PathBuf, #[source] io::Error),
    #[error("could not parse config file {}", .0.display())]
    ParseConfig(PathBuf, #[source] toml::de::Error),
    #[error("no sidebar named `{}` in {}", .0, .1.display())]
    UnknownSidebar(String, PathBuf),
    #[error("{} does not define exactly one sidebar; give a name", .0.display())]
    NameRequired(PathBuf),
    #[error("invalid sidebar name `{0}`")]
    InvalidName(String),
    #[error("sidebar `{0}` has an empty command")]
    EmptyCommand(String),
    #[error("sidebar `{0}`: invalid animation: {1}")]
    InvalidAnimation(String, String),
    #[error("sidebar `{0}` is already running")]
    AlreadyRunning(String),
    #[error("could not access pid file {}", .0.display())]
    Pidfile(PathBuf, #[source] io::Error),
    #[error("could not read the pid of the running instance from {}", .0.display())]
    BadPidfile(PathBuf),
    #[error("could not start the sidebar")]
    SpawnDaemon(#[source] io::Error),
    #[error("could not signal the running instance")]
    Kill(#[source] io::Error),
}

fn main() -> Result<(), Report<SidebarError>> {
    env_logger::builder()
        .filter_level(LevelFilter::Info)
        .parse_default_env()
        .init();
    cli::main().map_err(Report::new)
}
