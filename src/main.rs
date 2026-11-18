//! Fullscreen Wayland client that shows domain-warped keyframes and keeps the display awake.
//!
//! Each keyframe lives in a shared-memory buffer of its own, which exists from the start of its
//! drawing until the next keyframe has faded in over it. Frames only move the viewport source
//! rectangles and change the alpha multipliers, and the compositor does the scaling and blending.

use std::error::Error;
use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{MemfdFlags, memfd_create};
use rustix::io::Errno;
use screensaver::{SLOTS, Screen, Source, Stage, Surface, physical_size};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_region, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::alpha_modifier::v1::client::{
    wp_alpha_modifier_surface_v1, wp_alpha_modifier_v1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1,
};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1;
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);
/// Time after which a frame starts no further row of a keyframe, except for the first keyframe,
/// which is drawn whole. A row takes about a tenth of a millisecond, so no frame spends much
/// more than 1.5 ms drawing.
const DRAW_BUDGET: Duration = Duration::from_micros(1500);
const FALLBACK_SIZE: (i32, i32) = (1280, 720);
const KEY_ESCAPE: u32 = 1;
/// Fractional scale in 120ths when the compositor does not send one.
const UNSCALED: u32 = 120;

/// One keyframe slot: a synchronized subsurface with its own viewport and alpha multiplier.
struct Slot {
    surface: wl_surface::WlSurface,
    subsurface: wl_subsurface::WlSubsurface,
    viewport: wp_viewport::WpViewport,
    alpha: wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1,
}

/// The memory of one keyframe and the buffer that shows it to the compositor.
struct Store {
    file: File,
    buffer: wl_buffer::WlBuffer,
    /// Bytes per row.
    stride: usize,
}

impl Drop for Store {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// The stage of one logical size and scale, and when its last frame was shown.
struct Sized {
    size: (i32, i32),
    scale: u32,
    stage: Stage<Store>,
    last_frame: Option<Instant>,
}

/// The window's surfaces as the stage addresses them. Frame callbacks carry `frame`, the number
/// of the frame that asked for them.
struct Window<'a> {
    surface: &'a wl_surface::WlSurface,
    slots: &'a [Slot],
    shm: &'a wl_shm::WlShm,
    qh: &'a QueueHandle<App>,
    frame: u64,
}

impl Screen for Window<'_> {
    type Buffer = Store;

    fn create_buffer(
        &mut self,
        slot: usize,
        width: usize,
        height: usize,
    ) -> std::io::Result<Store> {
        let length = width * height * 4;
        let file = File::from(memfd_create("screensaver", MemfdFlags::CLOEXEC)?);
        file.set_len(length as u64)?;
        let pool = self
            .shm
            .create_pool(file.as_fd(), length as i32, self.qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Xrgb8888,
            self.qh,
            slot,
        );
        pool.destroy();
        Ok(Store {
            file,
            buffer,
            stride: width * 4,
        })
    }

    fn write_row(&mut self, buffer: &Store, row: usize, pixels: &[u8]) -> std::io::Result<()> {
        buffer
            .file
            .write_all_at(pixels, (row * buffer.stride) as u64)
    }

    fn destroy_buffer(&mut self, buffer: Store) {
        drop(buffer);
    }

    fn attach(&mut self, slot: usize, buffer: Option<&Store>) {
        let surface = &self.slots[slot].surface;
        surface.attach(buffer.map(|store| &store.buffer), 0, 0);
        if buffer.is_some() {
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        }
    }

    fn set_source(&mut self, slot: usize, source: Source) {
        let [x, y, width, height] = source.pixels();
        self.slots[slot].viewport.set_source(x, y, width, height);
    }

    fn set_alpha(&mut self, slot: usize, multiplier: u32) {
        self.slots[slot].alpha.set_multiplier(multiplier);
    }

    fn place_above(&mut self, slot: usize, below: usize) {
        self.slots[slot]
            .subsurface
            .place_above(&self.slots[below].surface);
    }

    fn request_frame(&mut self, surface: Surface) {
        let surface = match surface {
            Surface::Window => self.surface,
            Surface::Slot(slot) => &self.slots[slot].surface,
        };
        surface.frame(self.qh, self.frame);
    }

    fn commit(&mut self, slot: usize) {
        self.slots[slot].surface.commit();
    }
}

