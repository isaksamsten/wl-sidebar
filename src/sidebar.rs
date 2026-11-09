//! Sidebar placement, slide animation, and the proxy event loop.
//!
//! Threading model (same as wl-proxy's `SimpleProxy`): every client connection gets its
//! own thread with its own `State` (= its own connection to the real compositor). The
//! slide state is global; when it changes, every client thread is woken through an
//! eventfd and re-applies margin/exclusive-zone to the layer surfaces it owns.

use {
    crate::{
        SidebarError,
        anim::Animations,
        child, fade, ipc,
        proxy::{self, ClientWlDisplay},
        remap::{Phase, Remap},
    },
    error_reporter::Report,
    std::{
        cell::{Cell, RefCell},
        os::fd::{AsRawFd, OwnedFd},
        process::Command,
        rc::{Rc, Weak},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed},
        },
        thread,
        time::{Duration, Instant},
    },
    uapi::c,
    wl_proxy::{
        acceptor::Acceptor,
        client::ClientHandler,
        object::ObjectCoreApi,
        protocols::{
            wayland::wl_surface::WlSurface,
            wlr_layer_shell_unstable_v1::{
                zwlr_layer_shell_v1::ZwlrLayerShellV1Layer,
                zwlr_layer_surface_v1::{
                    ZwlrLayerSurfaceV1, ZwlrLayerSurfaceV1Anchor,
                    ZwlrLayerSurfaceV1KeyboardInteractivity,
                },
            },
        },
        simple::SimpleCommandExt,
        state::{Destructor, State},
    },
};

/// Poll timeout while animating (~120 Hz).
const FRAME_MS: c::c_int = 8;
/// With `hide_on_unfocus`, how long the sidebar may be without keyboard focus before it
/// hides. Focus moving between the client's own surfaces (e.g. to a popup) arrives as a
/// leave followed by an enter; this keeps that from hiding the sidebar.
const UNFOCUS_GRACE: Duration = Duration::from_millis(100);
/// `focus`: how long to wait for keyboard focus with `EXCLUSIVE` interactivity before
/// falling back to remapping.
const GRAB_TIMEOUT: Duration = Duration::from_millis(300);
/// `focus`: how long focus must survive the switch back to `ON_DEMAND`. Some compositors
/// (niri) drop focus when interactivity changes from `EXCLUSIVE` to `ON_DEMAND`.
const SETTLE: Duration = Duration::from_millis(150);

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn anchor(self) -> ZwlrLayerSurfaceV1Anchor {
        use ZwlrLayerSurfaceV1Anchor as A;
        match self {
            Edge::Left => A::TOP | A::BOTTOM | A::LEFT,
            Edge::Right => A::TOP | A::BOTTOM | A::RIGHT,
            Edge::Top => A::LEFT | A::RIGHT | A::TOP,
            Edge::Bottom => A::LEFT | A::RIGHT | A::BOTTOM,
        }
    }

    /// Layer surface size; 0 on the axis that is stretched between two anchors.
    fn size(self, size: u32) -> (u32, u32) {
        match self {
            Edge::Left | Edge::Right => (size, 0),
            Edge::Top | Edge::Bottom => (0, size),
        }
    }

    /// `[top, right, bottom, left]` with `m` applied to the anchored edge.
    fn margins(self, m: i32) -> [i32; 4] {
        match self {
            Edge::Top => [m, 0, 0, 0],
            Edge::Right => [0, m, 0, 0],
            Edge::Bottom => [0, 0, m, 0],
            Edge::Left => [0, 0, 0, m],
        }
    }
}

/// Sidebar thickness: width for left/right, height for top/bottom.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Size {
    Pixels(u32),
    /// Fraction (0, 1] of the available output area, measured from the compositor's
    /// first configure.
    Fraction(f64),
}

impl std::str::FromStr for Size {
    type Err = String;

