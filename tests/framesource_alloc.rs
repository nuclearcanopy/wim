//! Allocations per frame, counted rather than argued.
//!
//! In its own test binary on purpose. The only way to count
//! allocations is a `#[global_allocator]`, and that is per *binary*:
//! in the lib's test binary it would wrap the allocator for every
//! other test in the crate and the counters would be a race between
//! whatever happened to run in parallel. Here it wraps one test.
//!
//! What it pins is the second and third suspects the frame-budget
//! roadmap item names: reallocating the output buffer per frame, and
//! per-pixel access instead of a contiguous walk. The read lands in a
//! pre-sized buffer, completing a frame swaps two pointers, and
//! `ui::raster`'s scratch is thread-local and kept, so the steady
//! state has nowhere to allocate. See docs/video.md.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use wim::framesource::{FrameSource, Grid, Pull};
use wim::ui::raster::Tone;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

/// Forwards everything to `System` and counts on the way through, so
/// the measurement cannot change what is being measured beyond one
/// relaxed add.
struct Counting;

// SAFETY: every method delegates to `System` with the layout it was
// handed, unchanged; the counters are the only addition.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new, Ordering::Relaxed);
        System.realloc(ptr, layout, new)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A generated source, so there is no file to make and none to clean
/// up. 500 frames.
const LAVFI: &str = "av://lavfi:testsrc2=size=320x240:rate=25:duration=20";
const FPS: f64 = 25.0;

fn wait_frame(src: &mut FrameSource) -> Pull {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match src.pull().expect("pull") {
            Pull::Pending if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            other => return other,
        }
    }
}

/// Reading a frame and painting it allocates exactly nothing once the
/// first frame has been through.
///
/// The one test in this binary, so the global counters cannot be
/// polluted by a test running beside it.
#[test]
fn a_frame_costs_no_allocations_at_all() {
    for (cols, rows) in [(40u16, 20u16), (160, 48)] {
        let grid = Grid::new(cols, rows, 2.0);
        let area = Rect::new(0, 0, cols, rows);
        let mut buf = Buffer::empty(area);
        let mut src =
            FrameSource::spawn(Path::new(LAVFI), grid, 0.0, FPS).expect("mpv must be on PATH");

        // the warm-up is where the allocations legitimately are: the
        // two frame buffers, the fifo path, mpv's argv, and `raster`'s
        // thread-local scratch sizing itself to this grid.
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        src.paint(area, Tone::Mono, &mut buf);
        src.paint(area, Tone::Color, &mut buf);

        let frames = 50;
        let (allocs, bytes) = (
            ALLOCS.load(Ordering::Relaxed),
            BYTES.load(Ordering::Relaxed),
        );
        for _ in 0..frames {
            assert_eq!(wait_frame(&mut src), Pull::Frame);
            src.paint(area, Tone::Mono, &mut buf);
        }
        let allocs = ALLOCS.load(Ordering::Relaxed) - allocs;
        let bytes = BYTES.load(Ordering::Relaxed) - bytes;

        // printed as well as asserted: a non-zero number is more
        // useful than a bare failure.
        println!(
            "{cols}x{rows}: {allocs} allocations and {bytes} bytes over {frames} frames \
             ({:.2}/frame)",
            allocs as f64 / f64::from(frames)
        );
        assert_eq!(
            allocs, 0,
            "{cols}x{rows} allocated {allocs} times ({bytes} bytes) over {frames} frames"
        );
    }
}