struct App {
    running: bool,
    compositor: wl_compositor::WlCompositor,
    subcompositor: wl_subcompositor::WlSubcompositor,
    shm: wl_shm::WlShm,
    viewporter: wp_viewporter::WpViewporter,
    alpha_modifier: wp_alpha_modifier_v1::WpAlphaModifierV1,
    surface: wl_surface::WlSurface,
    viewport: wp_viewport::WpViewport,
    xdg_surface: xdg_surface::XdgSurface,
    /// One opaque black pixel that the viewport stretches over the whole window.
    black: wl_buffer::WlBuffer,
    black_attached: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    /// Size from the latest toplevel configure, if the compositor chose one.
    requested_size: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    /// Preferred fractional scale in 120ths.
    scale: Option<u32>,
    slots: Vec<Slot>,
    stage: Option<Sized>,
    seed: u64,
    /// Number of the last frame sent, which its frame callbacks carry.
    frame: u64,
    frame_pending: bool,
    redraw: bool,
}

impl App {
    fn configure(&mut self, qh: &QueueHandle<Self>) {
        let (width, height) = self.requested_size.or(self.size).unwrap_or(FALLBACK_SIZE);
        self.viewport.set_destination(width, height);
        let region = self.compositor.create_region(qh, ());
        region.add(0, 0, width, height);
        self.surface.set_opaque_region(Some(&region));
        region.destroy();
        // Without an explicit geometry the window spans every subsurface, so a subsurface that
        // reached past the screen edge would move the fullscreen window.
        self.xdg_surface.set_window_geometry(0, 0, width, height);
        if !self.black_attached {
            self.surface.attach(Some(&self.black), 0, 0);
            self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            self.black_attached = true;
        }
        self.size = Some((width, height));
        self.redraw = true;
    }