    /// `400` = pixels, `0.5` or `50%` = fraction of the output.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let frac = if let Some(p) = s.strip_suffix('%') {
            p.trim().parse::<f64>().map_err(|e| e.to_string())? / 100.0
        } else if s.contains('.') {
            s.parse::<f64>().map_err(|e| e.to_string())?
        } else {
            let px: u32 = s
                .parse()
                .map_err(|e: std::num::ParseIntError| e.to_string())?;
            if px == 0 {
                return Err("size must be > 0".into());
            }
            return Ok(Size::Pixels(px));
        };
        if !(frac > 0.0 && frac <= 1.0) {
            return Err("fraction must be in (0, 1] / (0%, 100%]".into());
        }
        Ok(Size::Fraction(frac))
    }
}

pub struct Config {
    pub edge: Edge,
    pub size: Size,
    /// Whether to reserve space (exclusive zone) while shown.
    pub reserve: bool,
    pub layer: ZwlrLayerShellV1Layer,
    /// Interactivity while shown. Always `NONE` while hidden.
    pub keyboard_interactivity: ZwlrLayerSurfaceV1KeyboardInteractivity,
    /// Take keyboard focus on show by unmapping the surface while hidden and mapping it
    /// again on show (see `remap`). Requires `ON_DEMAND` interactivity.
    pub focus: bool,
    pub namespace: String,
    /// Connector name of the output to pin the sidebar to.
    pub output: Option<String>,
    /// Hide when the sidebar loses keyboard focus.
    pub hide_on_unfocus: bool,
    /// Slide in and out. Otherwise the sidebar only moves offscreen once fully hidden.
    pub slide: bool,
    /// Fade in and out (`wp_alpha_modifier_v1`).
    pub fade: bool,
}

// ---------------------------------------------------------------------------------------
// Global slide state
// ---------------------------------------------------------------------------------------

struct Slide {
    visible: bool,
    /// Position at `start`. 1.0 = fully shown, 0.0 = fully hidden.
    from: f64,
    /// Velocity at `start`, carried over when toggling mid-animation.
    velocity: f64,
    start: Instant,
    anim: Animations,
}

static SLIDE: Mutex<Option<Slide>> = Mutex::new(None);
/// Eventfds of all client threads, keyed by client id.
static WAKERS: Mutex<Vec<(usize, Arc<uapi::OwnedFd>)>> = Mutex::new(Vec::new());
/// `focus`: toggling a shown but unfocused sidebar focuses it instead of hiding.
static REFOCUS_ENABLED: AtomicBool = AtomicBool::new(false);
/// Bumped to ask all client threads to remap their (shown) sidebars to take focus.
static REFOCUS: AtomicU64 = AtomicU64::new(0);
/// A refocus was requested and the sidebar hasn't gained keyboard focus since. The next
/// toggle then hides instead of trying again, so toggling can never get stuck on
/// "focused" when the compositor doesn't hand out focus (or the remap can't happen).
static REFOCUS_PENDING: AtomicBool = AtomicBool::new(false);
/// Number of client keyboards whose focus is on one of the client's surfaces.
static FOCUSED: AtomicUsize = AtomicUsize::new(0);
/// `hide_on_unfocus`: hide the sidebar when [`FOCUSED`] drops to 0.
static HIDE_ON_UNFOCUS: AtomicBool = AtomicBool::new(false);
/// `focus`: grabbing focus with `EXCLUSIVE` didn't work on this compositor; take focus
/// by remapping instead (see `remap`).
static REMAP_FALLBACK: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set when this thread saw [`FOCUSED`] drop to 0 with `hide_on_unfocus`. If focus
    /// is still gone at this time, the sidebar hides.
    static UNFOCUS_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Keyboard focus of one client `wl_keyboard`, counted in [`FOCUSED`]. Every surface of
/// the client belongs to the sidebar (toplevels become the sidebar, the rest are popups
/// and subsurfaces of it), so focus on any of them counts.
#[derive(Default)]
pub struct KeyboardFocus(bool);

impl KeyboardFocus {
    pub fn set(&mut self, focused: bool) {
        if self.0 == focused {
            return;
        }
        self.0 = focused;
        if focused {
            FOCUSED.fetch_add(1, Relaxed);
            REFOCUS_PENDING.store(false, Relaxed);
        } else if FOCUSED.fetch_sub(1, Relaxed) == 1 && HIDE_ON_UNFOCUS.load(Relaxed) {
            UNFOCUS_DEADLINE.set(Some(Instant::now() + UNFOCUS_GRACE));
        }
        log::debug!(
            "keyboard focus {}",
            if focused { "entered" } else { "left" }
        );
    }
}

