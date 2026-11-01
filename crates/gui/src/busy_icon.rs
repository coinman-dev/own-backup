//! The moving logo that says «a job is running»: in the notification area, and on
//! the window's taskbar button on Windows. Also the message from the system when a
//! job starts: the service has no desktop to show one on, so the icon's process,
//! which belongs to the person who is logged in, shows it.
//!
//! The frames are one strip, `icons/busy-frames.png`, made from
//! `images/own_backup_512_fit.webp` (see images/README.md). Whether a job runs is the
//! service's run queue, asked every few seconds from a thread of its own, so that
//! neither the tray's message loop nor the window waits for an answer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use own_backup_common::config::AppConfig;

use crate::service_link::ServiceLink;

/// One frame after another at this pace: a turn of the arrow in about 2.4 s.
#[cfg(windows)]
pub const FRAME_MS: u32 = 150;
/// How often the service is asked. A job that started is noticed in this time.
const ASK_EVERY: Duration = Duration::from_secs(3);

const STRIP: &[u8] = include_bytes!("../icons/busy-frames.png");

/// The frames as RGBA, one under another.
pub struct Frames {
    /// The side of one square frame.
    pub side: u32,
    pub count: usize,
    rgba: Vec<u8>,
}

impl Frames {
    /// The strip built into the program; `None` only if it is broken, and then
    /// the icon simply stands still.
    pub fn load() -> Option<Self> {
        let decoder = png::Decoder::new(std::io::Cursor::new(STRIP));
        let mut reader = decoder.read_info().ok()?;
        let mut rgba = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut rgba).ok()?;
        if info.color_type != png::ColorType::Rgba
            || info.bit_depth != png::BitDepth::Eight
            || info.width == 0
            || info.height % info.width != 0
        {
            return None;
        }
        rgba.truncate(info.buffer_size());
        Some(Self {
            side: info.width,
            count: (info.height / info.width) as usize,
            rgba,
        })
    }

    /// Frame `index` as it is stored.
    pub fn frame(&self, index: usize) -> &[u8] {
        let bytes = (self.side * self.side * 4) as usize;
        &self.rgba[index * bytes..(index + 1) * bytes]
    }

    /// Frame `index` at `side` pixels: what the system asks for depends on the
    /// scale of the screen.
    pub fn scaled(&self, index: usize, side: u32) -> Vec<u8> {
        shrink(self.frame(index), self.side, side)
    }
}

/// A square RGBA image brought down to `to` pixels: each new pixel is the average
/// of the old ones it covers, weighted by their alpha, or the colour hidden under
/// transparent pixels would bleed into the edge.
fn shrink(rgba: &[u8], from: u32, to: u32) -> Vec<u8> {
    let (from, to) = (from as usize, to.max(1) as usize);
    let mut out = Vec::with_capacity(to * to * 4);
    for y in 0..to {
        let (y0, y1) = (y * from / to, ((y + 1) * from).div_ceil(to).min(from));
        for x in 0..to {
            let (x0, x1) = (x * from / to, ((x + 1) * from).div_ceil(to).min(from));
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = &rgba[(sy * from + sx) * 4..(sy * from + sx) * 4 + 4];
                    let alpha = p[3] as u64;
                    r += p[0] as u64 * alpha;
                    g += p[1] as u64 * alpha;
                    b += p[2] as u64 * alpha;
                    a += alpha;
                    n += 1;
                }
            }
            out.extend_from_slice(
                &match (r.checked_div(a), g.checked_div(a), b.checked_div(a)) {
                    (Some(r), Some(g), Some(b)) => [r as u8, g as u8, b as u8, (a / n) as u8],
                    _ => [0, 0, 0, 0],
                },
            );
        }
    }
    out
}

/// What the service last said about its jobs.
#[derive(Default)]
pub struct JobWatch {
    busy: AtomicBool,
    started: Mutex<Option<String>>,
}

impl JobWatch {
    /// A job is running.
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    /// The message about a job that has started, once.
    pub fn take_started(&self) -> Option<String> {
        self.started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// Starts asking the service. An answer that does not come reads as «no job»: an
/// icon standing still is the honest default, and only a person the service lets
/// in can be told otherwise anyway.
pub fn watch_jobs() -> Arc<JobWatch> {
    let watch = Arc::new(JobWatch::default());
    let shared = watch.clone();
    let spawned = std::thread::Builder::new()
        .name("busy-icon".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::warn!(%error, "busy icon: no runtime, the icon stands still");
                    return;
                }
            };
            let config = AppConfig::load_or_default();
            let link = ServiceLink::new(&config.grpc_address, config.token_path());
            runtime.block_on(async {
                let mut seen = None;
                loop {
                    let answer = current_job(&link).await;
                    shared
                        .busy
                        .store(matches!(answer, Some(Some(_))), Ordering::Relaxed);
                    if let Some(id) = answer.and_then(|now| newly_started(&mut seen, now)) {
                        let name = job_name(&link, &id).await;
                        *shared
                            .started
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some(started_message(name.as_deref()));
                    }
                    tokio::time::sleep(ASK_EVERY).await;
                }
            });
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "busy icon: no thread, the icon stands still");
    }
    watch
}

/// The job the service is running; `None` if it did not answer. Asked without
/// starting the service, which `client()` would try to do: this is a decoration,
/// not a reason to touch it.
async fn current_job(link: &ServiceLink) -> Option<Option<String>> {
    let mut client = link.connected().await?;
    match client.get_queue().await {
        Ok(queue) => Some(queue.current),
        Err(_) => {
            link.invalidate().await;
            None
        }
    }
}

