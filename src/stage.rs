//! The keyframe slots of the window: which buffer each one holds, what the compositor last heard
//! about it, and the drawing of the next keyframe.
//!
//! The stage talks to the compositor only through [`Screen`], so its tests drive it against a
//! model of the compositor.

use std::io;

use crate::{Raster, Timeline, buffer_layout};

/// Keyframe surfaces, and the most keyframe buffers alive at once.
pub const SLOTS: usize = 2;
/// Shortest time between two slices of drawing, in seconds.
const SLICE_INTERVAL: f64 = 1.0 / 30.0;

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

    fn set_alpha(&mut self, slot: usize, multiplier: u32);

    fn place_above(&mut self, slot: usize, below: usize);

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
    multiplier: Option<u32>,
}

impl<B> Default for Canvas<B> {
    fn default() -> Self {
        Self {
            buffer: None,
            busy: false,
            attached: false,
            drawn: None,
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
    /// Earliest time of the next slice of drawing.
    next_slice: f64,
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
            timeline: Timeline::new(seed),
            job: None,
            band: Vec::new(),
            top: SLOTS - 1,
            next_slice: 0.0,
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

    /// Shows the timeline at `time` in seconds and returns whether anything on screen changed,
    /// in which case the caller commits the window. Every commit makes the compositor repaint,
    /// so nothing is sent while the screen stays as it is.
    pub fn show<S: Screen<Buffer = B>>(&mut self, time: f64, screen: &mut S) -> bool {
        self.timeline.advance_to(time);

        let mut changed = false;
        let mut layers = [None; SLOTS];
        for (depth, layer) in self.timeline.layers().enumerate() {
            layers[layer.slot] = Some(layer);
            if depth > 0 && layer.slot != self.top {
                screen.place_above(layer.slot, self.top);
                self.top = layer.slot;
                changed = true;
            }
        }
        let drawing = self.job.as_ref().map(|job| slot_of(job.index));
        for ((number, canvas), layer) in self.canvases.iter_mut().enumerate().zip(layers) {
            let Some(layer) = layer else {
                if canvas.attached {
                    screen.attach(number, None);
                    screen.commit(number);
                    canvas.attached = false;
                    canvas.drawn = None;
                    canvas.multiplier = None;
                    changed = true;
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
            let mut commit = false;
            if !canvas.attached {
                let buffer = canvas
                    .buffer
                    .as_ref()
                    .expect("a drawn keyframe has a buffer");
                screen.attach(number, Some(buffer));
                canvas.attached = true;
                canvas.busy = true;
                commit = true;
            }
            let multiplier = (layer.alpha() * f64::from(u32::MAX)).round() as u32;
            if canvas.multiplier != Some(multiplier) {
                screen.set_alpha(number, multiplier);
                canvas.multiplier = Some(multiplier);
                commit = true;
            }
            if commit {
                screen.commit(number);
                changed = true;
            }
        }
        changed
    }

    /// Draws rows of the keyframe the timeline needs next while `more` allows, unless the last
    /// slice was less than [`SLICE_INTERVAL`] ago. It commits nothing.
    pub fn draw<S: Screen<Buffer = B>>(
        &mut self,
        time: f64,
        screen: &mut S,
        mut more: impl FnMut() -> bool,
    ) -> io::Result<()> {
        if time < self.next_slice {
            return Ok(());
        }
        let mut drew = false;
        while self.draw_row(screen)? {
            drew = true;
            if !more() {
                break;
            }
        }
        if drew {
            self.next_slice = time + SLICE_INTERVAL;
        }
        Ok(())
    }

    /// Draws the whole first keyframe at once, so the screen is black only for as long as that
    /// takes.
    pub fn draw_first<S: Screen<Buffer = B>>(&mut self, screen: &mut S) -> io::Result<()> {
        while self.timeline.wanted() == Some(0) && self.draw_row(screen)? {}
        Ok(())
    }

    /// When to call [`Stage::show`] and [`Stage::draw`] again, if nothing arrives from the
    /// compositor before. `None` means only the compositor can end the wait, with the release
    /// of a buffer.
    pub fn next_wake(&self) -> Option<f64> {
        let time = self.timeline.time();
        let can_draw = self.job.is_some()
            || self.timeline.wanted().is_some_and(|index| {
                let canvas = &self.canvases[slot_of(index)];
                !canvas.busy && !canvas.attached
            });
        let drawing = if can_draw {
            Some(self.next_slice.max(time))
        } else {
            self.timeline.next_drawing().filter(|&at| at > time)
        };
        match (self.timeline.next_change(), drawing) {
            (Some(change), Some(drawing)) => Some(change.min(drawing)),
            (change, drawing) => change.or(drawing),
        }
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
    use crate::FADE_STEPS;

    /// The state of a synchronized subsurface that a commit carries.
    #[derive(Clone, Copy, Default)]
    struct State {
        /// A buffer to attach, or `Some(None)` to detach.
        attach: Option<Option<u32>>,
        alpha: Option<u32>,
    }

    #[derive(Default)]
    struct ModelSurface {
        pending: State,
        /// State that waits for the window's commit.
        cached: State,
        buffer: Option<u32>,
    }

    /// The parts of cosmic-comp 1.11 and smithay that the stage depends on.
    ///
    /// - A synchronized subsurface's state applies with its parent's commit.
    /// - A buffer is released once another buffer replaces it or the surface detaches it
    ///   (`RendererSurfaceState::update_buffer`).
    /// - Every commit schedules a render of the output (`CompositorHandler::commit`), so the
    ///   model counts every commit of the window as a repaint.
    #[derive(Default)]
    struct Model {
        slots: [ModelSurface; SLOTS],
        next_buffer: u32,
        alive: Vec<u32>,
        /// Buffers that were attached at some time.
        shown: Vec<u32>,
        /// Slots whose release events the client has not read yet.
        released: Vec<usize>,
        /// Times of the window's commits.
        repaints: Vec<f64>,
        /// A slot has requests that no commit carries yet.
        loose: bool,
    }

    impl Model {
        fn commit_window(&mut self, time: f64) {
            assert!(!self.loose, "a request waits for a commit of its slot");
            self.repaints.push(time);
            for (number, slot) in self.slots.iter_mut().enumerate() {
                let state = std::mem::take(&mut slot.cached);
                if let Some(buffer) = state.attach {
                    if slot.buffer.is_some() && slot.buffer != buffer {
                        self.released.push(number);
                    }
                    slot.buffer = buffer;
                }
            }
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
            self.loose = true;
        }

        fn set_alpha(&mut self, slot: usize, multiplier: u32) {
            self.slots[slot].pending.alpha = Some(multiplier);
            self.loose = true;
        }

        fn place_above(&mut self, slot: usize, below: usize) {
            assert_ne!(slot, below);
        }

        fn commit(&mut self, slot: usize) {
            let slot = &mut self.slots[slot];
            let state = std::mem::take(&mut slot.pending);
            slot.cached.attach = state.attach.or(slot.cached.attach);
            slot.cached.alpha = state.alpha.or(slot.cached.alpha);
            self.loose = false;
        }
    }

    /// Runs the client's loop against the model for `seconds` and returns the index of the
    /// keyframe on top and the times of the repaints. The client wakes only when the stage asks
    /// or a release arrives, commits the window only when the stage says the screen changed,
    /// and draws at most 20 rows of the next keyframe per slice.
    fn run(seconds: f64) -> (u64, Vec<f64>) {
        let mut model = Model::default();
        let mut stage: Stage<u32> = Stage::new(0x5eed, [320, 180]);
        let mut time = 0.0;
        while time < seconds {
            for slot in std::mem::take(&mut model.released) {
                stage.released(slot);
            }
            if stage.show(time, &mut model) {
                model.commit_window(time);
            }
            let mut rows = 0;
            let more = || {
                rows += 1;
                rows < 20
            };
            stage
                .draw(time, &mut model, more)
                .expect("the model has no errors");
            stage
                .draw_first(&mut model)
                .expect("the model has no errors");
            if !model.released.is_empty() {
                continue;
            }
            let wake = stage.next_wake().expect("the client would sleep for ever");
            time = time.max(wake);
        }
        let top = stage.timeline().layers().last().expect("a keyframe shows");
        for layer in stage.timeline().layers() {
            assert!(
                model.slots[layer.slot].buffer.is_some(),
                "keyframe {} shows without a buffer",
                layer.index
            );
        }
        (top.index, model.repaints)
    }

    // A keyframe starts every 10 s, so the fifth one is on top after 45 s. Each of the five
    // fades is 30 repaints within a second, and nothing is committed between two fades. A client
    // that commits on every frame, as it did while the keyframes panned, repaints 300 times in
    // 10 s.
    #[test]
    fn keyframes_change_on_time_and_nothing_repaints_between_fades() {
        let (top, repaints) = run(45.0);
        assert_eq!(top, 4);
        let mut fades = [0u32; 5];
        for &time in &repaints {
            let fade = (time / 10.0).floor();
            assert!(
                time - fade * 10.0 < 1.0,
                "a repaint at {time:.3} s falls between two fades"
            );
            fades[fade as usize] += 1;
        }
        for (fade, &count) in fades.iter().enumerate() {
            assert!(
                (1..=FADE_STEPS).contains(&count),
                "fade {fade} took {count} repaints"
            );
        }
        assert_eq!(repaints.len() as u32, 5 * FADE_STEPS);
    }
}