impl Drop for KeyboardFocus {
    fn drop(&mut self) {
        self.set(false);
    }
}

impl Slide {
    /// Returns `(position, velocity, still_animating)`.
    fn position(&self, now: Instant) -> (f64, f64, bool) {
        let t = now.saturating_duration_since(self.start);
        let s = self.anim.sample(self.visible, self.from, self.velocity, t);
        (s.pos, s.vel, !s.done)
    }

    /// Starts animating towards `visible` from the current position and velocity.
    fn retarget(&mut self, visible: bool, now: Instant) {
        let (pos, vel, _) = self.position(now);
        self.from = pos;
        self.velocity = vel;
        self.visible = visible;
        self.start = now;
    }
}

#[derive(Copy, Clone)]
struct Snapshot {
    pos: f64,
    visible: bool,
    animating: bool,
}

fn snapshot() -> Snapshot {
    let guard = SLIDE.lock().unwrap();
    let s = guard.as_ref().expect("slide state not initialized");
    let (pos, _, animating) = s.position(Instant::now());
    Snapshot {
        pos,
        visible: s.visible,
        animating,
    }
}

/// The current state: `shown` or `hidden` (the target, if mid-animation).
pub fn status() -> &'static str {
    if snapshot().visible { "shown" } else { "hidden" }
}

/// Shows or hides the sidebar. With `focus`, a shown (fully slid in) sidebar
/// without keyboard focus gets focused instead of hidden. Returns the resulting state.
pub fn toggle() -> &'static str {
    let state;
    {
        let mut guard = SLIDE.lock().unwrap();
        let s = guard.as_mut().expect("slide state not initialized");
        let now = Instant::now();
        let (_, _, animating) = s.position(now);
        if REFOCUS_ENABLED.load(Relaxed)
            && s.visible
            && !animating
            && FOCUSED.load(Relaxed) == 0
            && !REFOCUS_PENDING.load(Relaxed)
        {
            REFOCUS_PENDING.store(true, Relaxed);
            REFOCUS.fetch_add(1, Relaxed);
            state = "focused";
        } else {
            REFOCUS_PENDING.store(false, Relaxed);
            s.retarget(!s.visible, now);
            state = if s.visible { "shown" } else { "hidden" };
        }
        log::info!("toggle: {state}");
    }
    wake_all();
    state
}

/// Hides the sidebar after it lost keyboard focus (`hide_on_unfocus`).
fn hide_unfocused() {
    {
        let mut guard = SLIDE.lock().unwrap();
        let s = guard.as_mut().expect("slide state not initialized");
        if !s.visible {
            return;
        }
        s.retarget(false, Instant::now());
        log::info!("lost focus, hiding");
    }
    wake_all();
}

/// Wakes all client threads to apply a changed slide state.
fn wake_all() {
    for (_, fd) in WAKERS.lock().unwrap().iter() {
        let _ = uapi::eventfd_write(fd.raw(), 1);
    }
}

/// Layer-surface state for one slide position.
#[derive(Copy, Clone, PartialEq)]
struct Layout {
    /// Margin on the anchored edge.
    margin: i32,
    exclusive_zone: i32,
    keyboard_interactivity: ZwlrLayerSurfaceV1KeyboardInteractivity,
}

fn layout(c: &Config, size: u32, s: Snapshot) -> Layout {
    // Without sliding, the sidebar stays in place while it fades and only moves
    // offscreen once fully hidden.
    let shown = if c.slide {
        s.pos
    } else if s.visible || s.pos > 0.0 {
        1.0
    } else {
        0.0
    };
    let margin = -((1.0 - shown) * size as f64).round() as i32;
    // Compositors (wlroots, smithay/niri) reserve `exclusive_zone + margin` on the
    // anchored edge. Compensate for the (negative) slide margin so the reserved area is
    // a constant `size` for the whole animation. Otherwise every animation frame changes
    // the work area and all tiled windows resize (and re-render) every frame.
    //
    // Reserve as soon as showing starts (windows resize once, then the sidebar slides
    // into the gap) and keep it until fully hidden (one resize at the end).
    let exclusive_zone = if c.reserve && (s.visible || s.pos > 0.0) {
        size as i32 - margin
    } else {
        0
    };
    // Focus changes immediately on toggle, not at the end of the animation. While
    // hidden, `NONE` also drops any on-demand focus the sidebar had from a click, so
    // keystrokes don't go to an invisible terminal.
    let keyboard_interactivity = if s.visible {
        c.keyboard_interactivity
    } else {
        ZwlrLayerSurfaceV1KeyboardInteractivity::NONE
    };
    Layout {
        margin,
        exclusive_zone,
        keyboard_interactivity,
    }
}

