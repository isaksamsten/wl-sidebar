//! The config file: one `[name]` table per sidebar.
//!
//! ```toml
//! [term]
//! command = ["alacritty"]
//! edge = "left"
//! size = 400
//! ```

use {
    crate::{
        SidebarError,
        sidebar::{Config, Edge, Focus, Size},
    },
    serde::{Deserialize, Deserializer, de::Error as _},
    std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
    },
    wl_proxy::protocols::wlr_layer_shell_unstable_v1::{
        zwlr_layer_shell_v1::ZwlrLayerShellV1Layer,
        zwlr_layer_surface_v1::ZwlrLayerSurfaceV1KeyboardInteractivity,
    },
};

/// A sidebar selected from the config file.
pub struct Sidebar {
    pub name: String,
    pub config: Config,
    pub command: Vec<String>,
    pub duration: u64,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct Entry {
    /// `["prog", "arg"]`, or a string that is run with `sh -c`.
    command: CommandRepr,
    #[serde(default)]
    edge: EdgeArg,
    #[serde(default = "default_size", deserialize_with = "de_size")]
    size: Size,
    #[serde(default)]
    output: Option<String>,
    #[serde(default = "default_true")]
    reserve: bool,
    #[serde(default = "default_duration")]
    duration: u64,
    #[serde(default)]
    layer: Layer,
    #[serde(default)]
    keyboard_interactivity: KeyboardInteractivity,
    #[serde(default)]
    focus: FocusArg,
    /// Default: `wl-sidebar-<name>`.
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum CommandRepr {
    Argv(Vec<String>),
    Shell(String),
}

#[derive(Deserialize, Debug, Default, Copy, Clone)]
#[serde(rename_all = "kebab-case")]
enum EdgeArg {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Deserialize, Debug, Default, Copy, Clone)]
#[serde(rename_all = "kebab-case")]
enum KeyboardInteractivity {
    None,
    Exclusive,
    #[default]
    OnDemand,
}

/// `false`, `true` (picked by [`auto_focus`]), `"exclusive"` or `"remap"`.
#[derive(Deserialize, Debug, Copy, Clone)]
#[serde(untagged)]
enum FocusArg {
    Bool(bool),
    Mode(FocusMode),
}

impl Default for FocusArg {
    fn default() -> Self {
        Self::Bool(false)
    }
}

#[derive(Deserialize, Debug, Copy, Clone)]
#[serde(rename_all = "kebab-case")]
enum FocusMode {
    Exclusive,
    Remap,
}

/// The focus strategy for `focus = true`.
///
/// `Remap` needs a compositor that focuses `on-demand` layer surfaces when they get
/// mapped. That's not detectable over Wayland, so it's chosen for compositors known to do
/// it (niri). Everything else gets `Exclusive`, which works everywhere.
fn auto_focus(name: &str) -> Focus {
    let niri = std::env::var_os("NIRI_SOCKET").is_some();
    let focus = if niri { Focus::Remap } else { Focus::Exclusive };
    log::debug!("sidebar `{name}`: focus = true -> {focus:?} (niri: {niri})");
    focus
}

#[derive(Deserialize, Debug, Default, Copy, Clone)]
#[serde(rename_all = "kebab-case")]
enum Layer {
    Background,
    Bottom,
    #[default]
    Top,
    Overlay,
}

fn default_true() -> bool {
    true
}

fn default_duration() -> u64 {
    150
}

fn default_size() -> Size {
    Size::Pixels(400)
}

/// `400` = pixels, `0.5` = fraction, or a string as accepted by [`Size::from_str`].
fn de_size<'de, D: Deserializer<'de>>(d: D) -> Result<Size, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Int(i64),
        Float(f64),
        Str(String),
    }
    let s = match Repr::deserialize(d)? {
        Repr::Int(i) => i.to_string(),
        // Keep a `.` so that e.g. `1.0` is a fraction, not one pixel.
        Repr::Float(f) => format!("{f:?}"),
        Repr::Str(s) => s,
    };
    s.parse().map_err(D::Error::custom)
}

pub fn default_path() -> Result<PathBuf, SidebarError> {
    let dir = match std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").ok_or(SidebarError::NoConfigDir)?)
            .join(".config"),
    };
    Ok(dir.join("wl-sidebar").join("config.toml"))
}

/// Loads the sidebar `name` from the config file at `path`. Without a name, the file
/// must define exactly one sidebar.
pub fn load(path: &Path, name: Option<&str>) -> Result<Sidebar, SidebarError> {
    let text =
        fs::read_to_string(path).map_err(|e| SidebarError::ReadConfig(path.to_owned(), e))?;
    let mut entries: BTreeMap<String, Entry> =
        toml::from_str(&text).map_err(|e| SidebarError::ParseConfig(path.to_owned(), e))?;
    let (name, entry) = match name {
        Some(name) => entries
            .remove_entry(name)
            .ok_or_else(|| SidebarError::UnknownSidebar(name.to_owned(), path.to_owned()))?,
        None if entries.len() == 1 => entries.pop_first().unwrap(),
        None => return Err(SidebarError::NameRequired(path.to_owned())),
    };
    if name.is_empty() || name.contains('/') {
        return Err(SidebarError::InvalidName(name));
    }
    let command = match entry.command {
        CommandRepr::Argv(v) => v,
        CommandRepr::Shell(s) => vec!["sh".into(), "-c".into(), s],
    };
    if command.is_empty() {
        return Err(SidebarError::EmptyCommand(name));
    }
    let focus = match entry.focus {
        FocusArg::Bool(false) => Focus::Off,
        FocusArg::Bool(true) => auto_focus(&name),
        FocusArg::Mode(FocusMode::Exclusive) => Focus::Exclusive,
        FocusArg::Mode(FocusMode::Remap) => Focus::Remap,
    };
    let config = Config {
        edge: match entry.edge {
            EdgeArg::Left => Edge::Left,
            EdgeArg::Right => Edge::Right,
            EdgeArg::Top => Edge::Top,
            EdgeArg::Bottom => Edge::Bottom,
        },
        size: entry.size,
        reserve: entry.reserve,
        keyboard_interactivity: match entry.keyboard_interactivity {
            KeyboardInteractivity::None => ZwlrLayerSurfaceV1KeyboardInteractivity::NONE,
            KeyboardInteractivity::Exclusive => ZwlrLayerSurfaceV1KeyboardInteractivity::EXCLUSIVE,
            KeyboardInteractivity::OnDemand => ZwlrLayerSurfaceV1KeyboardInteractivity::ON_DEMAND,
        },
        layer: match entry.layer {
            Layer::Background => ZwlrLayerShellV1Layer::BACKGROUND,
            Layer::Bottom => ZwlrLayerShellV1Layer::BOTTOM,
            Layer::Top => ZwlrLayerShellV1Layer::TOP,
            Layer::Overlay => ZwlrLayerShellV1Layer::OVERLAY,
        },
        focus,
        namespace: entry
            .namespace
            .unwrap_or_else(|| format!("wl-sidebar-{name}")),
        output: entry.output,
    };
    Ok(Sidebar {
        name,
        config,
        command,
        duration: entry.duration,
    })
}
