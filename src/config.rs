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
        anim::{Animation, Animations, Curve, Overshoot, Spring},
        sidebar::{Config, Edge, Size},
    },
    serde::{Deserialize, Deserializer, de::Error as _},
    std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        time::Duration,
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
    pub animations: Animations,
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
    #[serde(default)]
    animation: Option<AnimationArg>,
    #[serde(default = "default_true")]
    slide: bool,
    #[serde(default)]
    fade: bool,
    #[serde(default)]
    layer: Layer,
    #[serde(default)]
    keyboard_interactivity: KeyboardInteractivity,
    /// Take keyboard focus on show (by remapping the surface).
    #[serde(default)]
    focus: bool,
    #[serde(default)]
    hide_on_unfocus: bool,
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

/// `[name.animation]`: `spring`/`easing` for both directions, overridden by `show`/`hide`.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct AnimationArg {
    #[serde(default)]
    spring: Option<SpringArg>,
    #[serde(default)]
    easing: Option<EasingArg>,
    #[serde(default)]
    show: Option<AnimSpec>,
    #[serde(default)]
    hide: Option<AnimSpec>,
    #[serde(default)]
    overshoot: Overshoot,
}

/// Exactly one of `spring` or `easing`.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct AnimSpec {
    #[serde(default)]
    spring: Option<SpringArg>,
    #[serde(default)]
    easing: Option<EasingArg>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct SpringArg {
    #[serde(default)]
    damping_ratio: Option<f64>,
    #[serde(default)]
    stiffness: Option<f64>,
    #[serde(default)]
    epsilon: Option<f64>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct EasingArg {
    #[serde(default)]
    curve: Curve,
    #[serde(default = "default_duration")]
    duration: u64,
}

impl SpringArg {
    fn resolve(&self) -> Result<Spring, String> {
        let d = Spring::default();
        let s = Spring {
            damping_ratio: self.damping_ratio.unwrap_or(d.damping_ratio),
            stiffness: self.stiffness.unwrap_or(d.stiffness),
            epsilon: self.epsilon.unwrap_or(d.epsilon),
        };
        for (key, v) in [
            ("damping-ratio", s.damping_ratio),
            ("stiffness", s.stiffness),
            ("epsilon", s.epsilon),
        ] {
            if !(v.is_finite() && v > 0.0) {
                return Err(format!("spring {key} must be > 0"));
            }
        }
        Ok(s)
    }
}

impl EasingArg {
    fn resolve(&self) -> Animation {
        Animation::Easing {
            curve: self.curve,
            duration: Duration::from_millis(self.duration),
        }
    }
}

/// `None` if neither `spring` nor `easing` is given.
fn resolve_spec(
    spring: &Option<SpringArg>,
    easing: &Option<EasingArg>,
) -> Result<Option<Animation>, String> {
    match (spring, easing) {
        (Some(_), Some(_)) => Err("give either `spring` or `easing`, not both".into()),
        (Some(s), None) => s.resolve().map(Animation::Spring).map(Some),
        (None, Some(e)) => Ok(Some(e.resolve())),
        (None, None) => Ok(None),
    }
}

fn resolve_animations(arg: Option<&AnimationArg>) -> Result<Animations, String> {
    let Some(arg) = arg else {
        return Ok(Animations::default());
    };
    let base = resolve_spec(&arg.spring, &arg.easing)?.unwrap_or_default();
    let side = |spec: &Option<AnimSpec>, which: &str| -> Result<Animation, String> {
        match spec {
            None => Ok(base),
            Some(s) => resolve_spec(&s.spring, &s.easing)
                .map_err(|e| format!("{which}: {e}"))?
                .ok_or_else(|| format!("{which}: needs `spring` or `easing`")),
        }
    };
    Ok(Animations {
        show: side(&arg.show, "show")?,
        hide: side(&arg.hide, "hide")?,
        overshoot: arg.overshoot,
    })
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
    if entry.focus && !matches!(entry.keyboard_interactivity, KeyboardInteractivity::OnDemand) {
        return Err(SidebarError::FocusNeedsOnDemand(name));
    }
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
        focus: entry.focus,
        namespace: entry
            .namespace
            .unwrap_or_else(|| format!("wl-sidebar-{name}")),
        output: entry.output,
        hide_on_unfocus: entry.hide_on_unfocus,
        slide: entry.slide,
        fade: entry.fade,
    };
    let animations = resolve_animations(entry.animation.as_ref())
        .map_err(|e| SidebarError::InvalidAnimation(name.clone(), e))?;
    Ok(Sidebar {
        name,
        config,
        command,
        animations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<Animations, String> {
        let e: Entry =
            toml::from_str(&format!("command = \"x\"\n{toml}")).map_err(|e| e.to_string())?;
        resolve_animations(e.animation.as_ref())
    }

    #[test]
    fn default_is_spring() {
        assert_eq!(parse("").unwrap(), Animations::default());
    }

    #[test]
    fn show_hide_split() {
        let a = parse(
            r#"
            [animation]
            spring = { damping-ratio = 0.5 }
            overshoot = "clamp"
            hide.easing = { curve = "ease-in-cubic", duration = 120 }
            "#,
        )
        .unwrap();
        assert_eq!(
            a.show,
            Animation::Spring(Spring {
                damping_ratio: 0.5,
                ..Spring::default()
            })
        );
        assert_eq!(
            a.hide,
            Animation::Easing {
                curve: Curve::EaseInCubic,
                duration: Duration::from_millis(120),
            }
        );
        assert_eq!(a.overshoot, Overshoot::Clamp);
    }

    #[test]
    fn invalid() {
        assert!(parse("animation.spring = { stiffness = 0 }").is_err());
        assert!(parse("animation = { spring = {}, easing = {} }").is_err());
        assert!(parse("animation.show = {}").is_err());
        assert!(parse("animation.easing = { curve = \"nope\" }").is_err());
    }
}