    /// Builds the keyframe buffers and a new timeline once the size and scale are known, and
    /// again whenever either changes.
    fn prepare_stage(&mut self, qh: &QueueHandle<Self>) -> std::io::Result<()> {
        let (Some(size), Some(scale)) = (self.size, self.scale) else {
            return Ok(());
        };
        if self
            .stage
            .as_ref()
            .is_some_and(|stage| stage.size == size && stage.scale == scale)
        {
            return Ok(());
        }
        self.stage = None;
        let logical = [size.0, size.1].map(|s| s.max(1));
        let screen = logical.map(|s| physical_size(s as u32, scale));
        while self.slots.len() < SLOTS {
            let surface = self.compositor.create_surface(qh, ());
            let subsurface = self
                .subcompositor
                .get_subsurface(&surface, &self.surface, qh, ());
            let viewport = self.viewporter.get_viewport(&surface, qh, ());
            let alpha = self.alpha_modifier.get_surface(&surface, qh, ());
            let region = self.compositor.create_region(qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
            self.slots.push(Slot {
                surface,
                subsurface,
                viewport,
                alpha,
            });
        }

        for slot in &self.slots {
            slot.viewport.set_destination(logical[0], logical[1]);
            let region = self.compositor.create_region(qh, ());
            region.add(0, 0, logical[0], logical[1]);
            slot.surface.set_opaque_region(Some(&region));
            region.destroy();
            slot.surface.attach(None, 0, 0);
            slot.surface.commit();
        }
        self.stage = Some(Sized {
            size,
            scale,
            stage: Stage::new(self.seed, screen),
            last_frame: None,
        });
        self.redraw = true;
        Ok(())
    }

    /// The window's surfaces for the stage, with the number of the frame being sent.
    fn window<'a>(&'a self, qh: &'a QueueHandle<Self>) -> Window<'a> {
        Window {
            surface: &self.surface,
            slots: &self.slots,
            shm: &self.shm,
            qh,
            frame: self.frame,
        }
    }

    /// Updates the keyframes and commits the window. The stage asks for the frame callback on
    /// the surfaces that show, and without a stage the window carries it.
    fn frame(&mut self, qh: &QueueHandle<Self>) -> std::io::Result<()> {
        self.frame += 1;
        if let Some(mut sized) = self.stage.take() {
            let now = Instant::now();
            let elapsed = sized
                .last_frame
                .map_or(0.0, |last| (now - last).as_secs_f64());
            sized.last_frame = Some(now);
            let deadline = now + DRAW_BUDGET;
            let result = sized
                .stage
                .frame(elapsed, &mut self.window(qh), || Instant::now() < deadline);
            self.stage = Some(sized);
            result?;
        } else {
            self.surface.frame(qh, self.frame);
        }
        self.surface.commit();
        self.frame_pending = true;
        self.redraw = false;
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();

    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let subcompositor: wl_subcompositor::WlSubcompositor = globals.bind(&qh, 1..=1, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ())?;
    let viewporter: wp_viewporter::WpViewporter = globals.bind(&qh, 1..=1, ())?;
    let pixels: wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1 =
        globals.bind(&qh, 1..=1, ())?;
    let alpha_modifier: wp_alpha_modifier_v1::WpAlphaModifierV1 = globals.bind(&qh, 1..=1, ())?;
    let _seat: Option<wl_seat::WlSeat> = globals.bind(&qh, 1..=7, ()).ok();

    let surface = compositor.create_surface(&qh, ());
    let viewport = viewporter.get_viewport(&surface, &qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("Screensaver".into());
    toplevel.set_app_id("screensaver".into());
    toplevel.set_fullscreen(None);
    let black = pixels.create_u32_rgba_buffer(0, 0, 0, u32::MAX, &qh, ());

    let scale = match globals
        .bind::<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
    {
        Ok(manager) => {
            manager.get_fractional_scale(&surface, &qh, ());
            None
        }
        Err(_) => Some(UNSCALED),
    };
    let _inhibitor = match globals
        .bind::<zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, _, _>(&qh, 1..=1, ())
    {
        Ok(manager) => Some(manager.create_inhibitor(&surface, &qh, ())),
        Err(_) => {
            eprintln!(
                "screensaver: the compositor has no idle inhibitor, so the display may sleep"
            );
            None
        }
    };
    surface.commit();

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    let mut app = App {
        running: true,
        compositor,
        subcompositor,
        shm,
        viewporter,
        alpha_modifier,
        surface,
        viewport,
        xdg_surface,
        black,
        black_attached: false,
        keyboard: None,
        pointer: None,
        requested_size: None,
        size: None,
        scale,
        slots: Vec::new(),
        stage: None,
        seed,
        frame: 0,
        frame_pending: false,
        redraw: false,
    };

    let mut next_frame = Instant::now();
    while app.running {
        queue.dispatch_pending(&mut app)?;
        if !app.running {
            break;
        }
        app.prepare_stage(&qh)?;

        let now = Instant::now();
        let mut timeout = None;
        if app.size.is_some() && (app.redraw || !app.frame_pending) {
            if app.redraw || now >= next_frame {
                app.frame(&qh)?;
                let scheduled = next_frame + FRAME_INTERVAL;
                next_frame = if scheduled > now {
                    scheduled
                } else {
                    now + FRAME_INTERVAL
                };
            } else {
                timeout = Some(next_frame - now);
            }
        }

        let flush_blocked = match conn.flush() {
            Ok(()) => false,
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => true,
            Err(e) => return Err(e.into()),
        };
        // The first keyframe is drawn after the first black frame is sent, so the screen goes dark
        // at once.
        if let Some(mut sized) = app.stage.take() {
            let result = sized.stage.draw_first(&mut app.window(&qh));
            app.stage = Some(sized);
            result?;
        }

        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let fd = guard.connection_fd();
        let flags = if flush_blocked {
            PollFlags::IN | PollFlags::OUT
        } else {
            PollFlags::IN
        };
        let mut fds = [PollFd::new(&fd, flags)];
        let timeout = timeout.map(Timespec::try_from).transpose()?;
        match poll(&mut fds, timeout.as_ref()) {
            Ok(_)
                if fds[0]
                    .revents()
                    .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP) =>
            {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(_) | Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        app: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            app.configure(qh);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        app: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                app.requested_size = (width > 0 && height > 0).then_some((width, height));
            }
            xdg_toplevel::Event::Close => app.running = false,
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            app.scale = Some(scale);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u64> for App {
    fn event(
        app: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        frame: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // A surface that stopped showing answers an older frame up to a second late.
        if let wl_callback::Event::Done { .. } = event
            && *frame == app.frame
        {
            app.frame_pending = false;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for App {
    fn event(
        app: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        slot: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(sized) = &mut app.stage
        {
            sized.stage.released(*slot);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        app: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Keyboard) && app.keyboard.is_none() {
                app.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Pointer) && app.pointer.is_none() {
                app.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            key: KEY_ESCAPE,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        {
            app.running = false;
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(
        _: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Enter { serial, .. } = event {
            pointer.set_cursor(serial, None, 0, 0);
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_region::WlRegion);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: wl_subcompositor::WlSubcompositor);
delegate_noop!(App: wl_subsurface::WlSubsurface);
delegate_noop!(App: wp_viewporter::WpViewporter);
delegate_noop!(App: wp_viewport::WpViewport);
delegate_noop!(App: wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1);
delegate_noop!(App: wp_alpha_modifier_v1::WpAlphaModifierV1);
delegate_noop!(App: wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1);
delegate_noop!(App: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(App: zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1);
delegate_noop!(App: zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1);
