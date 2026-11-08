//! `focus = true`: unmap the sidebar once it's fully hidden and map it again on show.
//!
//! Layer-shell has no request to focus an `on-demand` surface. Compositors such as niri
//! and Umbriel give keyboard focus to an `on-demand` layer surface when it gets mapped,
//! though. Briefly switching to `exclusive` and back doesn't work on niri, but remapping
//! on every show does: it grabs focus once, after which the sidebar behaves like any
//! `on-demand` surface (click elsewhere to unfocus, click it to focus again).
//!
//! The client doesn't know about any of this:
//!
//! - While unmapped, its `attach` and `commit` requests are held back. A layer surface
//!   must not get a buffer before the initial configure is acked, and the configure
//!   sequence is ours, not the client's. Other requests (damage, frame callbacks, ...)
//!   still go to the server and get applied by the commit that maps the surface again.
//! - On show, the buffer the client attached last is attached again. To make that
//!   safe, the compositor's `release` of that buffer is withheld from the client until
//!   the client attaches a different buffer, as if the compositor were still using it.
//!
//! Unmapping resets all layer-surface state (size, anchor, margin, ...), so mapping again
//! re-sends all of it (see `sidebar::update_sidebars`).

use {
    std::{
        cell::{Cell, RefCell},
        rc::{Rc, Weak},
    },
    wl_proxy::{
        object::{ObjectCoreApi, ObjectUtils},
        protocols::{
            wayland::{
                wl_buffer::{WlBuffer, WlBufferHandler},
                wl_surface::WlSurface,
            },
            wlr_layer_shell_unstable_v1::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        },
    },
};

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum Phase {
    /// Requests and events pass through (also before the client's first map).
    Mapped,
    /// Unmapped by us; the client's attaches and commits are held back.
    Unmapped,
    /// We committed the full layer state and wait for the configure to map again.
    Remapping,
}

pub struct Remap {
    phase: Cell<Phase>,
    /// The buffer the client attached last, i.e. the one it considers current.
    buffer: RefCell<Option<Rc<WlBuffer>>>,
    /// The compositor released `buffer`, but the client wasn't told.
    held: Cell<bool>,
    /// The client has committed `buffer`, so the surface is mapped (or would be).
    committed: Cell<bool>,
    /// Size of the last configure forwarded to the client.
    size: Cell<Option<(u32, u32)>>,
}

impl Remap {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            phase: Cell::new(Phase::Mapped),
            buffer: Default::default(),
            held: Cell::new(false),
            committed: Cell::new(false),
            size: Cell::new(None),
        })
    }

    pub fn phase(&self) -> Phase {
        self.phase.get()
    }

    fn is_current(&self, buffer: &WlBuffer) -> bool {
        self.buffer
            .borrow()
            .as_ref()
            .is_some_and(|b| std::ptr::eq(&**b, buffer))
    }

    /// Client `wl_surface.attach`. Returns whether to forward it.
    pub fn attach(self: &Rc<Self>, buffer: Option<&Rc<WlBuffer>>) -> bool {
        if let Some(b) = buffer
            && b.try_get_handler_ref::<ClientWlBuffer>().is_err()
        {
            b.set_handler(ClientWlBuffer {
                remap: Rc::downgrade(self),
            });
        }
        let old = self.buffer.replace(buffer.cloned());
        if let Some(old) = old
            && self.held.get()
            && !buffer.is_some_and(|b| Rc::ptr_eq(b, &old))
        {
            // The compositor released it while it was current; the client may reuse it now.
            old.send_release();
        }
        // If the same buffer is attached again, the compositor releases it again later.
        self.held.set(false);
        self.phase.get() == Phase::Mapped
    }

    /// Client `wl_surface.commit`. Returns whether to forward it.
    pub fn commit(&self) -> bool {
        if self.phase.get() != Phase::Mapped {
            return false;
        }
        self.committed.set(self.buffer.borrow().is_some());
        true
    }

    /// Unmaps the surface (null buffer + commit) if it's mapped. Returns whether it did.
    pub fn unmap(&self, wl_surface: &WlSurface) -> bool {
        if self.phase.get() != Phase::Mapped || !self.committed.get() {
            return false;
        }
        log::debug!("unmapping");
        wl_surface.send_attach(None, 0, 0);
        wl_surface.send_commit();
        self.phase.set(Phase::Unmapped);
        true
    }

    /// Called after the full layer state was sent and committed while unmapped; the
    /// compositor answers with a new initial configure.
    pub fn start_remap(&self) {
        log::debug!("remapping");
        self.phase.set(Phase::Remapping);
    }

    /// Layer-surface configure. Returns `true` if handled here, `false` if it must be
    /// forwarded to the client.
    pub fn configure(
        &self,
        layer: &ZwlrLayerSurfaceV1,
        wl_surface: &WlSurface,
        serial: u32,
        width: u32,
        height: u32,
    ) -> bool {
        match self.phase.get() {
            Phase::Mapped => {
                self.size.set(Some((width, height)));
                false
            }
            // A configure that was in flight when we unmapped. The remap commit triggers
            // a fresh one.
            Phase::Unmapped => true,
            Phase::Remapping => {
                self.phase.set(Phase::Mapped);
                let buffer = self.buffer.borrow().clone();
                match buffer {
                    Some(buffer) if self.size.get() == Some((width, height)) => {
                        layer.send_ack_configure(serial);
                        wl_surface.send_attach(Some(&buffer), 0, 0);
                        if wl_surface.version() >= WlSurface::MSG__DAMAGE_BUFFER__SINCE {
                            wl_surface.send_damage_buffer(0, 0, i32::MAX, i32::MAX);
                        } else {
                            wl_surface.send_damage(0, 0, i32::MAX, i32::MAX);
                        }
                        wl_surface.send_commit();
                        self.committed.set(true);
                        log::debug!("remapped");
                        true
                    }
                    // The size changed (or the client destroyed its buffer): let the
                    // client draw a new buffer; its commit maps the surface.
                    _ => {
                        log::debug!("remapping via the client");
                        self.size.set(Some((width, height)));
                        false
                    }
                }
            }
        }
    }
}

/// Handler for client-created wl_buffer objects attached to a remapped surface.
struct ClientWlBuffer {
    remap: Weak<Remap>,
}

impl WlBufferHandler for ClientWlBuffer {
    fn handle_release(&mut self, slf: &Rc<WlBuffer>) {
        if let Some(r) = self.remap.upgrade()
            && r.is_current(slf)
        {
            r.held.set(true);
            return;
        }
        slf.send_release();
    }

    fn handle_destroy(&mut self, slf: &Rc<WlBuffer>) {
        if let Some(r) = self.remap.upgrade()
            && r.is_current(slf)
        {
            r.buffer.take();
            r.held.set(false);
        }
        slf.send_destroy();
    }
}
