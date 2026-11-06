# wl-sidebar

Run any regular Wayland application as a slide-out sidebar: a `wlr-layer-shell`
surface anchored to one screen edge that reserves space (exclusive zone) while shown.

It is a Wayland _proxy_, not a compositor: it sits between the app and your compositor
and rewrites `xdg_toplevel` into `zwlr_layer_surface_v1`. Everything else (buffers,
input, popups, clipboard) is passed through untouched. Built on
[wl-proxy](https://github.com/mahkoh/wl-proxy); the translation code is adapted from its
`wl-paper` app (GPL-3.0).

## Usage

Define sidebars as named tables in `$XDG_CONFIG_HOME/wl-sidebar/config.toml` (default
`~/.config/wl-sidebar/config.toml`):

```toml
[term]
command = ["alacritty"]
edge = "left"
size = 400

[notes]
command = "obsidian --ozone-platform=wayland"   # a string is run with `sh -c`
edge = "right"
size = "30%"
focus = true
```

```sh
cargo build --release
wl-sidebar --toggle term   # starts `term` (shown) if not running, otherwise slides it out / in
```

If the config defines only one sidebar, the name can be omitted. `--config
PATH` uses another config file. `wl-sidebar --run [--hidden] NAME` runs a
sidebar in the foreground instead (useful for debugging or autostart).

| Key                      | Default               |                                                                                                                                                                                 |
| ------------------------ | --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `command`                | (required)            | program + args as an array, or a string run with `sh -c`                                                                                                                        |
| `edge`                   | `"left"`              | `left\|right\|top\|bottom`                                                                                                                                                      |
| `size`                   | `400`                 | width (height for top/bottom) in pixels, or a fraction of the output's usable area (`0.5`, `"50%"`; measured once at startup). Grows to the app's minimum size if that's larger |
| `output`                 |                       | pin to this output (e.g. `"DP-1"`)                                                                                                                                              |
| `reserve`                | `true`                | reserve space; `false` floats over windows                                                                                                                                      |
| `animation`              | spring                | slide animation, see [Animation](#animation)                                                                                                                                    |
| `slide`                  | `true`                | slide in/out; `false` keeps the sidebar in place (useful with `fade`)                                                                                                           |
| `fade`                   | `false`               | fade in/out; needs compositor support for `wp_alpha_modifier_v1`                                                                                                                |
| `layer`                  | `"top"`               | `background\|bottom\|top\|overlay`                                                                                                                                              |
| `keyboard-interactivity` | `"on-demand"`         | `none\|exclusive\|on-demand` while shown (always `none` while hidden)                                                                                                           |
| `focus`                  | `false`               | take keyboard focus on show (see [Focus](#focus))                                                                                                                               |
| `hide-on-unfocus`        | `false`               | hide when the sidebar loses keyboard focus (see [Focus](#focus))                                                                                                                |
| `namespace`              | `"wl-sidebar-<name>"` | layer-shell namespace                                                                                                                                                           |

niri keybinding example:

```kdl
binds {
    Mod+B { spawn "wl-sidebar" "--toggle" "term"; }
}
```

## Animation

By default the sidebar slides with a critically damped spring (snappy, no bounce).
Springs keep their momentum, so toggling mid-slide reverses smoothly.

```toml
[term.animation]
spring = { damping-ratio = 0.6, stiffness = 800 }      # both directions
hide.easing = { curve = "ease-in-cubic", duration = 120 }  # override one direction
overshoot = "clamp"
```

- `spring = { damping-ratio, stiffness, epsilon }`. Defaults are `1.0`, `1000` and
  `0.001`. `damping-ratio < 1` bounces, `1` settles as fast as possible without
  bouncing, and `> 1` is sluggish. Higher `stiffness` is faster. The spring stops
  once it's within `epsilon` of the target.
- `easing = { curve, duration }`. `duration` is in ms (default `150`). `curve` is
  one of `linear`, `ease-in-quad`, `ease-in-cubic`, `ease-out-quad`,
  `ease-out-cubic` (default), `ease-out-expo`, `ease-in-out-cubic`,
  `ease-out-back`, `ease-out-elastic` or `ease-out-bounce`.
- `show` / `hide` each take `{ spring = … }` or `{ easing = … }` and override the
  top-level animation for that direction.
- `overshoot`: the app keeps its size, so overshooting the shown position would
  open a gap at the screen edge. `bounce` (default) reflects off the edge like a
  wall, and `clamp` stops at it. Overshoot while hiding happens offscreen and is
  cut off.

With `fade = true`, the opacity follows the same animation: transparent when
hidden, opaque when shown. With `slide = false` as well, the sidebar only fades
and moves offscreen once it's fully transparent. The sidebar and all its
subsurfaces fade (popups and cursors don't), and the app can't use
`wp_alpha_modifier_v1` itself while fading is enabled.

## Focus

With `focus = true`, the sidebar takes keyboard focus when shown and gives it
back to the previous window when hidden.

On niri, you can click away from the sidebar while it's shown.
Toggling it then focuses it again instead of hiding it; toggle once more to
hide.

On other compositors, the sidebar keeps focus until it's hidden.

Without `focus`, click the sidebar to type in it.

With `hide-on-unfocus = true`, the sidebar hides as soon as it loses keyboard
focus, e.g. when you click another window. It only hides after it had focus, so
combine it with `focus = true` (or click the sidebar once after showing it).
On compositors where the sidebar keeps focus until hidden, it can still lose
focus to e.g. an exclusive launcher on a higher layer.

## Single-instance apps

The app must open its window from the process wl-sidebar starts, through the
proxy's `WAYLAND_DISPLAY`. Some apps don't do that by default:

- **Firefox**: if Firefox is already running, `firefox` just asks that instance to
  open a window. Run a separate instance with its own profile:
  `command = ["firefox", "--new-instance", "--profile", "/home/me/sidebar-firefox"]`
  (the snap package can't use hidden directories in `$HOME`).
- **Nautilus** (and other apps using libgxdp): on Wayland it asks the compositor
  for a private connection over D-Bus (`org.gnome.Mutter.ServiceChannel`, which
  niri also provides) and ignores `WAYLAND_DISPLAY`. Clear `XDG_SESSION_TYPE` to
  stop that: `command = "XDG_SESSION_TYPE= exec nautilus --new-window"`. It also
  must not already be running.
- **GTK apps** generally: if the app is a single-instance GApplication that's already
  running, the window opens in the existing instance. Ghostty, for example, needs
  `--gtk-single-instance=false`.

## How hiding works

Hiding animates a _negative margin_ on the anchored edge, so the app keeps its size and
never re-layouts. The exclusive zone is dropped once fully hidden. Negative-margin
handling is compositor-specific; if your compositor clamps it, disable the animation
with `animation.easing = { duration = 0 }`.
