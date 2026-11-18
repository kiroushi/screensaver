//! The keyframe slots of the window: which buffer each one holds, what the compositor last heard
//! about it, and the drawing of the next keyframe.
//!
//! The stage talks to the compositor only through [`Screen`], so its tests drive it against a
//! model of the compositor.

use std::io;

use crate::{Raster, Source, Timeline, buffer_layout};

/// Keyframe surfaces, and the most keyframe buffers alive at once.
pub const SLOTS: usize = 2;

/// A surface of the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// The toplevel, which shows opaque black under the keyframes.
    Window,
    /// The synchronized subsurface of a keyframe slot.
    Slot(usize),
}

/// The requests the stage sends to the compositor. Every request for a slot takes effect with
/// the next commit of the window.
pub trait Screen {
    /// A keyframe's memory and the buffer that shows it to the compositor.
    type Buffer;

    /// A buffer of `width` by `height` XRGB8888 pixels for `slot`.
    fn create_buffer(
        &mut self,
        slot: usize,
        width: usize,
        height: usize,
    ) -> io::Result<Self::Buffer>;

    /// Writes one row of a buffer that was never attached.
    fn write_row(&mut self, buffer: &Self::Buffer, row: usize, pixels: &[u8]) -> io::Result<()>;

    fn destroy_buffer(&mut self, buffer: Self::Buffer);

    /// Attaches a buffer to a slot with damage over all of it, or detaches the slot's buffer.
    fn attach(&mut self, slot: usize, buffer: Option<&Self::Buffer>);

    fn set_source(&mut self, slot: usize, source: Source);

    fn set_alpha(&mut self, slot: usize, multiplier: u32);

    fn place_above(&mut self, slot: usize, below: usize);

    /// Asks for a frame callback on a surface.
    fn request_frame(&mut self, surface: Surface);

    fn commit(&mut self, slot: usize);
}

/// A slot's keyframe buffer and the state the compositor last received for it.
struct Canvas<B> {
    /// Present from the start of the keyframe's drawing until the compositor releases it.
    buffer: Option<B>,
    /// The compositor may still read the buffer, so it must not be destroyed.
    busy: bool,
    attached: bool,
    /// Keyframe the buffer holds.
    drawn: Option<u64>,
    source: Option<Source>,
    multiplier: Option<u32>,
}

impl<B> Default for Canvas<B> {
    fn default() -> Self {
        Self {
            buffer: None,
            busy: false,
            attached: false,
            drawn: None,
            source: None,
            multiplier: None,
        }
    }
}

/// A keyframe being drawn into its slot's buffer, row by row.
struct Job {
    index: u64,
    raster: Raster,
}

/// The keyframes of one screen size: their timeline, their buffers and the next one's drawing.
pub struct Stage<B> {
    screen_height: u32,
    buffer: [usize; 2],
    /// Screen pixels per buffer pixel.
    buffer_scale: f64,
    canvases: [Canvas<B>; SLOTS],
    timeline: Timeline,
    job: Option<Job>,
    band: Vec<u8>,
    /// Slot whose surface is above the other.
    top: usize,
}

fn slot_of(index: u64) -> usize {
    (index % SLOTS as u64) as usize
}

impl<B> Stage<B> {
    /// A stage for a physical screen size. The slots must have no buffer attached, with the
    /// last slot on top.
    pub fn new(seed: u64, screen: [u32; 2]) -> Self {
        let (buffer, buffer_scale) = buffer_layout(screen);
        Self {
            screen_height: screen[1],
            buffer: buffer.map(|s| s as usize),
            buffer_scale,
            canvases: Default::default(),
            timeline: Timeline::new(seed, screen, buffer, buffer_scale),
            job: None,
            band: Vec::new(),
            top: SLOTS - 1,
        }
    }

    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// Records that the compositor released the buffer of `slot`.
    pub fn released(&mut self, slot: usize) {
        if let Some(canvas) = self.canvases.get_mut(slot) {
            canvas.busy = false;
        }
    }

