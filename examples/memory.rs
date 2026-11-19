//! Measures the memory the screensaver holds at a 5120x2160 screen without showing anything. It
//! connects to the compositor and binds `wl_shm` but creates no surface, then allocates and
//! draws keyframes exactly as the program does: one memfd and one `wl_buffer` per keyframe, with
//! the second keyframe drawn while the first is shown and the first freed afterwards.
//!
//! The program writes keyframes with `pwrite` and never maps them, so their pages are missing
//! from its resident set. The total printed here is the resident set plus the memfd pages.
//!
//! Usage: `cargo run --release --example memory`

use std::error::Error;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::{FileExt, MetadataExt};

use rustix::fs::{MemfdFlags, memfd_create};
use screensaver::{Raster, Timeline, buffer_layout};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};

const SCREEN: [u32; 2] = [5120, 2160];
const SEED: u64 = 0x5eed;
/// Megabytes are decimal, like the sizes in the documentation.
const MB: f64 = 1e6;

struct App;

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

delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);

struct Wayland {
    queue: EventQueue<App>,
    shm: wl_shm::WlShm,
}

struct Keyframe {
    file: File,
    buffer: Option<wl_buffer::WlBuffer>,
}

impl Drop for Keyframe {
    fn drop(&mut self) {
        if let Some(buffer) = &self.buffer {
            buffer.destroy();
        }
    }
}

/// A value of `/proc/self/status` in bytes.
fn status(key: &str) -> u64 {
    let text = std::fs::read_to_string("/proc/self/status").expect("procfs is mounted");
    let line = text
        .lines()
        .find(|line| line.starts_with(key))
        .expect("the key exists");
    let kilobytes: u64 = line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .expect("a size in kB");
    kilobytes * 1024
}

fn report(stage: &str, keyframes: &[&Keyframe]) -> std::io::Result<f64> {
    let mut shared = 0;
    for keyframe in keyframes {
        shared += keyframe.file.metadata()?.blocks() * 512;
    }
    let total = (status("VmRSS:") + shared) as f64 / MB;
    println!(
        "{stage:<44} total {total:6.2} MB = heap and stack {:5.2} + binary and libraries {:5.2} + keyframes {:5.2}",
        status("RssAnon:") as f64 / MB,
        (status("RssFile:") + status("RssShmem:")) as f64 / MB,
        shared as f64 / MB,
    );
    Ok(total)
}

fn draw(
    wayland: &mut Option<Wayland>,
    timeline: &Timeline,
    index: u64,
    size: [usize; 2],
    scale: f64,
) -> Result<Keyframe, Box<dyn Error>> {
    let [width, height] = size;
    let length = width * height * 4;
    let file = File::from(memfd_create("screensaver", MemfdFlags::CLOEXEC)?);
    file.set_len(length as u64)?;
    let buffer = match wayland {
        Some(wayland) => {
            let qh = wayland.queue.handle();
            let pool = wayland
                .shm
                .create_pool(file.as_fd(), length as i32, &qh, ());
            let buffer = pool.create_buffer(
                0,
                width as i32,
                height as i32,
                (width * 4) as i32,
                wl_shm::Format::Xrgb8888,
                &qh,
                (),
            );
            pool.destroy();
            wayland.queue.roundtrip(&mut App)?;
            Some(buffer)
        }
        None => None,
    };
    let mut raster = Raster::new(timeline.look(index), width, height, SCREEN[1], scale, true);
    let mut band = Vec::new();
    while let Some(row) = raster.next_band(&mut band) {
        file.write_all_at(&band, (row * width * 4) as u64)?;
    }
    Ok(Keyframe { file, buffer })
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut peak = report("at start", &[])?;
    let mut wayland = match Connection::connect_to_env() {
        Ok(conn) => {
            let (globals, queue) = registry_queue_init::<App>(&conn)?;
            let shm = globals.bind(&queue.handle(), 1..=1, ())?;
            Some(Wayland { queue, shm })
        }
        Err(error) => {
            println!("no compositor ({error}), so the Wayland client's heap is left out");
            None
        }
    };
    peak = peak.max(report("connected, no surface", &[])?);

    let (buffer, scale) = buffer_layout(SCREEN);
    let size = buffer.map(|s| s as usize);
    let timeline = Timeline::new(SEED);
    println!(
        "keyframe buffers are {}x{} XRGB8888, {scale:.3} screen pixels per buffer pixel",
        size[0], size[1]
    );

    let first = draw(&mut wayland, &timeline, 0, size, scale)?;
    peak = peak.max(report("one keyframe shown", &[&first])?);
    let second = draw(&mut wayland, &timeline, 1, size, scale)?;
    peak = peak.max(report("second keyframe drawn (peak)", &[&first, &second])?);
    drop(first);
    if let Some(wayland) = &mut wayland {
        wayland.queue.roundtrip(&mut App)?;
    }
    peak = peak.max(report("first keyframe freed", &[&second])?);
    println!(
        "peak total {peak:.2} MB, highest resident set {:.2} MB",
        status("VmHWM:") as f64 / MB
    );
    Ok(())
}
