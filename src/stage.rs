//! The keyframe on screen, the one being drawn, and the buffers that hold them.
//!
//! The stage talks to the compositor only through [`Screen`], so its tests drive it against a
//! model of the compositor.

use std::io;

use crate::{Raster, Timeline, buffer_layout};

/// The requests the stage sends to the compositor.
pub trait Screen {
    /// A keyframe's memory and the buffer that shows it to the compositor.
    type Buffer;

    /// A buffer of `width` by `height` XRGB8888 pixels. Its release reports `keyframe`.
    fn create_buffer(
        &mut self,
        keyframe: u64,
        width: usize,
        height: usize,
    ) -> io::Result<Self::Buffer>;

    /// Writes one row of a buffer that was never attached.
    fn write_row(&mut self, buffer: &Self::Buffer, row: usize, pixels: &[u8]) -> io::Result<()>;

    fn destroy_buffer(&mut self, buffer: Self::Buffer);

    /// Attaches a buffer to the window in place of the last one, with damage over all of it.
    /// It takes effect with the next commit of the window.
    fn attach(&mut self, buffer: &Self::Buffer);
}

/// A keyframe being drawn into its buffer, row by row.
struct Job<B> {
    index: u64,
    raster: Raster,
    buffer: B,
}

/// The keyframes of one screen size: their timeline, their buffers and the next one's drawing.
///
/// One buffer is on screen. A second one exists only while the next keyframe is drawn, and until
/// the compositor releases the buffer it replaced.
pub struct Stage<B> {
    screen_height: u32,
    buffer: [usize; 2],
    /// Screen pixels per buffer pixel.
    buffer_scale: f64,
    timeline: Timeline,
    /// The buffer on screen.
    shown: Option<B>,
    /// The buffer that left the screen, its keyframe, and whether the compositor released it.
    leaving: Option<(u64, B, bool)>,
    job: Option<Job<B>>,
    band: Vec<u8>,
}

impl<B> Stage<B> {
    /// A stage for a physical screen size.
    pub fn new(seed: u64, screen: [u32; 2]) -> Self {
        let (buffer, buffer_scale) = buffer_layout(screen);
        Self {
            screen_height: screen[1],
            buffer: buffer.map(|s| s as usize),
            buffer_scale,
            timeline: Timeline::new(seed),
            shown: None,
            leaving: None,
            job: None,
            band: Vec::new(),
        }
    }

    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// Records that the compositor released the buffer of `keyframe`.
    pub fn released(&mut self, keyframe: u64) {
        if let Some((leaving, _, released)) = &mut self.leaving
            && *leaving == keyframe
        {
            *released = true;
        }
    }

    /// Does what is due at `time` in seconds: frees a released buffer, and draws the next
    /// keyframe while `more` allows. Returns whether a new keyframe was attached, in which case
    /// the caller commits the window. That commit is the only one a keyframe costs, and every
    /// commit makes the compositor repaint.
    pub fn wake<S: Screen<Buffer = B>>(
        &mut self,
        time: f64,
        screen: &mut S,
        mut more: impl FnMut() -> bool,
    ) -> io::Result<bool> {
        if let Some((_, _, true)) = self.leaving
            && let Some((_, buffer, _)) = self.leaving.take()
        {
            screen.destroy_buffer(buffer);
        }
        if self.job.is_none() {
            // The next buffer waits for the last one to go, so there are never three.
            if time < self.timeline.due() || self.leaving.is_some() {
                return Ok(false);
            }
            let index = self.timeline.next();
            let [width, height] = self.buffer;
            self.job = Some(Job {
                index,
                raster: Raster::new(
                    self.timeline.look(index),
                    width,
                    height,
                    self.screen_height,
                    self.buffer_scale,
                    true,
                ),
                buffer: screen.create_buffer(index, width, height)?,
            });
        }
        let job = self.job.as_mut().expect("a job was just started");
        while let Some(row) = job.raster.next_band(&mut self.band) {
            screen.write_row(&job.buffer, row, &self.band)?;
            if !more() {
                return Ok(false);
            }
        }
        let job = self.job.take().expect("the job is done");
        screen.attach(&job.buffer);
        if let Some(old) = self.shown.replace(job.buffer) {
            self.leaving = Some((job.index.wrapping_sub(1), old, false));
        }
        self.timeline.show(time);
        Ok(true)
    }

