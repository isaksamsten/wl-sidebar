# wl-sidebar

https://github.com/user-attachments/assets/c24dd00e-fd44-476a-9605-4c92b695e869

Run a Wayland application as a slide-out sidebar, anchored to a screen edge with
space reserved while shown.

wl-sidebar proxies the app's Wayland connection, translating `xdg_toplevel` into
`zwlr_layer_surface_v1`. Other traffic (buffers, input, popups, clipboard) passes
through unchanged. Built on [wl-proxy](https://github.com/mahkoh/wl-proxy), with
translation code adapted from its `wl-paper` app (GPL-3.0).

## Installation

```sh
cargo install --git https://github.com/isaksamsten/wl-sidebar
```

Requires Rust 1.89 or newer and a compositor that supports `zwlr_layer_shell_v1`,
such as umbriel, niri, sway, Hyprland, river or KDE Plasma. GNOME doesn't support
it. Fading also needs `wp_alpha_modifier_v1`; without it, `fade` has no effect.
wl-sidebar is developed and tested on umbriel and niri.

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
wl-sidebar --toggle term          # start and show, or toggle if already running
wl-sidebar --start --hidden term  # start in the background (no-op if running)
wl-sidebar --close term           # quit the program (SIGTERM, then SIGKILL after 3 s)
wl-sidebar --list                 # configured sidebars and their state
```

Sidebars run in the background, so `--close` is how to quit one whose program has no
way to. It signals the program's whole process group, so helpers started by a
wrapper script exit too. The name is optional if only one sidebar is configured.
Use `--config PATH` for another config file.

`wl-sidebar --run [--hidden] NAME` runs in the foreground instead, for debugging or
for a service manager such as systemd. SIGINT (Ctrl-C), SIGTERM, and SIGHUP close it
like `--close`.

| Key                      | Default               | Description                                                                                                                                               |
| ------------------------ | --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `command`                | required              | Program and args as an array, or a string run with `sh -c`                                                                                                |
| `edge`                   | `"left"`              | `left\|right\|top\|bottom`                                                                                                                                |
| `size`                   | `400`                 | Width (height for top/bottom) in pixels, or a fraction of the output's usable area (`0.5`, `"50%"`), measured at startup. At least the app's minimum size |
| `output`                 |                       | Output name, e.g. `"DP-1"`                                                                                                                                |
| `reserve`                | `true`                | Reserve space; `false` floats over windows                                                                                                                |
| `animation`              | spring                | See [Animation](#animation)                                                                                                                               |
| `slide`                  | `true`                | Slide in/out; `false` keeps the sidebar in place during fades                                                                                             |
| `fade`                   | `false`               | Fade in/out; requires `wp_alpha_modifier_v1`                                                                                                              |
| `layer`                  | `"top"`               | `background\|bottom\|top\|overlay`                                                                                                                        |
| `keyboard-interactivity` | `"on-demand"`         | `none\|exclusive\|on-demand` while shown; always `none` while hidden. See [Focus](#focus)                                                                 |
| `focus`                  | `false`               | Take keyboard focus on show (requires `on-demand`); see [Focus](#focus)                                                                                   |
| `hide-on-unfocus`        | `false`               | Hide on keyboard focus loss; see [Focus](#focus)                                                                                                          |
| `namespace`              | `"wl-sidebar-<name>"` | Layer-shell namespace                                                                                                                                     |

Keybinding examples:

```toml
# umbriel
[keybinds]
"Mod+B" = "spawn wl-sidebar --toggle term"
```

```kdl
// niri
binds {
    Mod+B { spawn "wl-sidebar" "--toggle" "term"; }
}
```

## Animation

The default is a critically damped spring. Toggling mid-slide preserves momentum.

```toml
[term.animation]
spring = { damping-ratio = 0.6, stiffness = 800 }      # both directions
hide.easing = { curve = "ease-in-cubic", duration = 120 }  # override one direction
overshoot = "clamp"
```

- `spring = { damping-ratio, stiffness, epsilon }`. Defaults are `1.0`, `1000` and
  `0.001`. A damping ratio below `1` bounces; `1` is critically damped; above `1`
  is overdamped. Higher `stiffness` is faster. The spring stops within `epsilon`
  of the target.
- `easing = { curve, duration }`. `duration` is in ms (default `150`). `curve` is
  one of `linear`, `ease-in-quad`, `ease-in-cubic`, `ease-out-quad`,
  `ease-out-cubic` (default), `ease-out-expo`, `ease-in-out-cubic`,
  `ease-out-back`, `ease-out-elastic` or `ease-out-bounce`.
- `show` / `hide` each take `{ spring = … }` or `{ easing = … }` and override the
  top-level animation for that direction.
- `overshoot`: `bounce` (default) reflects at the screen edge; `clamp` stops there.
  This prevents gaps between the app and the edge. Hiding overshoot is cut off
  offscreen.

With `fade = true`, opacity follows the animation. Add `slide = false` to fade
without sliding; the sidebar moves offscreen once transparent. Subsurfaces fade
too, but popups and cursors don't. The app cannot use `wp_alpha_modifier_v1`
itself while fading is enabled.

## Focus

`keyboard-interactivity` sets how the sidebar gets keyboard focus while shown:

- `"on-demand"` (default): click the sidebar to type. Clicking another window
  moves focus away again.
- `"exclusive"`: the sidebar takes focus when shown and keeps it until hidden.
  Only an exclusive surface on a higher layer, such as a launcher, can take it.
- `"none"`: the sidebar never gets keyboard focus.

When hidden, focus returns to the previous window.

`focus = true` (with `"on-demand"`) also takes focus when shown. Clicking another
window still moves focus away. The next toggle then refocuses the sidebar, and
another hides it. If the sidebar doesn't get focus on your compositor, use
`"exclusive"` instead. With `focus = true`, the app cannot use explicit sync
(`linux-drm-syncobj`).

`hide-on-unfocus = true` hides the sidebar when it loses keyboard focus. It must
have received focus first: use `focus = true`, `"exclusive"`, or click it after
showing.

## Single-instance apps

The app must open its window through the proxy's `WAYLAND_DISPLAY`:

- **Firefox**: an existing instance opens new windows outside the proxy. Use a
  separate instance and profile:
  `command = ["firefox", "--new-instance", "--profile", "/home/me/sidebar-firefox"]`
  (the snap package can't use hidden directories in `$HOME`).
- **Nautilus** (and other libgxdp apps): bypasses `WAYLAND_DISPLAY` via
  `org.gnome.Mutter.ServiceChannel` on D-Bus, also provided by niri. Clear
  `XDG_SESSION_TYPE` to prevent this:
  `command = "XDG_SESSION_TYPE= exec nautilus --new-window"`.
  Nautilus must not already be running.
- **GTK single-instance apps**: windows open in the existing process if one is
  running. For Ghostty, use `--gtk-single-instance=false`.

## Flatpak

Flatpak apps work: `command = ["flatpak", "run", "--socket=wayland", "org.example.App"]`.
The app must use Wayland, not X11. For Electron apps, add `--ozone-platform=wayland`
after the app ID. Single-instance rules above apply: `flatpak run` on an app that's
already running usually opens the window in the existing instance.

**The sandbox doesn't restrict Wayland access.** Normally Flatpak uses
`wp_security_context_v1` so the compositor knows the connection is sandboxed and
can hide privileged protocols (screen capture, virtual input, layer-shell, …). Its
connection would then go to the compositor directly and skip wl-sidebar, so
wl-sidebar hides that protocol. The app then connects through wl-sidebar, and the
compositor treats it like any unsandboxed program. Only run Flatpak apps you
trust this way.

## How hiding works

Hiding animates a negative margin on the anchored edge without resizing the app.
The exclusive zone is dropped once fully hidden. If your compositor clamps
negative margins, disable animation with `animation.easing = { duration = 0 }`.