// ---------------------------------------------------------------------------------------
// Per-thread layer surfaces
// ---------------------------------------------------------------------------------------

struct Sidebar {
    layer: Weak<ZwlrLayerSurfaceV1>,
    wl_surface: Weak<WlSurface>,
    /// Resolved size in pixels; 0 while still measuring.
    size: Cell<u32>,
    /// The client's minimum size along the sidebar's axis (xdg_toplevel.set_min_size).
    /// Clients don't shrink below it, so it overrides a smaller `size`.
    min: Cell<u32>,
    /// Effective size last sent with set_size.
    placed: Cell<u32>,
    applied: Cell<Option<Layout>>,
    /// Set with `focus`.
    remap: Option<Rc<Remap>>,
    /// `focus`: progress of taking focus with `EXCLUSIVE` interactivity.
    grab: Cell<Grab>,
    /// `focus`: visibility at the last update, to detect when the sidebar is shown.
    shown: Cell<bool>,
}

/// `focus`: taking keyboard focus by briefly setting `EXCLUSIVE` interactivity. If that
/// fails, [`REMAP_FALLBACK`] is set and focus is taken by remapping from then on.
#[derive(Copy, Clone, PartialEq, Debug)]
enum Grab {
    Idle,
    /// `EXCLUSIVE` interactivity until the sidebar gets focus, or the deadline passes.
    Exclusive(Instant),
    /// Got focus and switched back to `ON_DEMAND`. If focus is still there at the
    /// deadline, it worked.
    Settling(Instant),
}

impl Grab {
    fn deadline(self) -> Option<Instant> {
        match self {
            Grab::Idle => None,
            Grab::Exclusive(d) | Grab::Settling(d) => Some(d),
        }
    }
}

impl Sidebar {
    /// Size actually used: the configured size, grown to the client's minimum.
    fn effective(&self) -> u32 {
        match self.size.get() {
            0 => 0,
            size => size.max(self.min.get()),
        }
    }
}

thread_local! {
    static SIDEBARS: RefCell<Vec<Sidebar>> = const { RefCell::new(Vec::new()) };
}

fn send_layout(layer: &ZwlrLayerSurfaceV1, c: &Config, l: Layout) {
    let [t, r, b, left] = c.edge.margins(l.margin);
    layer.send_set_margin(t, r, b, left);
    layer.send_set_exclusive_zone(l.exclusive_zone);
    layer.send_set_keyboard_interactivity(l.keyboard_interactivity);
}

/// Places the sidebar at its edge with the given pixel size and current slide position.
fn place(layer: &ZwlrLayerSurfaceV1, c: &Config, size: u32) -> Layout {
    let (w, h) = c.edge.size(size);
    layer.send_set_size(w, h);
    layer.send_set_anchor(c.edge.anchor());
    let l = layout(c, size, snapshot());
    send_layout(layer, c, l);
    l
}

/// Called from `get_toplevel` for every new layer surface. Does not commit; the
/// client's initial commit applies the state.
///
/// Returns `true` if the size is relative. The surface is then first stretched over the
/// whole output so that the compositor's first configure reports the available area;
/// see [`measured`].
pub fn register(
    layer: &Rc<ZwlrLayerSurfaceV1>,
    wl_surface: &Rc<WlSurface>,
    remap: Option<Rc<Remap>>,
    c: &Config,
) -> bool {
    let (size, applied) = match c.size {
        Size::Pixels(px) => (px, Some(place(layer, c, px))),
        Size::Fraction(_) => {
            use ZwlrLayerSurfaceV1Anchor as A;
            layer.send_set_size(0, 0);
            layer.send_set_anchor(A::TOP | A::BOTTOM | A::LEFT | A::RIGHT);
            (0, None)
        }
    };
    SIDEBARS.with_borrow_mut(|v| {
        v.push(Sidebar {
            layer: Rc::downgrade(layer),
            wl_surface: Rc::downgrade(wl_surface),
            size: Cell::new(size),
            min: Cell::new(0),
            placed: Cell::new(size),
            applied: Cell::new(applied),
            remap,
            grab: Cell::new(Grab::Idle),
            shown: Cell::new(false),
        })
    });
    size == 0
}

