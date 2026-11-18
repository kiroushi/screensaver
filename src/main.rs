//! Fullscreen Wayland client that shows flowing colour ribbons and keeps the display awake.

use std::error::Error;
use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{MemfdFlags, memfd_create};
use rustix::io::Errno;
use screensaver::{BUFFER_WIDTH, Renderer, buffer_height};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_region, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);
const FALLBACK_SIZE: (i32, i32) = (1280, 720);
const KEY_ESCAPE: u32 = 1;

struct Slot {
    buffer: wl_buffer::WlBuffer,
    offset: u64,
    busy: bool,
}

/// Two XRGB8888 buffers in one shared-memory pool, written with pwrite.
struct Buffers {
    file: File,
    slots: [Slot; 2],
}

impl Buffers {
    fn new(shm: &wl_shm::WlShm, height: usize, qh: &QueueHandle<App>) -> std::io::Result<Self> {
        let frame_len = BUFFER_WIDTH * height * 4;
        let file = File::from(memfd_create("screensaver", MemfdFlags::CLOEXEC)?);
        file.set_len((frame_len * 2) as u64)?;
        let pool = shm.create_pool(file.as_fd(), (frame_len * 2) as i32, qh, ());
        let slots = std::array::from_fn(|i| Slot {
            buffer: pool.create_buffer(
                (i * frame_len) as i32,
                BUFFER_WIDTH as i32,
                height as i32,
                (BUFFER_WIDTH * 4) as i32,
                wl_shm::Format::Xrgb8888,
                qh,
                (),
            ),
            offset: (i * frame_len) as u64,
            busy: false,
        });
        pool.destroy();
        Ok(Self { file, slots })
    }
}

impl Drop for Buffers {
    fn drop(&mut self) {
        for slot in &self.slots {
            slot.buffer.destroy();
        }
    }
}

struct App {
    running: bool,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    viewport: wp_viewport::WpViewport,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    /// Size from the latest toplevel configure, if the compositor chose one.
    requested_size: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    buffers: Option<Buffers>,
    renderer: Renderer,
    frame: Vec<u8>,
    frame_pending: bool,
    redraw: bool,
}

impl App {
    fn configure(&mut self, qh: &QueueHandle<Self>) -> std::io::Result<()> {
        let (width, height) = self.requested_size.or(self.size).unwrap_or(FALLBACK_SIZE);
        self.viewport.set_destination(width, height);
        let region = self.compositor.create_region(qh, ());
        region.add(0, 0, width, height);
        self.surface.set_opaque_region(Some(&region));
        region.destroy();

        let rows = buffer_height(width as u32, height as u32);
        if self.buffers.is_none() || rows != self.renderer.height() {
            self.buffers = None;
            self.buffers = Some(Buffers::new(&self.shm, rows, qh)?);
            self.renderer = Renderer::new(BUFFER_WIDTH, rows);
            self.frame = vec![0; self.renderer.frame_len()];
        }
        self.size = Some((width, height));
        self.redraw = true;
        Ok(())
    }

    /// Renders and commits a frame. Returns false when no buffer is free.
    fn draw(&mut self, time: f64, qh: &QueueHandle<Self>) -> std::io::Result<bool> {
        let Some(buffers) = &mut self.buffers else {
            return Ok(false);
        };
        let Some(slot) = buffers.slots.iter_mut().find(|slot| !slot.busy) else {
            return Ok(false);
        };
        self.renderer.render(time, &mut self.frame);
        buffers.file.write_all_at(&self.frame, slot.offset)?;
        self.surface.attach(Some(&slot.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.surface.frame(qh, ());
        self.surface.commit();
        slot.busy = true;
        self.frame_pending = true;
        self.redraw = false;
        Ok(true)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();

    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ())?;
    let viewporter: wp_viewporter::WpViewporter = globals.bind(&qh, 1..=1, ())?;
    let _seat: Option<wl_seat::WlSeat> = globals.bind(&qh, 1..=7, ()).ok();

    let surface = compositor.create_surface(&qh, ());
    let viewport = viewporter.get_viewport(&surface, &qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("Screensaver".into());
    toplevel.set_app_id("screensaver".into());
    toplevel.set_fullscreen(None);

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

    let mut app = App {
        running: true,
        compositor,
        shm,
        surface,
        viewport,
        keyboard: None,
        pointer: None,
        requested_size: None,
        size: None,
        buffers: None,
        renderer: Renderer::new(BUFFER_WIDTH, 0),
        frame: Vec::new(),
        frame_pending: false,
        redraw: false,
    };

    let start = Instant::now();
    let mut next_frame = start;
    while app.running {
        queue.dispatch_pending(&mut app)?;
        if !app.running {
            break;
        }

        let now = Instant::now();
        let mut timeout = None;
        if app.buffers.is_some() && (app.redraw || !app.frame_pending) {
            if app.redraw || now >= next_frame {
                if app.draw(start.elapsed().as_secs_f64(), &qh)? {
                    let scheduled = next_frame + FRAME_INTERVAL;
                    next_frame = if scheduled > now {
                        scheduled
                    } else {
                        now + FRAME_INTERVAL
                    };
                }
            } else {
                timeout = Some(next_frame - now);
            }
        }

        let flush_blocked = match conn.flush() {
            Ok(()) => false,
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => true,
            Err(e) => return Err(e.into()),
        };

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
            if let Err(error) = app.configure(qh) {
                eprintln!("screensaver: cannot create the frame buffers: {error}");
                app.running = false;
            }
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

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            app.frame_pending = false;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for App {
    fn event(
        app: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(buffers) = &mut app.buffers
            && let Some(slot) = buffers.slots.iter_mut().find(|slot| slot.buffer == *buffer)
        {
            slot.busy = false;
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
delegate_noop!(App: wp_viewporter::WpViewporter);
delegate_noop!(App: wp_viewport::WpViewport);
delegate_noop!(App: zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1);
delegate_noop!(App: zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1);
