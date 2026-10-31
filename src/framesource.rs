//! The frame source an ascii video renderer needs: a second, headless
//! mpv in encode mode writing pre-scaled packed rgb24 to a fifo.
//!
//! No frame crosses an ipc socket (`screenshot-raw` cannot, and that is
//! settled) and the audible instance is never touched. This is a
//! throwaway decoder whose only job is to hand over exactly
//! `w * h * 3` bytes per frame at the size the cell grid wants, with
//! mpv doing the downscale: letting mpv scale is what took the
//! per-frame cost down an order of magnitude.
//!
//! **The fifo is the clock.** mpv blocks in `write(2)` once the pipe
//! fills, so the rate this is read at is the rate mpv decodes at. Pause
//! is "stop reading" and has no special case anywhere below. The reads
//! are non-blocking, so the ui thread never waits on the decoder
//! either: a `pull` takes what has arrived and says `Pending`.
//!
//! Sync is read scheduling rather than seeking: frame `n` is media time
//! `start + n / fps`, catching up is read-and-discard and waiting is
//! not reading, so there is no drift to correct. See `Schedule`.
//!
//! Read docs/video.md before changing the filter chain or the read
//! loop. Both have measurements behind them, and the read loop has one
//! fifo behaviour that is not guessable.

use std::ffi::{CString, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use image::{DynamicImage, RgbImage};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::ui::raster::{self, Fit, Style, Tone};

/// Bytes per pixel. Packed rgb24: no padding and no stride, so a frame
/// is exactly `w * h * 3` bytes and nothing has to interpret a layout.
pub const BPP: usize = 3;

/// Samples per cell, per axis. `ui::raster` is a 2x2 quadrant
/// rasterizer, so a `cols x rows` cell rect wants a `2cols x 2rows`
/// pixel frame; mpv is asked for exactly that, which makes
/// `Fit::Stretch` a 1:1 copy with no resampling left to do.
pub const SAMPLES: u32 = 2;

/// Cell height over cell width, for a terminal that will not say.
/// `graphics::cell_aspect` is the real one.
pub const CELL_ASPECT: f32 = 2.0;

/// What a missing `container-fps` becomes. The property reads fine over
/// the existing socket for ordinary files but is 0 for some containers,
/// and a zero here would divide every frame time by it.
pub const FALLBACK_FPS: f64 = 25.0;

/// How far behind the picture may fall before respawning beats reading
/// through the gap.
///
/// A restart costs 180-550ms of black; read-and-discard runs at the
/// decoder's ceiling, so closing a `g`-second gap in 30fps content
/// costs about `0.4 * g` seconds. Break-even against a 350ms restart is
/// ~0.9s, and a catch-up at least keeps the picture moving where a
/// restart blanks it, so round up.
const MAX_CATCHUP: f64 = 1.0;

/// Refuses a cell grid whose frame would be larger than this. A u16
/// cell count can ask for a 131070-pixel axis, which is not a terminal,
/// it is an arithmetic accident; 16MiB is 2048x2730 pixels.
const MAX_FRAME_BYTES: usize = 16 << 20;

/// Slack, in frames, on the floor that turns a media time into a frame
/// index.
///
/// A host drives this off `time-pos`, which lands on exact frame
/// boundaries, and `7.0 / 25.0 * 25.0` is `6.999999999999999`. Floored
/// without this, that media time names the frame already on screen, so
/// nothing is read, so `time-pos` never moves: a permanent stall at
/// whichever frame rounds down first (frame 29, at 25fps). A millionth
/// of a frame is far below the one-frame resolution the whole scheme
/// has anyway.
const FRAME_EPS: f64 = 1e-6;

/// Distinguishes concurrent sources' temp dirs within one process; the
/// pid alone is not enough once two panes or two tests have one each.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The cell rect a frame is decoded for, and the terminal's cell shape.
///
/// Both are needed because the letterbox has to be computed against the
/// *physical* shape of the rect: a cell is about twice as tall as it is
/// wide, so a `cols x rows` rect is `cols : rows * cell_aspect`, not
/// `cols : rows`. Getting that wrong stretches the picture vertically
/// by the cell aspect, which is a 2x error, not a rounding one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cell_aspect: f32,
}

impl Grid {
    /// A nonsense `cell_aspect` (zero, negative, NaN, from a terminal
    /// that reported a zero font size) becomes `CELL_ASPECT` rather
    /// than propagating into the filter chain as a zero height.
    pub fn new(cols: u16, rows: u16, cell_aspect: f32) -> Grid {
        let cell_aspect = if cell_aspect.is_finite() && cell_aspect > 0.0 {
            cell_aspect
        } else {
            CELL_ASPECT
        };
        Grid {
            cols,
            rows,
            cell_aspect,
        }
    }

    /// The grid for a laid-out rect. Position is irrelevant; only the
    /// size reaches mpv.
    pub fn for_area(area: Rect, cell_aspect: f32) -> Grid {
        Grid::new(area.width, area.height, cell_aspect)
    }

    /// The frame size handed to mpv, in pixels.
    pub fn pixels(self) -> (u32, u32) {
        (
            u32::from(self.cols) * SAMPLES,
            u32::from(self.rows) * SAMPLES,
        )
    }

    /// Exactly how many bytes one frame is.
    pub fn frame_len(self) -> usize {
        let (w, h) = self.pixels();
        w as usize * h as usize * BPP
    }

    /// The square-pixel box the picture is letterboxed inside before
    /// the final squash: the sample grid with its height multiplied by
    /// the cell aspect, i.e. the sample grid's real-world shape.
    fn fit_box(self) -> (u32, u32) {
        let (w, h) = self.pixels();
        let tall = (f64::from(h) * f64::from(self.cell_aspect)).round();
        (w, (tall as u32).max(1))
    }

    /// mpv's `--vf`. Two scales, not one, and the second is the whole
    /// point: fit the picture inside the sample grid's *physical* shape
    /// (`fit_box`), pad it to that, then squash by exactly the cell
    /// aspect to land on the sample grid. A single
    /// `scale=2cols:2rows:force_original_aspect_ratio=decrease` would
    /// letterbox for square pixels and come out stretched on screen.
    /// `pad` is what keeps every frame the same size whatever the
    /// source aspect is, so the reader never renegotiates.
    fn vf(self) -> String {
        let (w, h) = self.pixels();
        let (bw, bh) = self.fit_box();
        format!(
            "scale={bw}:{bh}:force_original_aspect_ratio=decrease,\
             pad={bw}:{bh}:-1:-1:color=black,\
             scale={w}:{h},format=rgb24"
        )
    }
}

/// What showing media time `now` costs the reader.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// The frame already on screen is the right one: read nothing.
    /// Pause is this, with no special case for it.
    Hold,
    /// Read this many frames; the last is the one to show and the rest
    /// are discarded. Catching up is reading, never seeking.
    Advance(u32),
    /// Wider than reading can close: respawn at this media time.
    Restart(f64),
}

/// Where the frame instance is in the stream, and what to do about it.
///
/// The frame instance has no clock of its own, which is why it cannot
/// drift: frame `n` *is* media time `start + n / fps`, so "which frame
/// should be on screen" is arithmetic against the audible instance's
/// `time-pos`, which wim already observes.
#[derive(Debug, Clone, Copy)]
pub struct Schedule {
    start: f64,
    fps: f64,
    served: u64,
}