/// Handles the measuring configure (already acked) of a relative-size sidebar: resolves
/// the size, moves the surface to its edge, and commits so that the compositor sends
/// the real configure, which is forwarded to the client. The client has not attached a
/// buffer yet, so this commit is buffer-less.
pub fn measured(
    layer: &Rc<ZwlrLayerSurfaceV1>,
    wl_surface: &WlSurface,
    width: u32,
    height: u32,
    c: &Config,
) {
    let Size::Fraction(frac) = c.size else {
        return;
    };
    let full = match c.edge {
        Edge::Left | Edge::Right => width,
        Edge::Top | Edge::Bottom => height,
    };
    let size = ((full as f64 * frac).round() as u32).max(1);
    log::info!("output is {width}x{height}, sidebar size {size}");
    SIDEBARS.with_borrow(|v| {
        if let Some(sb) = v
            .iter()
            .find(|sb| std::ptr::eq(sb.layer.as_ptr(), &**layer))
        {
            sb.size.set(size);
            let size = sb.effective();
            sb.applied.set(Some(place(layer, c, size)));
            sb.placed.set(size);
        }
    });
    wl_surface.send_commit();
}

/// Handles xdg_toplevel.set_min_size. The sidebar grows to the client's minimum along
/// its axis (applied by the next [`update_sidebars`]); otherwise the client draws past
/// the layer surface, outside the reserved area and the hide distance.
pub fn set_min_size(layer: &ZwlrLayerSurfaceV1, width: i32, height: i32, c: &Config) {
    let min = match c.edge {
        Edge::Left | Edge::Right => width,
        Edge::Top | Edge::Bottom => height,
    }
    .max(0) as u32;
    SIDEBARS.with_borrow(|v| {
        if let Some(sb) = v.iter().find(|sb| std::ptr::eq(sb.layer.as_ptr(), layer))
            && sb.min.replace(min) != min
            && min > sb.size.get()
            && sb.size.get() != 0
        {
            log::info!(
                "client minimum size {min} exceeds sidebar size {}, growing",
                sb.size.get()
            );
        }
    });
}

thread_local! {
    /// Last `REFOCUS` value handled by this thread.
    static SEEN_REFOCUS: Cell<u64> = Cell::new(REFOCUS.load(Relaxed));
}