    /// Shows the timeline `elapsed` seconds after the last frame, asks for the next frame
    /// callback, and draws rows of the next keyframe while `more` allows. The caller commits the
    /// window afterwards.
    pub fn frame<S: Screen<Buffer = B>>(
        &mut self,
        elapsed: f64,
        screen: &mut S,
        more: impl FnMut() -> bool,
    ) -> io::Result<()> {
        self.timeline.advance(elapsed);

        let mut layers = [None; SLOTS];
        for (depth, layer) in self.timeline.layers().enumerate() {
            layers[layer.slot] = Some(layer);
            if depth > 0 && layer.slot != self.top {
                screen.place_above(layer.slot, self.top);
                self.top = layer.slot;
            }
        }
        let drawing = self.job.as_ref().map(|job| slot_of(job.index));
        let mut opaque = false;
        for ((number, canvas), layer) in self.canvases.iter_mut().enumerate().zip(layers) {
            let Some(layer) = layer else {
                if canvas.attached {
                    screen.attach(number, None);
                    screen.commit(number);
                    canvas.attached = false;
                    canvas.drawn = None;
                    canvas.source = None;
                    canvas.multiplier = None;
                } else if canvas.drawn.is_none()
                    && !canvas.busy
                    && drawing != Some(number)
                    && let Some(buffer) = canvas.buffer.take()
                {
                    screen.destroy_buffer(buffer);
                }
                continue;
            };
            debug_assert_eq!(canvas.drawn, Some(layer.index));
            if !canvas.attached {
                let buffer = canvas
                    .buffer
                    .as_ref()
                    .expect("a drawn keyframe has a buffer");
                screen.attach(number, Some(buffer));
                canvas.attached = true;
                canvas.busy = true;
            }
            if canvas.source != Some(layer.source) {
                screen.set_source(number, layer.source);
                canvas.source = Some(layer.source);
            }
            let multiplier = (layer.alpha * f64::from(u32::MAX)).round() as u32;
            if canvas.multiplier != Some(multiplier) {
                screen.set_alpha(number, multiplier);
                canvas.multiplier = Some(multiplier);
            }
            opaque |= multiplier == u32::MAX;
            // cosmic-comp sends a frame callback every refresh only to a surface that shows on
            // screen. A surface hidden behind opaque ones gets one every 995 ms. The window is
            // hidden as soon as a keyframe is opaque, so the keyframes carry the callback.
            screen.request_frame(Surface::Slot(number));
            screen.commit(number);
        }
        if !opaque {
            screen.request_frame(Surface::Window);
        }
        self.draw(screen, more)
    }

    /// Draws rows of the keyframe the timeline needs next while `more` allows.
    fn draw<S: Screen<Buffer = B>>(
        &mut self,
        screen: &mut S,
        mut more: impl FnMut() -> bool,
    ) -> io::Result<()> {
        while self.draw_row(screen)? && more() {}
        Ok(())
    }

    /// Draws the whole first keyframe at once, so the screen is black only for as long as that
    /// takes.
    pub fn draw_first<S: Screen<Buffer = B>>(&mut self, screen: &mut S) -> io::Result<()> {
        while self.timeline.wanted() == Some(0) && self.draw_row(screen)? {}
        Ok(())
    }