impl Schedule {
    /// `start` is snapped down onto the frame grid, because mpv's
    /// `--start=t` under `--hr-seek=yes` lands on `floor(t * fps)`.
    /// Left unsnapped, every frame time below is wrong by up to one
    /// frame for the whole life of the instance.
    pub fn new(start: f64, fps: f64) -> Schedule {
        let fps = if fps.is_finite() && fps > 0.0 {
            fps
        } else {
            FALLBACK_FPS
        };
        let start = if start.is_finite() && start > 0.0 {
            (start * fps).floor() / fps
        } else {
            0.0
        };
        Schedule {
            start,
            fps,
            served: 0,
        }
    }

    pub fn start(self) -> f64 {
        self.start
    }

    pub fn fps(self) -> f64 {
        self.fps
    }

    /// Frames handed over so far, i.e. the index of the next one.
    pub fn served(self) -> u64 {
        self.served
    }

    /// Media time of frame `n` of this instance.
    pub fn frame_time(self, n: u64) -> f64 {
        self.start + n as f64 / self.fps
    }

    /// Media time of the frame not yet read; where a respawn resumes.
    pub fn next_time(self) -> f64 {
        self.frame_time(self.served)
    }

    /// Media time of the newest frame handed over, or `None` before the
    /// first one.
    pub fn shown_time(self) -> Option<f64> {
        self.served.checked_sub(1).map(|n| self.frame_time(n))
    }

    /// What to do to show media time `now`.
    pub fn step(self, now: f64) -> Step {
        if !now.is_finite() {
            return Step::Hold;
        }
        let slack = (MAX_CATCHUP * self.fps).ceil().max(1.0);
        let target = ((now - self.start) * self.fps + FRAME_EPS).floor();
        let newest = self.served as f64 - 1.0;
        // seeked backwards past what holding can absorb. a pipe does
        // not rewind, so the only answer is a new instance.
        if target < newest - slack {
            return Step::Restart(now);
        }
        // the newest frame is already at or past `now`: a small
        // backwards seek, or simply ahead of the clock. holding is
        // right and costs nothing; restarting here would blank the
        // screen to move the picture a frame.
        if target <= newest {
            return Step::Hold;
        }
        let need = target - newest;
        if need > slack {
            return Step::Restart(now);
        }
        Step::Advance(need as u32)
    }

    fn count(&mut self) {
        self.served += 1;
    }
}

/// What one `pull` achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    /// A whole frame landed; `frame` is it.
    Frame,
    /// Nothing complete right now and mpv is alive. The ordinary
    /// answer, not an error: either mpv has not opened its end yet, or
    /// it is mid-write.
    Pending,
    /// mpv exited and the pipe is drained. Sticky.
    Ended,
}

/// A second, headless mpv decoding to a fifo, plus the read loop that
/// paces it.
///
/// Owns its child, its fifo and its temp dir, and tears all three down
/// on drop, including while unwinding from a panic. `src/term.rs`'s
/// guard discipline is the reason that matters: a panic restores the
/// terminal, and a frame instance left decoding into a dead pipe would
/// be the one thing that outlived it.
pub struct FrameSource {
    media: PathBuf,
    grid: Grid,
    dir: PathBuf,
    fifo: PathBuf,
    child: Option<Child>,
    pipe: Option<File>,
    /// The newest complete frame. `ImageRgb8` rather than a bare `Vec`
    /// only so `ui::raster` can be handed it without a copy; the bytes
    /// are the same packed rgb24 either way.
    front: DynamicImage,
    /// Where the frame being read lands, so a partial read never tears
    /// the frame on screen. Completing one swaps the two, which is a
    /// pointer move and the reason the steady state allocates nothing.
    back: DynamicImage,
    filled: usize,
    sched: Schedule,
    ended: bool,
}