/// Applies the current slide position to all layer surfaces of this thread. Layer-shell
/// state is double-buffered, so we commit the surface ourselves; otherwise an idle
/// client would never pick up the change. Returns when to run again for a pending
/// focus grab.
fn update_sidebars(c: &Config, s: Snapshot) -> Option<Instant> {
    let current = REFOCUS.load(Relaxed);
    let refocus = SEEN_REFOCUS.with(|seen| seen.replace(current)) != current;
    let now = Instant::now();
    let mut wake: Option<Instant> = None;
    SIDEBARS.with_borrow_mut(|v| {
        v.retain(|sb| {
            let (Some(layer), Some(wl_surface)) = (sb.layer.upgrade(), sb.wl_surface.upgrade())
            else {
                return false;
            };
            if layer.server_id().is_none() || wl_surface.server_id().is_none() {
                return false;
            }
            let size = sb.effective();
            if size == 0 {
                return true; // still measuring
            }
            if let Some(remap) = &sb.remap {
                let was_shown = sb.shown.replace(s.visible);
                // With remapping, showing maps the (unmapped while hidden) surface, which
                // takes focus by itself; only a refocus needs an extra unmap.
                let mut remap_now = false;
                if !s.visible {
                    sb.grab.set(Grab::Idle);
                } else if REMAP_FALLBACK.load(Relaxed) {
                    remap_now = refocus;
                } else {
                    if refocus || !was_shown {
                        log::debug!("focus: trying exclusive interactivity");
                        sb.grab.set(Grab::Exclusive(now + GRAB_TIMEOUT));
                    }
                    let focused = FOCUSED.load(Relaxed) > 0;
                    let failed = match sb.grab.get() {
                        Grab::Exclusive(_) if focused => {
                            sb.grab.set(Grab::Settling(now + SETTLE));
                            false
                        }
                        Grab::Settling(d) if now >= d && focused => {
                            log::debug!("focus: exclusive interactivity works");
                            sb.grab.set(Grab::Idle);
                            false
                        }
                        Grab::Exclusive(d) | Grab::Settling(d) => now >= d,
                        Grab::Idle => false,
                    };
                    if failed {
                        log::info!(
                            "focus: exclusive -> on-demand did not keep focus, \
                             falling back to remapping"
                        );
                        REMAP_FALLBACK.store(true, Relaxed);
                        sb.grab.set(Grab::Idle);
                        remap_now = true;
                    }
                    wake = match (wake, sb.grab.get().deadline()) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                }
                // Take focus while shown: unmap, and map again right below.
                if remap_now {
                    if remap.unmap(&wl_surface) {
                        sb.applied.set(None);
                    } else {
                        log::warn!("cannot refocus: surface is {:?}", remap.phase());
                    }
                }
                match remap.phase() {
                    Phase::Unmapped => {
                        if s.visible {
                            // Unmapping reset all layer state; send all of it again.
                            // Some smithay versions also reset the layer (to `background`).
                            if layer.version() >= ZwlrLayerSurfaceV1::MSG__SET_LAYER__SINCE {
                                layer.send_set_layer(c.layer);
                            }
                            sb.applied.set(Some(place(&layer, c, size)));
                            sb.placed.set(size);
                            apply_fade(c, &wl_surface, s);
                            wl_surface.send_commit();
                            remap.start_remap();
                        }
                        // Committing while unmapped would trigger a configure for the
                        // reset (invalid) state.
                        return true;
                    }
                    Phase::Mapped
                        if REMAP_FALLBACK.load(Relaxed)
                            && !s.visible
                            && !s.animating
                            && remap.unmap(&wl_surface) =>
                    {
                        sb.applied.set(None);
                        return true;
                    }
                    _ => {}
                }
            }
            let faded = apply_fade(c, &wl_surface, s);
            if sb.placed.get() != size {
                sb.applied.set(Some(place(&layer, c, size)));
                sb.placed.set(size);
                wl_surface.send_commit();
                return true;
            }
            let mut l = layout(c, size, s);
            if matches!(sb.grab.get(), Grab::Exclusive(_)) {
                l.keyboard_interactivity = ZwlrLayerSurfaceV1KeyboardInteractivity::EXCLUSIVE;
            }
            if sb.applied.get() != Some(l) {
                send_layout(&layer, c, l);
                sb.applied.set(Some(l));
                wl_surface.send_commit();
            } else if faded {
                wl_surface.send_commit();
            }
            true
        })
    });
    wake
}

/// Whether a sidebar of this thread is taking focus with `EXCLUSIVE` interactivity.
/// Focus may briefly come and go meanwhile, which must not trigger `hide_on_unfocus`.
fn grabbing() -> bool {
    SIDEBARS.with_borrow(|v| v.iter().any(|sb| sb.grab.get() != Grab::Idle))
}

/// Sets the alpha multiplier of the sidebar surface tree for the current position.
/// Returns whether `wl_surface` must be committed.
fn apply_fade(c: &Config, wl_surface: &WlSurface, s: Snapshot) -> bool {
    c.fade && fade::apply(wl_surface, s.pos)
}

// ---------------------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------------------

