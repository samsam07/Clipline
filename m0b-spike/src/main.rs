//! M0b spike — Linux/KDE-Wayland lazy `on_render` bridge via **ext-data-control**.
//! THROWAWAY / not committed. The Wayland twin of the Windows M0a spike.
//!
//! We own the selection and advertise `text/plain(;charset=utf-8)` + `text/uri-list`
//! with NO bytes. When a paste occurs, the compositor sends our source a
//! `send(mime, fd)` event — THE inversion. We block the wayland dispatch thread
//! while a tokio task produces the bytes (a `sleep` stands in for the network),
//! then write the fd — or, on timeout, close the fd empty (graceful paste-fail).
//!
//! Serve mode (default): set the selection and pump events for `--run-secs` while a
//! real consumer (`wl-paste`, or Kate/Dolphin) pastes. Every `send` is logged, so we
//! can also SEE whether the Plasma clipboard manager force-reads us at copy time
//! (the Wayland analog of the Windows Finding B/C).
//!
//! Flags: --delay-ms N (default 1500), --timeout-ms N (default 5000),
//!        --text-bytes N (default 80), --run-secs N (default 8).

use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::time::{sleep, timeout};

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

static STARTED: OnceLock<Instant> = OnceLock::new();

fn log(msg: &str) {
    let t = STARTED.get().map(|s| s.elapsed().as_millis()).unwrap_or(0);
    eprintln!("[{t:>6} ms] {msg}");
}

struct App {
    manager: Option<ExtDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    source: Option<ExtDataControlSourceV1>,
    handle: Handle,
    delay: Duration,
    render_timeout: Duration,
    text_bytes: usize,
}

/// The "network fetch" result (runs after the simulated delay). Free function so it
/// can be moved into a spawned task (no borrow of `App`).
fn produce(mime: &str, delay: Duration, text_bytes: usize) -> Vec<u8> {
    if mime.starts_with("text/uri-list") {
        // Linux file-by-reference analog: materialize a local staging file and
        // advertise its file:// URI (contents live on disk, not in the clipboard).
        let dir = std::env::temp_dir().join("clipline-m0b-spike");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("lazy-payload-{}.txt", std::process::id()));
        let _ = std::fs::write(
            &path,
            b"Clipline M0b: this file's bytes were materialized lazily on paste.\n",
        );
        format!("file://{}\r\n", path.display()).into_bytes()
    } else {
        let base = format!("Hello from Clipline M0b — served LAZILY on paste (delay {} ms). ", delay.as_millis());
        let mut s = base.clone();
        while s.len() < text_bytes {
            s.push_str(&base);
        }
        s.truncate(text_bytes.max(base.len()));
        s.into_bytes()
    }
}

impl App {
    /// THE inversion, Wayland-style. Unlike Windows `WM_RENDERFORMAT` (which requires
    /// blocking the platform thread), the data-control `send` must NOT block the
    /// dispatch thread: readers have short timeouts and concurrent reads would
    /// serialize. So we hand the fd to an async task that fetches (with a timeout)
    /// and writes it whenever ready — the dispatch loop keeps running. On timeout we
    /// drop the fd (reader sees EOF → graceful paste-fail).
    fn handle_send(&self, mime: String, fd: OwnedFd) {
        log(&format!(
            "send('{mime}') → dispatched async fetch (non-blocking; timeout {} ms)…",
            self.render_timeout.as_millis()
        ));
        let (delay, render_timeout, text_bytes) = (self.delay, self.render_timeout, self.text_bytes);
        self.handle.spawn(async move {
            let produced = timeout(render_timeout, async {
                sleep(delay).await;
                produce(&mime, delay, text_bytes)
            })
            .await;
            match produced {
                Ok(bytes) => {
                    let n = bytes.len();
                    let res = tokio::task::spawn_blocking(move || {
                        File::from(fd).write_all(&bytes)
                    })
                    .await;
                    match res {
                        Ok(Ok(())) => log(&format!("served '{mime}' lazily → wrote {n} bytes to fd")),
                        Ok(Err(e)) => log(&format!("write to fd failed for '{mime}': {e}")),
                        Err(e) => log(&format!("write task join error for '{mime}': {e}")),
                    }
                }
                Err(_) => {
                    log(&format!(
                        "TIMEOUT producing '{mime}' after {} ms → closing fd empty (graceful paste-fail)",
                        render_timeout.as_millis()
                    ));
                    drop(fd);
                }
            }
        });
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "ext_data_control_manager_v1" => {
                    state.manager = Some(registry.bind::<ExtDataControlManagerV1, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    ));
                }
                "wl_seat" => {
                    state.seat =
                        Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(4), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for App {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.handle_send(mime_type, fd)
            }
            ext_data_control_source_v1::Event::Cancelled => {
                log("source Cancelled — lost selection ownership");
                source.destroy();
                state.source = None;
            }
            _ => {}
        }
    }
}

// We must own a data device to call set_selection; it emits data_offer/selection
// events about OTHER selections, which we ignore (we're a source, not a consumer).
impl Dispatch<ExtDataControlDeviceV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ExtDataControlDeviceV1,
        _: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    event_created_child!(App, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        _: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn arg_num(args: &[String], flag: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    STARTED.set(Instant::now()).ok();
    let args: Vec<String> = std::env::args().collect();
    let delay = Duration::from_millis(arg_num(&args, "--delay-ms", 1500));
    let render_timeout = Duration::from_millis(arg_num(&args, "--timeout-ms", 5000));
    let text_bytes = arg_num(&args, "--text-bytes", 80) as usize;
    let run_secs = arg_num(&args, "--run-secs", 8);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let conn = Connection::connect_to_env().expect("connect to Wayland ($WAYLAND_DISPLAY)");
    let mut queue = conn.new_event_queue::<App>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut app = App {
        manager: None,
        seat: None,
        source: None,
        handle: rt.handle().clone(),
        delay,
        render_timeout,
        text_bytes,
    };
    queue.roundtrip(&mut app).expect("roundtrip (bind globals)");

    log(&format!(
        "M0b start — delay={} ms, timeout={} ms, text_bytes={}, run_secs={}, will_timeout={}",
        delay.as_millis(),
        render_timeout.as_millis(),
        text_bytes,
        run_secs,
        delay >= render_timeout,
    ));

    let manager = app.manager.clone().expect("no ext_data_control_manager_v1");
    let seat = app.seat.clone().expect("no wl_seat");

    let source = manager.create_data_source(&qh, ());
    for m in ["text/plain;charset=utf-8", "text/plain", "text/uri-list"] {
        source.offer(m.to_string());
    }
    app.source = Some(source.clone());

    let device = manager.get_data_device(&seat, &qh, ());
    device.set_selection(Some(&source));
    log("selection set: offering text/plain(;charset=utf-8) + text/uri-list, NO bytes rendered");
    queue.roundtrip(&mut app).expect("roundtrip (set selection)");

    // Watchdog: bound the spike's lifetime so it exits cleanly for scripted runs.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(run_secs));
        log("run duration elapsed — exiting");
        std::process::exit(0);
    });

    loop {
        if let Err(e) = queue.blocking_dispatch(&mut app) {
            log(&format!("dispatch error: {e}"));
            break;
        }
    }
}