impl FrameSource {
    /// Spawns a frame instance for `media`, decoding at `grid`, from
    /// media time `start`, with the audible instance's `container-fps`.
    pub fn spawn(media: &Path, grid: Grid, start: f64, fps: f64) -> io::Result<FrameSource> {
        let len = grid.frame_len();
        if len == 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "a cell grid with no cells has no frame to decode",
            ));
        }
        if len > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{len} bytes per frame is past MAX_FRAME_BYTES"),
            ));
        }
        let dir = std::env::temp_dir().join(format!(
            "wim-frames-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        let mut src = FrameSource {
            media: media.to_path_buf(),
            grid,
            fifo: dir.join("frames.rgb24"),
            dir,
            child: None,
            pipe: None,
            front: blank(grid),
            back: blank(grid),
            filled: 0,
            sched: Schedule::new(start, fps),
            ended: false,
        };
        // `src` is live by here, so a failure below still runs `Drop`
        // and takes the dir and any half-made fifo with it.
        src.open()?;
        Ok(src)
    }

    /// mkfifo, spawn, open the read end.
    fn open(&mut self) -> io::Result<()> {
        // a fresh fifo every time: one reused from a killed instance
        // can still hold that instance's bytes, which would splice a
        // stale partial frame onto the first frame after a seek.
        let _ = std::fs::remove_file(&self.fifo);
        mkfifo(&self.fifo)?;

        let mut cmd = Command::new("mpv");
        cmd.arg("--no-config")
            .arg("--no-terminal")
            .arg(format!("--vf={}", self.grid.vf()))
            // a frame source and nothing else: no audio device to fight
            // the audible instance over, no subtitles.
            .arg("--audio=no")
            .arg("--sub=no")
            // a pin, not a fix: mpv's default is already exact for
            // `--start` (measured, frame 161 of 300 for `--start=5.37`
            // at 30fps). `--hr-seek=no` lands on the previous keyframe
            // instead, which on a long-gop file is frame 0, a 5.4
            // second error; and a wrong `start` is a permanent sync
            // offset rather than a glitch, so it is worth naming.
            .arg("--hr-seek=yes")
            .arg("--of=rawvideo")
            .arg("--ovc=rawvideo")
            .arg("--ovcopts=pix_fmt=rgb24");
        // `--start=0` is still a seek, and a seek to zero drops the
        // first frame (measured); omitting it starts at frame 0.
        if self.sched.start() > 0.0 {
            cmd.arg(format!("--start={}", self.sched.start()));
        }
        let mut out = OsString::from("--o=");
        out.push(&self.fifo);
        cmd.arg(out).arg(&self.media);
        // all three, unlike the audible instance, which has to inherit
        // stdout so a drawing vo's escapes reach the terminal. this one
        // must never write to the terminal at all.
        self.child = Some(
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );

        // O_NONBLOCK on both counts: a blocking open of a fifo's read
        // end waits for a writer forever, and a blocking read would put
        // the ui thread behind the decoder.
        self.pipe = Some(
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.fifo)?,
        );
        self.filled = 0;
        Ok(())
    }

    pub fn grid(&self) -> Grid {
        self.grid
    }

    pub fn schedule(&self) -> Schedule {
        self.sched
    }

    /// The newest complete frame, as `ui::raster`'s input type. Black
    /// until the first `Pull::Frame`.
    pub fn frame(&self) -> &DynamicImage {
        &self.front
    }

    /// The same frame as packed rgb24, exactly `grid().frame_len()`
    /// bytes, row-major, no stride.
    pub fn frame_bytes(&self) -> &[u8] {
        raw(&self.front)
    }

    /// What to do to show media time `now`.
    pub fn step(&self, now: f64) -> Step {
        self.sched.step(now)
    }

    /// Takes whatever has arrived, and reports a whole frame the moment
    /// one completes. Never blocks.
    pub fn pull(&mut self) -> io::Result<Pull> {
        if self.ended {
            return Ok(Pull::Ended);
        }
        let len = self.grid.frame_len();
        loop {
            let read = {
                let Some(pipe) = self.pipe.as_mut() else {
                    self.ended = true;
                    return Ok(Pull::Ended);
                };
                pipe.read(&mut raw_mut(&mut self.back)[self.filled..])
            };
            match read {
                // the fifo behaviour that cost the prototype a
                // debugging cycle: with no writer attached yet, a read
                // returns `Ok(0)`, a clean eof, *not* `WouldBlock`. mpv
                // has not finished opening its end at this point, so
                // zero has to mean "wait"; child liveness, not the byte
                // count, is the only thing that says it is over.
                Ok(0) => {
                    if self.child_gone() {
                        self.ended = true;
                        return Ok(Pull::Ended);
                    }
                    return Ok(Pull::Pending);
                }
                Ok(n) => {
                    self.filled += n;
                    if self.filled == len {
                        self.filled = 0;
                        std::mem::swap(&mut self.front, &mut self.back);
                        self.sched.count();
                        return Ok(Pull::Frame);
                    }
                }
                // a writer is attached with nothing ready yet: the
                // other half of "zero means wait".
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(Pull::Pending),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Drives the source to the frame for media time `now`: the one
    /// call a host needs per ui frame.
    ///
    /// There is no pause branch. A paused player stops advancing
    /// `now`, so `step` says `Hold`, so nothing is read, so mpv blocks
    /// in `write(2)` and stops decoding.
    pub fn advance_to(&mut self, now: f64) -> io::Result<Pull> {
        match self.step(now) {
            Step::Hold if self.ended => Ok(Pull::Ended),
            Step::Hold => Ok(Pull::Pending),
            Step::Restart(at) => {
                self.restart(at)?;
                self.pull()
            }
            Step::Advance(want) => {
                let mut got = false;
                for _ in 0..want {
                    match self.pull()? {
                        Pull::Frame => got = true,
                        // a newer frame beats reporting the stall: the
                        // host should paint what did arrive.
                        _ if got => return Ok(Pull::Frame),
                        stalled => return Ok(stalled),
                    }
                }
                Ok(if got { Pull::Frame } else { Pull::Pending })
            }
        }
    }

    /// Respawns at media time `at`. The only thing a user seek can do,
    /// since encode mode has no socket to seek over; costs 180-550ms of
    /// black, which is why `Schedule::step` would rather read.
    pub fn restart(&mut self, at: f64) -> io::Result<()> {
        self.teardown();
        self.ended = false;
        self.sched = Schedule::new(at, self.sched.fps());
        self.open()
    }

    /// Follows a resized pane, resuming at the frame reading had got
    /// to. The scale is baked into `--vf` at spawn, so a new cell grid
    /// is a respawn; same cost as a seek, which is why this is not
    /// called for a grid that has not moved.
    pub fn resize(&mut self, grid: Grid) -> io::Result<()> {
        if grid == self.grid {
            return Ok(());
        }
        let len = grid.frame_len();
        if len == 0 || len > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "cell grid out of range",
            ));
        }
        let at = self.sched.next_time();
        self.grid = grid;
        // the one allocating operation in the module, and it is
        // per-resize rather than per-frame.
        self.front = blank(grid);
        self.back = blank(grid);
        self.restart(at)
    }

    /// Draws the current frame into `area`.
    ///
    /// `Fit::Stretch` is not a distortion: the frame is already the
    /// sample grid's size with mpv's own letterbox baked in, so
    /// stretching is a 1:1 copy and the rasterizer resamples nothing.
    /// An `area` that no longer matches the grid (mid-resize, before
    /// `resize` has respawned) scales the last frame to the new pane,
    /// which is the least-wrong interim picture.
    pub fn paint(&self, area: Rect, tone: Tone, buf: &mut Buffer) {
        let style = Style {
            fit: Fit::Stretch,
            tone,
        };
        raster::with_scratch(|r| {
            r.render(&self.front, area, self.grid.cell_aspect, style, buf);
        });
    }

    /// Whether mpv has exited. Reaps it in passing, so a finished
    /// instance never sits as a zombie.
    fn child_gone(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
            None => true,
        }
    }

    /// Kills this struct's own child by its own pid, closes the read
    /// end and removes the fifo. Never a pattern-matched kill, and
    /// never anything it did not spawn.
    ///
    /// Closing the read end is the second line of defence for the paths
    /// `Drop` does not reach (`abort`, `process::exit`): the kernel
    /// closes the fd at exit anyway, and a frame instance with no
    /// reader takes SIGPIPE on its next write. Measured: exit status
    /// 141. So a leaked instance is bounded by one pipe buffer of work.
    fn teardown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.pipe = None;
        let _ = std::fs::remove_file(&self.fifo);
        self.filled = 0;
    }
}