/// The job to tell about, given the service's answer. The first answer only sets
/// the starting point: a job already running when the icon appeared did not start
/// now. A silence of the service does not come here, so it does not repeat a message.
fn newly_started(seen: &mut Option<Option<String>>, now: Option<String>) -> Option<String> {
    let before = seen.replace(now.clone())?;
    now.filter(|id| before.as_ref() != Some(id))
}

async fn job_name(link: &ServiceLink, id: &str) -> Option<String> {
    let job = link.connected().await?.get_job(id).await.ok()??;
    Some(job.name).filter(|name| !name.is_empty())
}

fn started_message(name: Option<&str>) -> String {
    match name {
        Some(name) => own_backup_common::tr!("tray.job_started", name = name).into_owned(),
        None => own_backup_common::tr!("tray.job_started_unnamed").into_owned(),
    }
}

/// Windows: the same movement on the window's taskbar button, for as long as the
/// window exists. Not on the boot medium, whose strip draws no icons from here.
#[cfg(windows)]
pub fn animate_window(window: tauri::WebviewWindow, idle: Option<tauri::image::Image<'static>>) {
    if own_backup_common::media_mode::is_media_mode() {
        return;
    }
    let Some(frames) = Frames::load() else {
        return;
    };
    let jobs = watch_jobs();
    let spawned = std::thread::Builder::new()
        .name("busy-window-icon".into())
        .spawn(move || {
            let (mut index, mut moving) = (0, false);
            loop {
                if jobs.busy() {
                    let image =
                        tauri::image::Image::new(frames.frame(index), frames.side, frames.side);
                    if window.set_icon(image).is_err() {
                        return; // the window is gone
                    }
                    index = (index + 1) % frames.count;
                    moving = true;
                    std::thread::sleep(Duration::from_millis(FRAME_MS as u64));
                } else {
                    if std::mem::take(&mut moving) {
                        if let Some(image) = idle.clone() {
                            let _ = window.set_icon(image);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "busy icon: no thread for the window's icon");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str) -> Option<String> {
        Some(id.to_string())
    }

    #[test]
    fn a_job_is_told_about_once_when_it_starts() {
        let mut seen = None;
        assert_eq!(newly_started(&mut seen, None), None);
        assert_eq!(newly_started(&mut seen, job("a")), job("a"));
        assert_eq!(newly_started(&mut seen, job("a")), None);
        assert_eq!(newly_started(&mut seen, None), None);
        assert_eq!(newly_started(&mut seen, job("a")), job("a"));
    }

    #[test]
    fn the_next_job_of_the_queue_is_told_about_too() {
        let mut seen = None;
        assert_eq!(newly_started(&mut seen, job("a")), None);
        assert_eq!(newly_started(&mut seen, job("b")), job("b"));
    }

    #[test]
    fn a_job_already_running_when_the_icon_appeared_is_not_news() {
        let mut seen = None;
        assert_eq!(newly_started(&mut seen, job("a")), None);
        assert_eq!(newly_started(&mut seen, job("a")), None);
    }

    #[test]
    fn the_message_is_taken_once() {
        let watch = JobWatch::default();
        assert_eq!(watch.take_started(), None);
        *watch.started.lock().unwrap() = Some("x".into());
        assert_eq!(watch.take_started().as_deref(), Some("x"));
        assert_eq!(watch.take_started(), None);
    }

    #[test]
    fn the_message_names_the_job() {
        assert!(started_message(Some("Disk_C")).contains("Disk_C"));
        assert!(!started_message(None).contains("%{"));
    }

    #[test]
    fn the_built_in_strip_is_whole() {
        let frames = Frames::load().expect("the strip decodes");
        assert_eq!(frames.side, 48);
        assert_eq!(frames.count, 16);
        for i in 0..frames.count {
            assert_eq!(frames.frame(i).len(), 48 * 48 * 4);
            assert!(
                frames.frame(i).chunks(4).any(|p| p[3] == 255),
                "frame {i} has something on it"
            );
        }
    }

    #[test]
    fn the_frames_differ_so_the_icon_moves() {
        let frames = Frames::load().unwrap();
        for i in 1..frames.count {
            assert_ne!(frames.frame(i - 1), frames.frame(i), "frame {i}");
        }
    }

    #[test]
    fn the_logo_reaches_the_sides_of_every_frame() {
        let frames = Frames::load().unwrap();
        let side = frames.side as usize;
        for i in 0..frames.count {
            let frame = frames.frame(i);
            let column_used = |x: usize| (0..side).any(|y| frame[(y * side + x) * 4 + 3] > 16);
            assert!(column_used(0) && column_used(side - 1), "frame {i}");
        }
    }

    #[test]
    fn scaling_gives_the_size_asked_for() {
        let frames = Frames::load().unwrap();
        for side in [16u32, 20, 24, 32, 48] {
            assert_eq!(frames.scaled(0, side).len(), (side * side * 4) as usize);
        }
    }

    #[test]
    fn transparency_does_not_bleed_its_hidden_colour() {
        // Half opaque red, half fully transparent green.
        let image = [255, 0, 0, 255, 0, 255, 0, 0, 255, 0, 0, 255, 0, 255, 0, 0];
        let one = shrink(&image, 2, 1);
        assert_eq!(
            &one[..3],
            &[255, 0, 0],
            "no green from the transparent half"
        );
        assert_eq!(one[3], 127);
    }

    #[test]
    fn nothing_stays_nothing() {
        let image = [9u8, 9, 9, 0].repeat(16);
        assert!(shrink(&image, 4, 2).iter().all(|&b| b == 0));
    }
}