    /// When to call [`Stage::wake`] again after a call at `time`, if nothing arrives from the
    /// compositor before. `None` means only the compositor can end the wait, with the release
    /// of a buffer.
    pub fn next_wake(&self, time: f64) -> Option<f64> {
        match (&self.job, &self.leaving) {
            (Some(_), _) | (None, Some((_, _, true))) => Some(time),
            (None, Some(_)) => None,
            (None, None) => Some(self.timeline.due()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parts of cosmic-comp 1.11 and smithay that the stage depends on.
    ///
    /// - A buffer is released once another buffer replaces it
    ///   (`RendererSurfaceState::update_buffer`).
    /// - Every commit schedules a render of the output (`CompositorHandler::commit`), so the
    ///   model counts every commit of the window as a repaint.
    #[derive(Default)]
    struct Model {
        /// The buffer that waits for the window's commit.
        pending: Option<u64>,
        buffer: Option<u64>,
        alive: Vec<u64>,
        /// Buffers that were attached at some time.
        shown: Vec<u64>,
        /// Release events the client has not read yet.
        released: Vec<u64>,
        /// Times of the window's commits.
        repaints: Vec<f64>,
    }

    impl Model {
        fn commit_window(&mut self, time: f64) {
            self.repaints.push(time);
            if let Some(buffer) = self.pending.take() {
                self.released.extend(self.buffer.replace(buffer));
            }
        }
    }

    impl Screen for Model {
        type Buffer = u64;

        fn create_buffer(&mut self, keyframe: u64, _: usize, _: usize) -> io::Result<u64> {
            self.alive.push(keyframe);
            assert!(
                self.alive.len() <= 2,
                "{} buffers are alive",
                self.alive.len()
            );
            Ok(keyframe)
        }

        fn write_row(&mut self, buffer: &u64, _: usize, _: &[u8]) -> io::Result<()> {
            assert!(
                !self.shown.contains(buffer),
                "a row was written to a buffer the compositor has seen"
            );
            Ok(())
        }

        fn destroy_buffer(&mut self, buffer: u64) {
            assert!(
                self.buffer != Some(buffer) && self.pending != Some(buffer),
                "an attached buffer was destroyed"
            );
            self.alive.retain(|&alive| alive != buffer);
        }

        fn attach(&mut self, buffer: &u64) {
            self.shown.push(*buffer);
            self.pending = Some(*buffer);
        }
    }

    /// Runs the client's loop against the model for `seconds` and returns the model. The client
    /// wakes only when the stage asks or a release arrives, commits the window only when the
    /// stage attached a keyframe, and draws at most 50 rows per wake.
    fn run(seconds: f64) -> Model {
        let mut model = Model::default();
        let mut stage: Stage<u64> = Stage::new(0x5eed, [320, 180]);
        let mut time = 0.0;
        let mut wakes = 0;
        while time < seconds {
            for keyframe in std::mem::take(&mut model.released) {
                stage.released(keyframe);
            }
            let mut rows = 0;
            let more = || {
                rows += 1;
                rows < 50
            };
            if stage
                .wake(time, &mut model, more)
                .expect("the model has no errors")
            {
                model.commit_window(time);
            }
            if !model.released.is_empty() {
                continue;
            }
            let wake = stage
                .next_wake(time)
                .expect("the client would sleep for ever");
            if wake > time {
                wakes += 1;
                assert_eq!(
                    model.alive.len(),
                    1,
                    "a second buffer is held between keyframes"
                );
            }
            time = time.max(wake);
        }
        assert_eq!(wakes, 5, "the client slept {wakes} times");
        assert_eq!(model.buffer, stage.timeline().shown());
        model
    }

    // A keyframe replaces the last one every 10 s in one commit, so 45 s see five commits and
    // nothing else. A fade through 30 alpha levels is 30 commits per keyframe.
    #[test]
    fn each_keyframe_costs_one_repaint_and_nothing_repaints_in_between() {
        let model = run(45.0);
        assert_eq!(model.buffer, Some(4));
        assert_eq!(model.repaints, [0.0, 10.0, 20.0, 30.0, 40.0]);
    }
}