pub fn run(
    name: &str,
    config: Config,
    program: &[String],
    start_hidden: bool,
    anim: Animations,
) -> Result<(), SidebarError> {
    // Before spawning any thread: they all inherit the mask.
    child::block_signals().map_err(SidebarError::BlockSignals)?;
    REFOCUS_ENABLED.store(config.focus, Relaxed);
    HIDE_ON_UNFOCUS.store(config.hide_on_unfocus, Relaxed);
    *SLIDE.lock().unwrap() = Some(Slide {
        visible: !start_hidden,
        from: if start_hidden { 0.0 } else { 1.0 },
        velocity: 0.0,
        start: Instant::now(),
        anim,
    });

    // Becomes the running instance (fails if there already is one) and starts serving
    // `--toggle`, `--close` and `--list`. The lock is held for the lifetime of the process.
    let _instance = ipc::listen(name, |cmd| match cmd {
        "toggle" => Ok(toggle().to_owned()),
        "close" => child::close(c::SIGTERM).map(str::to_owned),
        "status" => Ok(status().to_owned()),
        _ => Err(format!("unknown command `{cmd}`")),
    })?;

    let acceptor = Acceptor::new(1000, false).map_err(SidebarError::CreateAcceptor)?;
    child::spawn(
        Command::new(&program[0])
            .args(&program[1..])
            .with_wayland_display(acceptor.display()),
    )
    .map_err(SidebarError::SpawnChild)?;

    let config = Arc::new(config);
    let mut id = 0;
    loop {
        let socket = acceptor
            .accept()
            .map_err(SidebarError::Accept)?
            .expect("blocking acceptor returned None");
        id += 1;
        log::debug!("client {id} connected");
        let config = config.clone();
        thread::Builder::new()
            .name(format!("client-{id}"))
            .spawn(move || {
                if let Err(e) = run_client(id, socket, &config) {
                    log::error!("client {id}: {}", Report::new(e));
                }
                WAKERS.lock().unwrap().retain(|(i, _)| *i != id);
                log::debug!("client {id} done");
            })
            .map_err(SidebarError::SpawnThread)?;
    }
}

struct ClientHandlerImpl {
    /// Destroys the state (and thereby ends the client loop) when the client goes away.
    _destructor: Destructor,
}

impl ClientHandler for ClientHandlerImpl {}

fn run_client(id: usize, socket: OwnedFd, config: &Arc<Config>) -> Result<(), SidebarError> {
    let efd = uapi::eventfd(0, c::EFD_CLOEXEC | c::EFD_NONBLOCK)
        .map_err(|e| SidebarError::Eventfd(e.into()))?;
    let efd = Arc::new(efd);
    WAKERS.lock().unwrap().push((id, efd.clone()));

    let state = State::builder(proxy::BASELINE)
        .with_log_prefix(&format!("client-{id}"))
        .build()
        .map_err(SidebarError::CreateState)?;
    let client = state
        .add_client(&Rc::new(socket))
        .map_err(SidebarError::CreateState)?;
    client.set_handler(ClientHandlerImpl {
        _destructor: state.create_destructor(),
    });
    client
        .display()
        .set_handler(ClientWlDisplay::new(config.clone()));

    while state.is_not_destroyed() {
        if let Some(deadline) = UNFOCUS_DEADLINE.get()
            && Instant::now() >= deadline
        {
            UNFOCUS_DEADLINE.set(None);
            // Focus may briefly go away while taking it; see `Grab`.
            if FOCUSED.load(Relaxed) == 0 && !grabbing() {
                hide_unfocused();
            }
        }
        let snap = snapshot();
        let wake = update_sidebars(config, snap);
        state.before_poll().map_err(SidebarError::Dispatch)?;
        let mut fds = [
            c::pollfd {
                fd: state.poll_fd().as_raw_fd(),
                events: c::POLLIN,
                revents: 0,
            },
            c::pollfd {
                fd: efd.raw(),
                events: c::POLLIN,
                revents: 0,
            },
        ];
        let mut timeout = if snap.animating { FRAME_MS } else { -1 };
        for deadline in [UNFOCUS_DEADLINE.get(), wake].into_iter().flatten() {
            let ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .saturating_add(1) as c::c_int;
            timeout = if timeout < 0 { ms } else { timeout.min(ms) };
        }
        if let Err(e) = uapi::poll(&mut fds, timeout)
            && e.0 != c::EINTR
        {
            return Err(SidebarError::Poll(e.into()));
        }
        if fds[1].revents != 0 {
            let _ = uapi::eventfd_read(efd.raw());
        }
        if let Err(e) = state.dispatch_available() {
            // The state destroys itself on error, which ends the loop.
            log::error!("could not dispatch: {}", Report::new(e));
        }
    }
    Ok(())
}
