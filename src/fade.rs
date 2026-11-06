//! `fade = true`: fades the sidebar with `wp_alpha_modifier_v1`.
//!
//! The alpha multiplier only applies to the one `wl_surface` it's set on, not to its
//! subsurfaces. Many clients (Chromium/Electron, Firefox, video players) draw their
//! content into subsurfaces, so every surface of the sidebar's subsurface tree gets its
//! own alpha modifier.
//!
//! A surface can have only one alpha modifier object, so the global is hidden from
//! the client while fading is enabled.
//!
//! All of this is per client thread, like the proxy state it refers to.

use {
    std::{
        cell::{Cell, RefCell},
        rc::{Rc, Weak},
    },
    wl_proxy::{
        object::ObjectCoreApi,
        protocols::{
            alpha_modifier_v1::{
                wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1,
                wp_alpha_modifier_v1::WpAlphaModifierV1,
            },
            wayland::wl_surface::WlSurface,
        },
    },
};

thread_local! {
    /// The proxy-bound `wp_alpha_modifier_v1`, if the compositor has it.
    static MANAGER: RefCell<Option<Rc<WpAlphaModifierV1>>> = const { RefCell::new(None) };
    /// Every client surface (created while fading is enabled).
    static SURFACES: RefCell<Vec<Rc<FadeSurface>>> = const { RefCell::new(Vec::new()) };
}

/// Upper bound on subsurface nesting, in case a client builds a cycle (a protocol
/// error the compositor reports, but we see the requests first).
const MAX_DEPTH: usize = 32;

pub fn set_manager(manager: Rc<WpAlphaModifierV1>) {
    MANAGER.set(Some(manager));
}

pub fn has_manager() -> bool {
    MANAGER.with_borrow(Option::is_some)
}

/// Fade state of one client `wl_surface`.
pub struct FadeSurface {
    surface: Weak<WlSurface>,
    /// Set while the surface is a subsurface.
    parent: RefCell<Option<Weak<WlSurface>>>,
    /// Created on first use.
    alpha: RefCell<Option<Rc<WpAlphaModifierSurfaceV1>>>,
    /// Multiplier last sent; starts at the protocol default (opaque).
    applied: Cell<u32>,
    /// The client has committed the surface at least once. Before that, committing
    /// it ourselves could change what its first commit means (e.g. the initial
    /// xdg_surface commit), so it's left alone.
    committed: Cell<bool>,
    /// The client destroyed the surface.
    destroyed: Cell<bool>,
}

impl FadeSurface {
    /// Creates and registers the fade state of a new client surface.
    pub fn new(surface: &Rc<WlSurface>) -> Rc<Self> {
        let f = Rc::new(Self {
            surface: Rc::downgrade(surface),
            parent: Default::default(),
            alpha: Default::default(),
            applied: Cell::new(u32::MAX),
            committed: Cell::new(false),
            destroyed: Cell::new(false),
        });
        SURFACES.with_borrow_mut(|v| v.push(f.clone()));
        f
    }

    /// `wl_subcompositor.get_subsurface` (`Some`) or `wl_subsurface.destroy` (`None`).
    pub fn set_parent(&self, parent: Option<&Rc<WlSurface>>) {
        *self.parent.borrow_mut() = parent.map(Rc::downgrade);
    }

    /// Client `wl_surface.commit`.
    pub fn committed(&self) {
        self.committed.set(true);
    }

    /// Client `wl_surface.destroy`; must run before it's forwarded, since the alpha
    /// modifier has to be destroyed first.
    pub fn destroy(&self) {
        self.destroyed.set(true);
        if let Some(alpha) = self.alpha.take() {
            alpha.send_destroy();
        }
    }

    fn surface(&self) -> Option<Rc<WlSurface>> {
        if self.destroyed.get() {
            return None;
        }
        self.surface.upgrade().filter(|s| s.server_id().is_some())
    }
}

fn find<'a>(v: &'a [Rc<FadeSurface>], s: &WlSurface) -> Option<&'a Rc<FadeSurface>> {
    v.iter().find(|f| std::ptr::eq(f.surface.as_ptr(), s))
}

/// How many subsurface levels `s` is below `root`, if it's in `root`'s tree.
fn depth_below(v: &[Rc<FadeSurface>], s: &Rc<WlSurface>, root: &WlSurface) -> Option<usize> {
    let mut cur = s.clone();
    for depth in 0..MAX_DEPTH {
        if std::ptr::eq(&*cur, root) {
            return Some(depth);
        }
        let parent = find(v, &cur)?.parent.borrow().as_ref()?.upgrade()?;
        cur = parent;
    }
    None
}

/// Converts an opacity in `[0, 1]` to an alpha multiplier.
fn factor(alpha: f64) -> u32 {
    (alpha.clamp(0.0, 1.0) * u32::MAX as f64).round() as u32
}

/// Sets the alpha multiplier of `root` and its subsurface tree. Subsurfaces whose
/// multiplier changed are committed here, deepest first. Their state is cached until
/// the parent commits (synchronized subsurfaces), so it all appears together with the
/// root's next commit.
///
/// Returns whether `root`'s multiplier changed; the caller must then commit it.
pub fn apply(root: &WlSurface, alpha: f64) -> bool {
    let Some(manager) = MANAGER.with_borrow(|m| m.clone()) else {
        return false;
    };
    let factor = factor(alpha);
    SURFACES.with_borrow_mut(|v| {
        v.retain(|f| f.surface().is_some());
        let mut root_changed = false;
        let mut children = vec![];
        for f in v.iter() {
            if f.applied.get() == factor {
                continue;
            }
            let Some(s) = f.surface() else { continue };
            let Some(depth) = depth_below(v, &s, root) else {
                continue;
            };
            if !f.committed.get() {
                continue;
            }
            let alpha = f
                .alpha
                .borrow_mut()
                .get_or_insert_with(|| {
                    let a = manager.create_child::<WpAlphaModifierSurfaceV1>();
                    manager.send_get_surface(&a, &s);
                    a
                })
                .clone();
            alpha.send_set_multiplier(factor);
            f.applied.set(factor);
            if depth == 0 {
                root_changed = true;
            } else {
                children.push((depth, s));
            }
        }
        children.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
        for (_, s) in children {
            s.send_commit();
        }
        root_changed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factor_range() {
        assert_eq!(factor(0.0), 0);
        assert_eq!(factor(1.0), u32::MAX);
        assert_eq!(factor(-0.5), 0);
        assert_eq!(factor(1.5), u32::MAX);
        assert_eq!(factor(0.5), u32::MAX / 2 + 1);
    }
}