    /// Draws one row of the keyframe the timeline needs next and returns whether there was one.
    /// The keyframe gets a new buffer once the compositor has released the slot's last one. The
    /// compositor sees the new buffer only after its last row is written.
    fn draw_row<S: Screen<Buffer = B>>(&mut self, screen: &mut S) -> io::Result<bool> {
        if self.job.is_none() {
            let Some(index) = self.timeline.wanted() else {
                return Ok(false);
            };
            let slot = slot_of(index);
            let canvas = &mut self.canvases[slot];
            if canvas.busy || canvas.attached {
                return Ok(false);
            }
            let [width, height] = self.buffer;
            if let Some(old) = canvas
                .buffer
                .replace(screen.create_buffer(slot, width, height)?)
            {
                screen.destroy_buffer(old);
            }
            let look = self.timeline.look(index);
            self.job = Some(Job {
                index,
                raster: Raster::new(
                    look,
                    width,
                    height,
                    self.screen_height,
                    self.buffer_scale,
                    true,
                ),
            });
        }
        let job = self.job.as_mut().expect("a job was just started");
        let canvas = &mut self.canvases[slot_of(job.index)];
        match job.raster.next_band(&mut self.band) {
            Some(row) => {
                let buffer = canvas.buffer.as_ref().expect("a job has a buffer");
                screen.write_row(buffer, row, &self.band)?;
            }
            None => {
                canvas.drawn = Some(job.index);
                self.timeline.mark_drawn(job.index);
                self.job = None;
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Time between two refreshes of the model's output, and the client's shortest time between
    /// two frames.
    const REFRESH: f64 = 1.0 / 30.0;
    /// How long cosmic-comp 1.11 waits before it sends a frame callback to a surface that does
    /// not show on screen (`THROTTLE` in its `Common::send_frames`).
    const THROTTLE: f64 = 0.995;
    const OPAQUE: u32 = u32::MAX;

    /// The state of a surface that a commit carries.
    #[derive(Clone, Copy, Default)]
    struct State {
        /// A buffer to attach, or `Some(None)` to detach.
        attach: Option<Option<u32>>,
        alpha: Option<u32>,
        frame: bool,
    }

    impl State {
        /// Moves this state into `into`, like a commit does.
        fn merge_into(&mut self, into: &mut State) {
            let state = std::mem::take(self);
            into.attach = state.attach.or(into.attach);
            into.alpha = state.alpha.or(into.alpha);
            into.frame |= state.frame;
        }
    }

    #[derive(Default)]
    struct ModelSurface {
        pending: State,
        /// State of a synchronized subsurface that waits for the window's commit.
        cached: State,
        buffer: Option<u32>,
        alpha: Option<u32>,
        /// A frame callback waits to be sent.
        frame: bool,
        last_frame: f64,
    }

    /// The parts of cosmic-comp 1.11 and smithay that the stage depends on.
    ///
    /// - A synchronized subsurface's state applies with its parent's commit.
    /// - A buffer is released once another buffer replaces it or the surface detaches it
    ///   (`RendererSurfaceState::update_buffer`).
    /// - A surface whose element is completely hidden behind opaque regions is not presented and
    ///   loses its primary scanout output (`OutputDamageTracker` and
    ///   `PrimaryScanoutOutput::update_from_render_element_states`). It then gets a frame
    ///   callback only when the last one is [`THROTTLE`] old (`send_frames_surface_tree`).
    /// - A surface is opaque only at full alpha (`WaylandSurfaceRenderElement::opaque_regions`).
    #[derive(Default)]
    struct Model {
        time: f64,
        window: ModelSurface,
        slots: [ModelSurface; SLOTS],
        top: usize,
        next_buffer: u32,
        alive: Vec<u32>,
        /// Buffers that were attached at some time.
        shown: Vec<u32>,
        /// Slots whose release events the client has not read yet.
        released: Vec<usize>,
    }

    impl Model {
        fn commit_window(&mut self) {
            let window = &mut self.window;
            window.frame |= std::mem::take(&mut window.pending.frame);
            for (number, slot) in self.slots.iter_mut().enumerate() {
                let state = std::mem::take(&mut slot.cached);
                if let Some(buffer) = state.attach {
                    if slot.buffer.is_some() && slot.buffer != buffer {
                        self.released.push(number);
                    }
                    slot.buffer = buffer;
                }
                slot.alpha = state.alpha.or(slot.alpha);
                slot.frame |= state.frame;
            }
        }

        fn opaque(&self, slot: usize) -> bool {
            let slot = &self.slots[slot];
            slot.buffer.is_some() && slot.alpha.unwrap_or(OPAQUE) == OPAQUE
        }

        /// One refresh of the output. Returns whether a frame callback was sent.
        fn refresh(&mut self) -> bool {
            self.time += REFRESH;
            let below = (self.top + 1) % SLOTS;
            let hidden = [
                self.opaque(0) || self.opaque(1),
                below == 0 && self.opaque(self.top),
                below == 1 && self.opaque(self.top),
            ];
            let mut sent = false;
            let surfaces = std::iter::once(&mut self.window).chain(&mut self.slots);
            for (surface, hidden) in surfaces.zip(hidden) {
                // A surface without a buffer is unmapped and gets no frame callback at all.
                if surface.buffer.is_none() {
                    continue;
                }
                let overdue = self.time - surface.last_frame >= THROTTLE;
                if surface.frame && (!hidden || overdue) {
                    surface.frame = false;
                    surface.last_frame = self.time;
                    sent = true;
                }
            }
            sent
        }

        fn attached(&self, buffer: u32) -> bool {
            self.slots.iter().any(|slot| {
                slot.buffer == Some(buffer)
                    || slot.cached.attach == Some(Some(buffer))
                    || slot.pending.attach == Some(Some(buffer))
            })
        }
    }

    impl Screen for Model {
        type Buffer = u32;

        fn create_buffer(&mut self, _: usize, _: usize, _: usize) -> io::Result<u32> {
            self.next_buffer += 1;
            self.alive.push(self.next_buffer);
            assert!(
                self.alive.len() <= SLOTS,
                "{} buffers are alive",
                self.alive.len()
            );
            Ok(self.next_buffer)
        }

        fn write_row(&mut self, buffer: &u32, _: usize, _: &[u8]) -> io::Result<()> {
            assert!(
                !self.shown.contains(buffer),
                "a row was written to a buffer the compositor has seen"
            );
            Ok(())
        }

        fn destroy_buffer(&mut self, buffer: u32) {
            assert!(!self.attached(buffer), "an attached buffer was destroyed");
            self.alive.retain(|&alive| alive != buffer);
        }

        fn attach(&mut self, slot: usize, buffer: Option<&u32>) {
            self.shown.extend(buffer);
            self.slots[slot].pending.attach = Some(buffer.copied());
        }

        fn set_source(&mut self, _: usize, _: Source) {}

        fn set_alpha(&mut self, slot: usize, multiplier: u32) {
            self.slots[slot].pending.alpha = Some(multiplier);
        }

        fn place_above(&mut self, slot: usize, below: usize) {
            assert_ne!(slot, below);
            self.top = slot;
        }

        fn request_frame(&mut self, surface: Surface) {
            match surface {
                Surface::Window => self.window.pending.frame = true,
                Surface::Slot(slot) => self.slots[slot].pending.frame = true,
            }
        }

        fn commit(&mut self, slot: usize) {
            let slot = &mut self.slots[slot];
            slot.pending.merge_into(&mut slot.cached);
        }
    }

    /// Runs the client's loop against the model for `seconds` and returns the index of the
    /// keyframe on top. A frame starts only after a frame callback of the frame before, and it
    /// draws at most 20 rows of the next keyframe.
    fn run(seconds: f64) -> u64 {
        let mut model = Model {
            top: SLOTS - 1,
            ..Model::default()
        };
        model.window.buffer = Some(0);
        let mut stage: Stage<u32> = Stage::new(0x5eed, [320, 180]);
        let mut frame_pending = false;
        let mut last_frame: Option<f64> = None;
        while model.time < seconds {
            for slot in std::mem::take(&mut model.released) {
                stage.released(slot);
            }
            if !frame_pending {
                let elapsed = last_frame.map_or(0.0, |last| model.time - last);
                last_frame = Some(model.time);
                let mut rows = 0;
                let more = || {
                    rows += 1;
                    rows < 20
                };
                stage
                    .frame(elapsed, &mut model, more)
                    .expect("the model has no errors");
                model.commit_window();
                frame_pending = true;
                stage
                    .draw_first(&mut model)
                    .expect("the model has no errors");
            }
            if model.refresh() {
                frame_pending = false;
            }
        }
        let top = stage.timeline().layers().last().expect("a keyframe shows");
        for layer in stage.timeline().layers() {
            assert!(
                model.slots[layer.slot].buffer.is_some(),
                "keyframe {} shows without a buffer",
                layer.index
            );
        }
        top.index
    }

    // A keyframe starts every 10 s, so the fifth one is on top after 45 s. With the frame
    // callback on the window alone, the model sends one a second once the first keyframe is
    // opaque, each frame moves the timeline by a tenth of a second at most, and the first
    // keyframe is still on top.
    #[test]
    fn keyframes_change_on_time_against_the_compositor() {
        assert_eq!(run(45.0), 4);
    }
}