impl Drop for FrameSource {
    fn drop(&mut self) {
        self.teardown();
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// By hand rather than derived: a derive would print both frame
/// buffers, which at the grids this runs at is a megabyte of pixels per
/// line.
impl std::fmt::Debug for FrameSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameSource")
            .field("media", &self.media)
            .field("grid", &self.grid)
            .field("fifo", &self.fifo)
            .field("pid", &self.child.as_ref().map(Child::id))
            .field("filled", &self.filled)
            .field("sched", &self.sched)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

/// A black frame at `grid`'s size, and the allocation of both buffers.
fn blank(grid: Grid) -> DynamicImage {
    let (w, h) = grid.pixels();
    DynamicImage::ImageRgb8(RgbImage::new(w, h))
}

/// The frame buffers are built as `ImageRgb8` and only ever replaced by
/// another one, so both of these are infallible by construction.
/// Unreachable rather than an empty slice, which would spin the read
/// loop forever.
fn raw(img: &DynamicImage) -> &[u8] {
    match img {
        DynamicImage::ImageRgb8(b) => b,
        _ => unreachable!("a frame buffer is always ImageRgb8"),
    }
}

fn raw_mut(img: &mut DynamicImage) -> &mut [u8] {
    match img {
        DynamicImage::ImageRgb8(b) => b,
        _ => unreachable!("a frame buffer is always ImageRgb8"),
    }
}

/// `mkfifo(2)`; `std::fs` has no fifo.
fn mkfifo(path: &Path) -> io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "fifo path contains a nul byte"))?;
    // SAFETY: mkfifo only reads the nul-terminated path handed to it.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // ------------------------------------------------- the arithmetic

    /// The geometry claim the whole renderer rests on, and the one the
    /// prototype got wrong: `ui::raster` wants two samples per cell per
    /// axis, and the letterbox has to be computed against the cell's
    /// *physical* shape. A cell is about twice as tall as it is wide,
    /// so fitting a picture into a `2cols x 2rows` square-pixel box
    /// stretches it vertically by the cell aspect, a 2x error.
    #[test]
    fn the_letterbox_is_computed_against_the_cell_shape_not_square_pixels() {
        let grid = Grid::new(160, 48, 2.0);
        assert_eq!(grid.pixels(), (320, 96));
        assert_eq!(grid.frame_len(), 320 * 96 * 3);
        // the fit box is the sample grid's real-world shape.
        assert_eq!(grid.fit_box(), (320, 192));
        assert_eq!(
            grid.vf(),
            "scale=320:192:force_original_aspect_ratio=decrease,\
             pad=320:192:-1:-1:color=black,scale=320:96,format=rgb24"
        );
        // a square cell needs no squash, and the same chain degenerates
        // into that rather than being special-cased.
        assert_eq!(Grid::new(160, 48, 1.0).fit_box(), (320, 96));
    }

    /// A terminal that reports a zero or absurd font size must not put
    /// a zero height into the filter chain; mpv would refuse the whole
    /// `--vf` and never write a byte.
    #[test]
    fn a_nonsense_cell_aspect_falls_back_instead_of_reaching_mpv() {
        for bad in [0.0, -2.0, f32::NAN, f32::INFINITY] {
            let grid = Grid::new(40, 20, bad);
            assert_eq!(grid.cell_aspect, CELL_ASPECT, "{bad}");
            assert_eq!(grid.fit_box(), (80, 80));
        }
        // an aspect that is merely extreme is honoured rather than
        // replaced, but it must never round the box height to zero:
        // mpv refuses the whole `--vf` and writes nothing at all.
        assert_eq!(Grid::new(1, 1, 0.01).fit_box().1, 1);
        assert_eq!(Grid::new(1, 1, 2.0).fit_box().1, 4);
    }

    /// `--start=t` under `--hr-seek=yes` lands on `floor(t * fps)`
    /// (measured: `--start=5.37` on a 30fps file handed over frame
    /// 161, t=5.3667). Left unsnapped, `frame_time` is wrong by up to
    /// one frame for the whole life of the instance, which reads as a
    /// permanent sync offset rather than as drift.
    #[test]
    fn a_start_time_snaps_onto_the_frame_grid() {
        let sched = Schedule::new(5.37, 30.0);
        assert_eq!(sched.start(), 161.0 / 30.0);
        assert_eq!(sched.frame_time(0), 161.0 / 30.0);
        // already on the grid: unchanged, not nudged.
        assert_eq!(Schedule::new(2.0, 25.0).start(), 2.0);
        // before the start of the stream is zero, not negative.
        assert_eq!(Schedule::new(-4.0, 25.0).start(), 0.0);
    }

    /// `container-fps` is 0 for some containers, and a zero here would
    /// divide every frame time by it.
    #[test]
    fn a_missing_container_fps_falls_back_rather_than_dividing_by_zero() {
        for bad in [0.0, -30.0, f64::NAN, f64::INFINITY] {
            let sched = Schedule::new(1.0, bad);
            assert_eq!(sched.fps(), FALLBACK_FPS, "{bad}");
            assert!(sched.frame_time(10).is_finite(), "{bad}");
            assert!(
                matches!(sched.step(1.0), Step::Advance(_) | Step::Hold),
                "{bad}"
            );
        }
    }

    /// No drift by construction: frame `n` is `start + n / fps` and
    /// nothing accumulates, so ten thousand frames in the answer is
    /// still exact to a hair rather than to a frame.
    #[test]
    fn frame_time_does_not_accumulate_error() {
        let sched = Schedule::new(0.0, 30.0);
        assert!((sched.frame_time(10_000) - 10_000.0 / 30.0).abs() < 1e-9);
        let late = Schedule::new(600.0, 23.976);
        assert!((late.frame_time(100_000) - (late.start() + 100_000.0 / 23.976)).abs() < 1e-6);
    }

    /// The knife edge, and it is not hypothetical: a host drives this
    /// off `time-pos`, which lands on exact frame boundaries, and
    /// `7 / 25.0 * 25.0` is `6.999999999999999`. Floored naively that
    /// is the frame already on screen, so `step` says hold, so nothing
    /// is read, so `time-pos` never moves either: a permanent stall at
    /// whichever frame rounds down first.
    #[test]
    fn a_clock_exactly_on_a_frame_boundary_does_not_stall() {
        for fps in [25.0, 30.0, 23.976, 59.94] {
            let mut sched = Schedule::new(0.0, fps);
            for n in 0..400u64 {
                assert_eq!(
                    sched.step(n as f64 / fps),
                    Step::Advance(1),
                    "{fps}fps stalled on frame {n}"
                );
                sched.count();
            }
        }
    }

    /// Pause, with no pause branch anywhere: a frozen `now` says hold,
    /// so nothing is read, so mpv blocks in `write(2)`.
    #[test]
    fn a_frozen_clock_holds_which_is_what_pause_is() {
        let mut sched = Schedule::new(0.0, 25.0);
        assert_eq!(
            sched.step(0.0),
            Step::Advance(1),
            "the first frame is needed"
        );
        sched.count();
        for _ in 0..100 {
            assert_eq!(sched.step(0.02), Step::Hold);
        }
    }

    /// Catching up is read-and-discard, and the count is exact: a
    /// quarter second behind at 25fps is six frames, not a seek.
    #[test]
    fn catching_up_asks_for_exactly_the_frames_it_is_behind() {
        let mut sched = Schedule::new(0.0, 25.0);
        for _ in 0..5 {
            sched.count();
        }
        // served 0..4, so the newest is frame 4 at t=0.16.
        assert_eq!(sched.step(0.16), Step::Hold);
        assert_eq!(sched.step(0.2), Step::Advance(1));
        assert_eq!(sched.step(0.4), Step::Advance(6));
    }

    /// The threshold that decides between the two, and the only place
    /// the 180-550ms of black on a respawn is spent deliberately.
    #[test]
    fn only_a_gap_wider_than_reading_can_close_restarts() {
        let mut sched = Schedule::new(0.0, 25.0);
        sched.count();
        // one second of 25fps content is the limit, so inside it reads.
        assert_eq!(sched.step(1.0), Step::Advance(25));
        assert_eq!(sched.step(5.0), Step::Restart(5.0));
    }

    /// A small backwards seek holds rather than respawning: the frame
    /// on screen is a little way into the future and the clock walks
    /// back up to it, which costs nothing and beats 180-550ms of black.
    /// Past the same one-second limit it cannot be absorbed, because a
    /// pipe does not rewind.
    #[test]
    fn a_small_backwards_seek_holds_and_a_large_one_restarts() {
        let mut sched = Schedule::new(10.0, 25.0);
        for _ in 0..30 {
            sched.count();
        }
        // newest is frame 29, t = 11.16.
        assert_eq!(sched.step(11.0), Step::Hold);
        assert_eq!(sched.step(10.5), Step::Hold);
        assert_eq!(sched.step(10.0), Step::Restart(10.0));
        assert_eq!(sched.step(2.0), Step::Restart(2.0));
        // and before the instance's own start is always a restart,
        // whatever has been served.
        assert_eq!(Schedule::new(50.0, 25.0).step(1.0), Step::Restart(1.0));
    }

    /// A `time-pos` that has not been observed yet arrives as NaN
    /// rather than as a number; holding is the safe reading, since a
    /// restart on a bad clock would respawn every frame.
    #[test]
    fn an_unobserved_clock_holds_rather_than_respawning() {
        let sched = Schedule::new(0.0, 25.0);
        assert_eq!(sched.step(f64::NAN), Step::Hold);
        assert_eq!(sched.step(f64::INFINITY), Step::Hold);
    }

    #[test]
    fn shown_time_is_none_until_a_frame_has_landed() {
        let mut sched = Schedule::new(4.0, 25.0);
        assert_eq!(sched.shown_time(), None);
        assert_eq!(sched.next_time(), 4.0);
        sched.count();
        assert_eq!(sched.shown_time(), Some(4.0));
        assert_eq!(sched.next_time(), 4.04);
    }

    /// A grid with no cells, or one whose arithmetic has run away, is
    /// refused before a process is spawned for it.
    #[test]
    fn an_impossible_grid_is_refused_before_mpv_is_spawned() {
        for grid in [
            Grid::new(0, 0, 2.0),
            Grid::new(0, 40, 2.0),
            Grid::new(40, 0, 2.0),
            Grid::new(4096, 4096, 2.0),
        ] {
            let err =
                FrameSource::spawn(Path::new(LAVFI), grid, 0.0, 25.0).expect_err("should refuse");
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{grid:?}");
        }
    }

    // ------------------------------------------------- against mpv

    /// A generated source rather than a file: nothing to create, so
    /// nothing to clean up, and no encoder has to exist. 500 frames.
    const LAVFI: &str = "av://lavfi:testsrc2=size=320x240:rate=25:duration=20";
    /// Same generator at 16:9, for the letterbox geometry.
    const LAVFI_WIDE: &str = "av://lavfi:testsrc2=size=320x180:rate=25:duration=20";
    const FPS: f64 = 25.0;
    /// mpv is 124-290ms to a first frame; this is slack for a loaded
    /// machine, not an expected wait.
    const PATIENCE: Duration = Duration::from_secs(20);

    fn source(media: &str, grid: Grid, start: f64) -> FrameSource {
        FrameSource::spawn(Path::new(media), grid, start, FPS).expect("mpv must be on PATH")
    }

    /// Reads until a whole frame lands, or gives up and reports what it
    /// got instead. Every caller asserts on the result, so a timeout is
    /// a failure rather than a silent skip.
    fn wait_frame(src: &mut FrameSource) -> Pull {
        let deadline = Instant::now() + PATIENCE;
        loop {
            match src.pull().expect("pull") {
                Pull::Pending if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                other => return other,
            }
        }
    }

    /// An encoded clip in its own temp dir, removed on drop.
    ///
    /// `mpeg4` rather than `libx264` for the correctness tests: it is
    /// built into ffmpeg so it cannot be missing, and `g=250` makes the
    /// whole clip one gop, so a keyframe seek lands on frame 0 and an
    /// exact one lands where it was asked. The benchmarks ask for h264
    /// instead, because they measure decode cost.
    struct Clip {
        dir: PathBuf,
        path: PathBuf,
    }

    impl Clip {
        fn encode(codec: &str, opts: &str, size: &str, rate: u32, frames: u32) -> Clip {
            let dir = std::env::temp_dir().join(format!(
                "wim-frames-clip-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let path = dir.join("clip.mp4");
            let status = Command::new("mpv")
                .arg("--no-config")
                .arg("--no-terminal")
                .arg(format!("av://lavfi:testsrc2=size={size}:rate={rate}"))
                .arg(format!("--frames={frames}"))
                .arg("--audio=no")
                .arg("--of=mp4")
                .arg(format!("--ovc={codec}"))
                .arg(format!("--ovcopts={opts}"))
                .arg(format!("--o={}", path.display()))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("mpv must be on PATH");
            assert!(
                status.success() && path.exists(),
                "{codec} would not encode: {status}"
            );
            Clip { dir, path }
        }

        fn mpeg4(size: &str, rate: u32, frames: u32) -> Clip {
            Clip::encode("mpeg4", "g=250,qscale=6", size, rate, frames)
        }

        fn media(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }
    }

    impl Drop for Clip {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir(&self.dir);
        }
    }

    /// Frame `n` of `media` read sequentially from the start, as the
    /// reference for what a seek to that frame's time should produce.
    fn nth_frame(media: &str, grid: Grid, n: u64) -> Vec<u8> {
        let mut src = source(media, grid, 0.0);
        for i in 0..=n {
            assert_eq!(wait_frame(&mut src), Pull::Frame, "frame {i} of {n}");
        }
        src.frame_bytes().to_vec()
    }

    /// The whole premise: a real decoded frame arrives, and it is
    /// exactly `w * h * 3` bytes with no stride to interpret.
    #[test]
    fn a_frame_is_exactly_the_grid_times_three_bytes() {
        let grid = Grid::new(32, 16, 2.0);
        let mut src = source(LAVFI, grid, 0.0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        assert_eq!(src.frame_bytes().len(), grid.frame_len());
        assert_eq!(src.frame_bytes().len(), 64 * 32 * 3);
        assert!(
            src.frame_bytes().iter().any(|&b| b != 0),
            "a black frame means mpv wrote nothing and the buffer is still the blank one"
        );
        assert_eq!(src.schedule().served(), 1);
    }

    /// The fifo behaviour that cost the prototype a debugging cycle: a
    /// read on a fifo with no writer attached yet is `Ok(0)`, a clean
    /// eof, not `WouldBlock`. Read as eof it ends the stream before mpv
    /// has opened its end, every single time.
    #[test]
    fn zero_bytes_before_mpv_opens_its_end_means_wait_not_eof() {
        let mut src = source(LAVFI, Grid::new(32, 16, 2.0), 0.0);
        // the first pull happens microseconds after the fork, long
        // before mpv has its output open: it must not be `Ended`.
        let mut pendings = 0;
        let deadline = Instant::now() + PATIENCE;
        loop {
            match src.pull().expect("pull") {
                Pull::Pending => {
                    pendings += 1;
                    assert!(Instant::now() < deadline, "no frame ever arrived");
                }
                Pull::Frame => break,
                Pull::Ended => panic!("read zero bytes and called it the end of the stream"),
            }
        }
        assert!(
            pendings > 0,
            "mpv answered before the first pull; test proves nothing"
        );
    }

    /// The letterbox, end to end against a real decode, and the test
    /// that would have caught the prototype's filter chain.
    ///
    /// A 16:9 source in a 160x48 cell grid is wider than the grid's
    /// physical shape, so it letterboxes *horizontally*: 90 of the 96
    /// sample rows lit, three black sample rows top and bottom, and
    /// every one of the 320 columns lit. Letterboxing for square pixels
    /// instead inverts both: all 96 rows lit and only 170 of the
    /// columns, i.e. the picture squeezed into a pillarbox. Measured
    /// both ways.
    #[test]
    fn a_wide_source_letterboxes_to_the_rows_the_cell_aspect_predicts() {
        let grid = Grid::new(160, 48, 2.0);
        let mut src = source(LAVFI_WIDE, grid, 0.0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        let (w, h) = grid.pixels();
        let (w, h) = (w as usize, h as usize);
        let px = src.frame_bytes();
        let mean = |it: &mut dyn Iterator<Item = usize>, n: usize| {
            it.map(|i| u32::from(px[i])).sum::<u32>() / (n * BPP) as u32
        };
        // the squash is bilinear, so the edge is soft by a row; 40 is
        // well clear of both the lit rows (119) and the bleed (25).
        let rows = (0..h)
            .filter(|&y| mean(&mut (y * w * BPP..(y + 1) * w * BPP), w) > 40)
            .count();
        let cols = (0..w)
            .filter(|&x| {
                mean(
                    &mut (0..h).flat_map(|y| ((y * w + x) * BPP)..((y * w + x) * BPP + BPP)),
                    h,
                ) > 40
            })
            .count();
        assert_eq!(rows, 90, "{rows} lit rows of {h}; the aspect maths moved");
        assert_eq!(
            cols, w,
            "{cols} lit columns of {w}; the picture is pillarboxed"
        );
    }

    /// The fifo is the clock. Not reading stops the decoder, because
    /// mpv blocks in `write(2)` once the pipe fills; so pause costs
    /// nothing and there is no buffer to bound.
    #[test]
    fn not_reading_stops_the_decoder() {
        // one frame is larger than a pipe buffer at this grid, so there
        // is nowhere for a free-running decoder to put its work.
        let grid = Grid::new(160, 48, 2.0);
        assert!(grid.frame_len() > 64 * 1024);
        let mut src = source(LAVFI, grid, 0.0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);

        // what reading flat out gets, as the counterfactual.
        let t = Instant::now();
        let mut ceiling = 0;
        while t.elapsed() < Duration::from_secs(1) {
            if src.pull().expect("pull") == Pull::Frame {
                ceiling += 1;
            }
        }
        assert!(
            ceiling > 10,
            "only {ceiling} fps read flat out; too slow to compare"
        );

        // and what a one second stall leaves waiting.
        std::thread::sleep(Duration::from_secs(1));
        let mut waiting = 0;
        while src.pull().expect("pull") == Pull::Frame {
            waiting += 1;
        }
        println!("stall: {waiting} frames waiting; flat out this grid reads {ceiling}/s");
        assert!(
            waiting * 4 < ceiling,
            "{waiting} frames waiting after a 1s stall against a {ceiling}/s ceiling: \
             the decoder is running free, so the fifo is not the clock"
        );
    }

    /// Sync is read scheduling, so the arithmetic has to be right about
    /// which frame a respawn lands on: the first frame after a restart
    /// at `t` must be byte-for-byte the frame reached by reading to `t`
    /// from the start. This is what `--hr-seek=yes` buys; without it
    /// mpv lands on the previous keyframe instead.
    #[test]
    fn a_restart_lands_on_the_frame_the_schedule_names() {
        let grid = Grid::new(32, 16, 2.0);
        let want = nth_frame(LAVFI, grid, 50);

        let mut src = source(LAVFI, grid, 0.0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        src.restart(50.0 / FPS).expect("restart");
        assert_eq!(src.schedule().served(), 0);
        assert_eq!(src.schedule().start(), 2.0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        assert!(
            src.frame_bytes() == want.as_slice(),
            "a restart at t=2.0 did not land on frame 50"
        );
        assert_eq!(src.schedule().shown_time(), Some(2.0));
    }

    /// The same claim against a real encoded clip rather than a
    /// generator, which is where it can actually go wrong: a keyframe
    /// seek lands on the previous keyframe, and with the whole clip in
    /// one gop that is frame 0. Measured both ways; the exact seek
    /// lands on frame 100, a keyframe seek on frame 0.
    #[test]
    fn a_restart_on_a_keyframed_clip_lands_on_the_named_frame_not_a_keyframe() {
        let clip = Clip::mpeg4("320x240", 25, 250);
        let media = clip.media();
        let grid = Grid::new(32, 16, 2.0);
        let want = nth_frame(&media, grid, 100);

        let mut src = source(&media, grid, 0.0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        src.restart(4.0).expect("restart");
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        assert!(
            src.frame_bytes() == want.as_slice(),
            "a restart at t=4.0 landed on a keyframe rather than on frame 100"
        );
    }

    /// A restart in the middle of a frame must drop the half it was
    /// holding. Carried over, those bytes splice onto the first frame
    /// of the new instance and the picture is torn for one frame after
    /// every seek.
    #[test]
    fn a_restart_mid_frame_drops_the_partial_rather_than_splicing_it() {
        // larger than a pipe buffer, so the first pull is guaranteed
        // to be a partial read.
        let grid = Grid::new(160, 48, 2.0);
        let want = nth_frame(LAVFI, grid, 25);

        let mut src = source(LAVFI, grid, 0.0);
        let deadline = Instant::now() + PATIENCE;
        while src.filled == 0 {
            src.pull().expect("pull");
            assert!(Instant::now() < deadline, "no partial read ever happened");
        }
        assert!(
            src.filled < grid.frame_len(),
            "that was a whole frame, not a partial"
        );

        src.restart(1.0).expect("restart");
        assert_eq!(src.filled, 0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        assert!(
            src.frame_bytes() == want.as_slice(),
            "the first frame after a mid-frame restart is not frame 25"
        );
    }

    /// Pause is "stop reading", and `advance_to` is where that has to
    /// show up: a clock that is not moving reads nothing at all.
    #[test]
    fn advance_to_reads_nothing_while_the_clock_is_still() {
        let grid = Grid::new(32, 16, 2.0);
        let mut src = source(LAVFI, grid, 0.0);
        let deadline = Instant::now() + PATIENCE;
        while src.advance_to(0.0).expect("advance") != Pull::Frame {
            assert!(Instant::now() < deadline, "no first frame");
        }
        assert_eq!(src.schedule().served(), 1);
        for _ in 0..200 {
            assert_eq!(src.advance_to(0.0).expect("advance"), Pull::Pending);
        }
        assert_eq!(src.schedule().served(), 1, "a still clock pulled frames");
    }

    /// And a moving one does: `advance_to` on its own is enough to
    /// follow a clock, with the restart folded in.
    #[test]
    fn advance_to_follows_a_moving_clock_and_restarts_over_a_seek() {
        let grid = Grid::new(32, 16, 2.0);
        let mut src = source(LAVFI, grid, 0.0);
        let deadline = Instant::now() + PATIENCE;
        while src.schedule().served() < 10 {
            // the clock is driven off what has been shown rather than
            // off wall time, so this asks for exactly the next frame
            // and never races the decoder into a restart.
            let now = src.schedule().shown_time().unwrap_or(0.0) + 1.0 / FPS;
            src.advance_to(now).expect("advance");
            assert!(Instant::now() < deadline, "no frames at all");
        }
        // a seek past the catch-up limit respawns, and the schedule
        // follows it rather than the old instance. the respawn does not
        // hand over a frame in the same call, which is exactly the
        // 180-550ms of black a seek costs.
        assert_eq!(src.advance_to(12.0).expect("advance"), Pull::Pending);
        assert_eq!(src.schedule().start(), 12.0);
        assert_eq!(src.schedule().served(), 0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        assert_eq!(src.schedule().shown_time(), Some(12.0));
    }

    /// A resized pane is a respawn, because the scale is baked into
    /// `--vf`; it has to resume where reading had got to rather than
    /// from the start of the file.
    #[test]
    fn a_resize_respawns_at_the_frame_reading_had_reached() {
        let mut src = source(LAVFI, Grid::new(32, 16, 2.0), 0.0);
        for _ in 0..5 {
            assert_eq!(wait_frame(&mut src), Pull::Frame);
        }
        let resume = src.schedule().next_time();
        assert_eq!(resume, 5.0 / FPS);

        let wider = Grid::new(48, 24, 2.0);
        src.resize(wider).expect("resize");
        assert_eq!(src.grid(), wider);
        assert_eq!(src.schedule().start(), resume);
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        assert_eq!(src.frame_bytes().len(), wider.frame_len());
        // an unchanged grid is not a respawn; it would blank the screen
        // on every frame the pane did not move.
        let served = src.schedule().served();
        src.resize(wider).expect("resize");
        assert_eq!(src.schedule().served(), served);
    }

    /// Completing a frame hands back the other buffer, never the same
    /// one: the double buffer is what stops a partial read tearing the
    /// frame on screen.
    ///
    /// This is *not* the allocation proof, and the attempt to make it
    /// one is worth recording. Mutating the swap into
    /// `front = back.clone()` passes this test: the clone allocates
    /// while the old buffer is still live, so it cannot reuse that
    /// chunk, and the next frame's clone gets the chunk just freed.
    /// A copy per frame therefore alternates between two addresses
    /// exactly like a swap does, and pointer identity cannot tell them
    /// apart. The count lives in `tests/framesource_alloc.rs`, which
    /// has an allocator of its own.
    #[test]
    fn completing_a_frame_hands_back_the_other_buffer() {
        let grid = Grid::new(32, 16, 2.0);
        let mut src = source(LAVFI, grid, 0.0);
        let mut seen = Vec::new();
        let mut last = 0usize;
        for i in 0..40 {
            assert_eq!(wait_frame(&mut src), Pull::Frame);
            let ptr = src.frame_bytes().as_ptr() as usize;
            assert_eq!(src.frame_bytes().len(), grid.frame_len());
            assert_ne!(
                ptr, last,
                "frame {i} came back in the same buffer: not a swap"
            );
            last = ptr;
            if !seen.contains(&ptr) {
                seen.push(ptr);
            }
        }
        assert_eq!(
            seen.len(),
            2,
            "{} distinct frame buffers over 40 frames; something reallocates per frame",
            seen.len()
        );
    }

    /// The end of the stream is mpv exiting *and* the pipe running dry,
    /// in that order, and it sticks: a host that keeps polling must not
    /// see a frame again.
    #[test]
    fn the_end_of_the_stream_is_ended_and_stays_ended() {
        // two frames of content, so the end arrives immediately.
        let short = "av://lavfi:testsrc2=size=64x64:rate=25:duration=0.08";
        let mut src = source(short, Grid::new(16, 8, 2.0), 0.0);
        let deadline = Instant::now() + PATIENCE;
        loop {
            match src.pull().expect("pull") {
                Pull::Ended => break,
                _ => assert!(Instant::now() < deadline, "the stream never ended"),
            }
        }
        assert!(
            src.schedule().served() >= 1,
            "ended before handing over a frame"
        );
        for _ in 0..5 {
            assert_eq!(src.pull().expect("pull"), Pull::Ended);
        }
        // and a host still driving it with the clock it has keeps
        // hearing the same answer rather than a stale frame.
        let held = src.schedule().shown_time().expect("a frame landed");
        assert_eq!(src.advance_to(held).expect("advance"), Pull::Ended);
    }

    /// A media file mpv cannot open is an ended stream rather than a
    /// hang. The only thing that says "over" is the child exiting, so a
    /// child that exits straight away has to be noticed.
    #[test]
    fn a_media_mpv_cannot_open_ends_instead_of_hanging() {
        let missing = std::env::temp_dir().join("wim-framesource-no-such-file.mkv");
        let mut src = source(&missing.to_string_lossy(), Grid::new(16, 8, 2.0), 0.0);
        let deadline = Instant::now() + PATIENCE;
        loop {
            match src.pull().expect("pull") {
                Pull::Ended => break,
                Pull::Frame => panic!("a frame from a file that does not exist"),
                Pull::Pending => assert!(Instant::now() < deadline, "hung on a missing file"),
            }
        }
    }

    /// Nothing outlives the struct: the child is reaped, the fifo is
    /// gone and the temp dir with it. Only ever this source's own pid,
    /// never a pattern.
    #[test]
    fn a_drop_takes_the_child_the_fifo_and_the_dir_with_it() {
        let (pid, fifo, dir);
        {
            let mut src = source(LAVFI, Grid::new(32, 16, 2.0), 0.0);
            assert_eq!(wait_frame(&mut src), Pull::Frame);
            pid = src.child.as_ref().expect("child").id();
            fifo = src.fifo.clone();
            dir = src.dir.clone();
            assert!(fifo.exists(), "no fifo was made");
        }
        // SAFETY: signal 0 only tests for the process' existence.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        assert!(!alive, "mpv {pid} outlived its FrameSource");
        assert!(!fifo.exists(), "left a fifo behind");
        assert!(!dir.exists(), "left a temp dir behind");
    }

    /// The panic path, which is the one `src/term.rs` cares about: a
    /// frame instance left decoding into a dead pipe would be the one
    /// thing that outlived the terminal restore. `Drop` runs while
    /// unwinding, so it does not.
    #[test]
    fn a_panic_still_tears_the_child_down() {
        let caught = std::panic::catch_unwind(|| {
            let mut src = source(LAVFI, Grid::new(32, 16, 2.0), 0.0);
            assert_eq!(wait_frame(&mut src), Pull::Frame);
            let pid = src.child.as_ref().expect("child").id();
            let dir = src.dir.clone();
            std::panic::panic_any((pid, dir));
        });
        let payload = caught.expect_err("the closure must panic");
        let (pid, dir) = *payload.downcast::<(u32, PathBuf)>().expect("own payload");
        // SAFETY: signal 0 only tests for the process' existence.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        assert!(!alive, "mpv {pid} survived a panic");
        assert!(!dir.exists(), "a panic left a temp dir behind");
    }

    /// Two sources at once must not share a fifo; the pid alone is not
    /// enough to tell them apart inside one process.
    #[test]
    fn two_sources_in_one_process_get_their_own_fifos() {
        let grid = Grid::new(16, 8, 2.0);
        let mut a = source(LAVFI, grid, 0.0);
        let mut b = source(LAVFI, grid, 4.0);
        assert_ne!(a.fifo, b.fifo);
        assert_eq!(wait_frame(&mut a), Pull::Frame);
        assert_eq!(wait_frame(&mut b), Pull::Frame);
        assert_ne!(
            a.frame_bytes(),
            b.frame_bytes(),
            "two instances at different start times handed over the same frame"
        );
    }

    // ------------------------------------------------- rasterization

    /// The frame reaches the screen through the album-art rasterizer
    /// rather than a second one, and in `Tone::Mono` every cell is a
    /// `theme::shade` grey, same invariant the rest of the ui holds to.
    #[test]
    fn a_frame_paints_grey_cells_in_mono() {
        let grid = Grid::new(24, 12, 2.0);
        let area = Rect::new(0, 0, 24, 12);
        let mut src = source(LAVFI, grid, 0.0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);

        let mut buf = Buffer::empty(area);
        src.paint(area, Tone::Mono, &mut buf);
        let mut drawn = 0;
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = buf.cell((x, y)).expect("in bounds");
                if cell.symbol() != " " {
                    drawn += 1;
                }
                for colour in [cell.fg, cell.bg] {
                    if let ratatui::style::Color::Rgb(r, g, b) = colour {
                        assert!(r == g && g == b, "hue leaked into a mono video cell");
                    }
                }
            }
        }
        assert!(drawn > 100, "only {drawn} cells carried a glyph");
    }

    /// Colour is possible without a second code path, because the tone
    /// is the rasterizer's own parameter: whether video is allowed to
    /// use it is a policy question, not a capability one.
    #[test]
    fn colour_is_the_same_call_with_a_different_tone() {
        let grid = Grid::new(24, 12, 2.0);
        let area = Rect::new(0, 0, 24, 12);
        let mut src = source(LAVFI, grid, 0.0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);

        let mut mono = Buffer::empty(area);
        src.paint(area, Tone::Mono, &mut mono);
        let mut colour = Buffer::empty(area);
        src.paint(area, Tone::Color, &mut colour);
        assert_ne!(mono, colour, "the tone made no difference");
        let hues = colour
            .content
            .iter()
            .filter(|c| matches!(c.fg, ratatui::style::Color::Rgb(r, g, b) if r != g || g != b));
        assert!(hues.count() > 0, "Tone::Color painted no colour at all");
    }

    /// A pane smaller than the frame's grid must clip, not panic; the
    /// grid lags the layout by one respawn.
    #[test]
    fn painting_into_a_rect_smaller_than_the_grid_clips() {
        let grid = Grid::new(24, 12, 2.0);
        let mut src = source(LAVFI, grid, 0.0);
        assert_eq!(wait_frame(&mut src), Pull::Frame);
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 3));
        src.paint(Rect::new(2, 1, 40, 40), Tone::Mono, &mut buf);
        src.paint(Rect::new(0, 0, 0, 0), Tone::Mono, &mut buf);
    }

    // ------------------------------------------------- measurement

    /// Cpu time, not wall time: the other cores are busy during a run,
    /// and wall clock measures their scheduling as much as this code.
    /// `CLOCK_PROCESS_CPUTIME_ID` is this process only, so it is wim's
    /// half of the cost; mpv's half is `RUSAGE_CHILDREN`.
    fn cpu_now() -> Duration {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime only writes the timespec we hand it.
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
        Duration::new(
            u64::try_from(ts.tv_sec).unwrap_or(0),
            u32::try_from(ts.tv_nsec).unwrap_or(0),
        )
    }

    /// Cpu burnt by reaped children, i.e. by the frame instances. The
    /// decode cost does not show up in this process at all, so without
    /// this the measurement would report the cheap half only.
    fn child_cpu() -> Duration {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: getrusage only writes the rusage we hand it.
        unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
        let secs = |t: libc::timeval| {
            Duration::new(
                u64::try_from(t.tv_sec).unwrap_or(0),
                u32::try_from(t.tv_usec).unwrap_or(0) * 1000,
            )
        };
        secs(ru.ru_utime) + secs(ru.ru_stime)
    }

    /// Rss of a pid, in kilobytes, from `/proc`. One extra process is a
    /// cost worth stating rather than estimating.
    fn rss_kb(pid: u32) -> u64 {
        std::fs::read_to_string(format!("/proc/{pid}/statm"))
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map_or(0, |pages| pages * 4)
    }

    /// h264, because a cpu measurement against a *generator* measures
    /// the generator: lavfi has no keyframes, so every restart decodes
    /// from zero, and its "decode" is cheaper than any real codec.
    fn h264(size: &str, rate: u32, frames: u32) -> Clip {
        Clip::encode("libx264", "preset=veryfast,crf=28", size, rate, frames)
    }

    /// The numbers the frame-budget item asks for, at the cell grids
    /// that matter, against real h264 at two source resolutions.
    ///
    /// Two loops per case rather than one, because they answer
    /// different questions. Flat out gives the decode ceiling and mpv's
    /// cpu, which is the budget. Paced gives wim's own cpu per frame,
    /// which a flat-out loop cannot: almost every `pull` in it is a
    /// no-op poll, so dividing the spin by the frames measures the spin.
    ///
    /// Allocations per frame are not timed because they are zero by
    /// construction and pinned as such by
    /// `the_steady_state_swaps_two_buffers_and_allocates_nothing`: the
    /// read goes into a pre-sized buffer, completing a frame is a swap
    /// of two pointers, and `raster`'s scratch is thread-local and
    /// kept. The only allocating operations are `spawn` and `resize`.
    ///
    /// `cargo test --release -- --ignored --nocapture bench_frame_source`
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_frame_source() {
        let clips = [
            ("480p25", h264("640x480", 25, 700)),
            ("1080p30", h264("1920x1080", 30, 400)),
        ];
        for (label, clip) in &clips {
            for (cols, rows) in [(80, 24), (160, 48), (320, 96)] {
                let grid = Grid::new(cols, rows, 2.0);
                let area = Rect::new(0, 0, cols, rows);
                let media = clip.media();

                // flat out: the ceiling, and the decode cost.
                let mut src = source(&media, grid, 0.0);
                assert_eq!(wait_frame(&mut src), Pull::Frame);
                let rss = rss_kb(src.child.as_ref().expect("child").id());
                let kids0 = child_cpu();
                let wall = Instant::now();
                let mut frames = 0u32;
                while wall.elapsed() < Duration::from_secs(2) {
                    match src.pull().expect("pull") {
                        Pull::Frame => frames += 1,
                        Pull::Ended => break,
                        Pull::Pending => {}
                    }
                }
                let flat_out = f64::from(frames) / wall.elapsed().as_secs_f64();
                drop(src);
                let decode =
                    child_cpu().saturating_sub(kids0).as_secs_f64() / f64::from(frames.max(1));

                // paced, the way a ui loop reads: wim's own cost.
                let mut src = source(&media, grid, 0.0);
                assert_eq!(wait_frame(&mut src), Pull::Frame);
                let (mut read_cpu, mut frames, mut polls) = (Duration::ZERO, 0u32, 0u32);
                let wall = Instant::now();
                while wall.elapsed() < Duration::from_secs(2) {
                    let t = cpu_now();
                    let got = src.pull().expect("pull");
                    read_cpu += cpu_now().saturating_sub(t);
                    polls += 1;
                    match got {
                        Pull::Frame => frames += 1,
                        Pull::Ended => break,
                        Pull::Pending => std::thread::sleep(Duration::from_micros(500)),
                    }
                }
                let paced = f64::from(frames) / wall.elapsed().as_secs_f64();
                let per_frame = read_cpu / frames.max(1);

                let mut buf = Buffer::empty(area);
                let mut raster = Duration::MAX;
                for _ in 0..20 {
                    let t = cpu_now();
                    src.paint(area, Tone::Mono, &mut buf);
                    raster = raster.min(cpu_now().saturating_sub(t));
                }
                drop(src);

                println!(
                    "{label} {cols:>3}x{rows:<3} ceiling {flat_out:>6.1} fps  \
                     decode {:>8.3}ms/f  read {per_frame:>9.3?}/f ({:.1} polls/f)  \
                     raster {raster:>9.3?}/f  paced {paced:>6.1} fps  rss {:>4}MB",
                    decode * 1000.0,
                    f64::from(polls) / f64::from(frames.max(1)),
                    rss / 1024,
                );
            }
        }
    }

    /// What a seek costs, which is the design's one real wart: encode
    /// mode has no socket to seek over, so the instance is respawned
    /// and the pane is black until its first frame lands.
    ///
    /// `cargo test --release -- --ignored --nocapture bench_restart`
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_restart() {
        let clips = [
            ("480p25", h264("640x480", 25, 700)),
            ("1080p30", h264("1920x1080", 30, 400)),
        ];
        for (label, clip) in &clips {
            let media = clip.media();
            for (cols, rows) in [(80, 24), (160, 48), (320, 96)] {
                let grid = Grid::new(cols, rows, 2.0);
                let mut src = source(&media, grid, 0.0);
                assert_eq!(wait_frame(&mut src), Pull::Frame);
                let mut best = Duration::MAX;
                let mut worst = Duration::ZERO;
                for i in 1..=6 {
                    let t = Instant::now();
                    src.restart(f64::from(i)).expect("restart");
                    assert_eq!(wait_frame(&mut src), Pull::Frame);
                    let took = t.elapsed();
                    best = best.min(took);
                    worst = worst.max(took);
                }
                println!(
                    "{label} restart {cols:>3}x{rows:<3} {best:>9.3?} best  {worst:>9.3?} worst"
                );
            }
        }
    }
}
