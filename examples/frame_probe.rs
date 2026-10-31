//! proof that a frame source exists: pull one real decoded frame out of
//! mpv at cell-grid resolution and print it.
//!
//! docs/video.md used to record "there is no frame source", on the
//! strength of `screenshot-raw` being unusable over json ipc (it is;
//! mpv emits a literal `"data":}` and the line is not json). that is a
//! fact about *that one command*, not about mpv. a second, headless mpv
//! in encode mode with `--of=rawvideo` hands over pre-scaled packed
//! rgb24 on a fifo, no ipc, no image encode, no parsing: exactly
//! `w * h * 3` bytes per frame, and mpv does the downscale.
//!
//! the fifo is also the clock. mpv blocks in `write(2)` once the pipe
//! is full, so whatever rate this reads at *is* the decode rate; there
//! is no racing ahead and no buffer to bound.
//!
//! run it:
//! ```text
//! cargo run --example frame_probe -- /path/to/video.mp4 160 96
//! ```
//! see docs/video.md, "a frame source does exist".

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// pixel layout handed over: packed, one byte per channel, no padding,
/// so a frame is `w * h * BPP` bytes and nothing has to interpret a
/// stride.
const BPP: usize = 3;

fn main() {
    let mut args = std::env::args().skip(1);
    let video = match args.next() {
        Some(v) => v,
        None => {
            eprintln!("usage: frame_probe <video> [cols] [rows]");
            std::process::exit(2);
        }
    };
    let w: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(160);
    let h: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(96);

    let dir = std::env::temp_dir().join(format!("wim-frame-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let fifo = dir.join("frames.rgb24");
    mkfifo(&fifo);

    // scale to the grid, letterbox inside it, then force packed rgb24.
    // the pad is what makes every frame the same size whatever the
    // source aspect is, so the reader never has to renegotiate.
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,\
         pad={w}:{h}:-1:-1:color=black,format=rgb24"
    );

    let t0 = Instant::now();
    let mut child = Command::new("mpv")
        .arg("--no-config")
        .arg("--no-terminal")
        .arg(format!("--vf={vf}"))
        // this instance is a frame source and nothing else: no audio
        // device to fight the audible instance over, no subtitles.
        .arg("--audio=no")
        .arg("--sub=no")
        .arg("--of=rawvideo")
        .arg("--ovc=rawvideo")
        .arg("--ovcopts=pix_fmt=rgb24")
        .arg(format!("--o={}", fifo.display()))
        .arg(&video)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("mpv must be on PATH");

    // O_NONBLOCK so a dead mpv is a timeout rather than a hang: a
    // blocking open of a fifo read end waits for a writer forever.
    let mut pipe = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .expect("open fifo");

    let frame_len = w * h * BPP;
    let mut frame = vec![0u8; frame_len];
    let mut filled = 0;
    let deadline = Instant::now() + Duration::from_secs(20);
    while filled < frame_len {
        // the fifo gotcha that cost an hour: with no writer attached
        // yet, a read on a fifo returns `Ok(0)`, i.e. a clean eof, not
        // `WouldBlock`. mpv has not finished opening its end at this
        // point, so zero has to mean "wait" here; mpv's liveness, not
        // the byte count, is what says it is over. `WouldBlock` is the
        // other case, a writer attached with nothing ready yet.
        let waited = match pipe.read(&mut frame[filled..]) {
            Ok(0) => true,
            Ok(n) => {
                filled += n;
                false
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => true,
            Err(e) => {
                eprintln!("read: {e}");
                break;
            }
        };
        if waited {
            if Instant::now() > deadline {
                break;
            }
            if let Ok(Some(status)) = child.try_wait() {
                eprintln!("mpv exited without handing over a frame: {status}");
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let elapsed = t0.elapsed();

    // only this probe's own child is ever signalled, by its own pid.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&fifo);
    let _ = std::fs::remove_dir(&dir);

    if filled < frame_len {
        eprintln!("incomplete frame: {filled} of {frame_len} bytes");
        std::process::exit(1);
    }

    let px = |x: usize, y: usize| {
        let i = (y * w + x) * BPP;
        (frame[i], frame[i + 1], frame[i + 2])
    };
    let mean = |c: usize| {
        frame[c..]
            .iter()
            .step_by(BPP)
            .map(|&b| u64::from(b))
            .sum::<u64>() as f64
            / (w * h) as f64
    };

    println!("source      {video}");
    println!("frame       {w}x{h} rgb24, {filled} bytes (w*h*3 = {frame_len})");
    println!("first frame {} ms after spawning mpv", elapsed.as_millis());
    println!("px(0,0)         {:?}", px(0, 0));
    println!("px(w/4,h/2)     {:?}", px(w / 4, h / 2));
    println!("px(w/2,h/2)     {:?}", px(w / 2, h / 2));
    println!("px(w-1,h-1)     {:?}", px(w - 1, h - 1));
    println!(
        "mean rgb        ({:.1}, {:.1}, {:.1})",
        mean(0),
        mean(1),
        mean(2)
    );
    println!(
        "non-black px    {} of {}",
        frame
            .chunks(BPP)
            .filter(|p| p.iter().any(|&b| b != 0))
            .count(),
        w * h
    );
}

/// `mkfifo(2)`; `std::fs` has no fifo.
fn mkfifo(path: &Path) {
    let c = CString::new(path.as_os_str().as_bytes()).expect("fifo path");
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    assert_eq!(
        rc,
        0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}
