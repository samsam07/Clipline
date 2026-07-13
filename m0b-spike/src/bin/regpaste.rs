//! M0b diagnostic — read the selection via the REGULAR `wl_data_device` path (what
//! normal apps like Kate/Dolphin use), to compare against the data-control read
//! (`wl-paste`). Isolates whether KWin applies a shorter timeout when bridging a
//! data-control *source* to a regular consumer. THROWAWAY.
//!
//! Usage: regpaste [mime]   (default text/plain). Run while the m0b-spike source
//! owns the selection.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::time::Instant;

use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_registry, wl_seat,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};

#[derive(Default)]
struct App {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    device: Option<wl_data_device::WlDataDevice>,
    current_offer: Option<wl_data_offer::WlDataOffer>,
    offered: Vec<String>,
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
                "wl_seat" => {
                    state.seat =
                        Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(4), qh, ()));
                }
                "wl_data_device_manager" => {
                    state.manager =
                        Some(registry.bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(
                            name,
                            version.min(3),
                            qh,
                            (),
                        ));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for App {
    fn event(_: &mut Self, _: &wl_data_device_manager::WlDataDeviceManager, _: wl_data_device_manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::Selection { id } => {
                state.current_offer = id;
            }
            wl_data_device::Event::DataOffer { .. } => {}
            _ => {}
        }
    }

    event_created_child!(App, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.offered.push(mime_type);
        }
    }
}

fn main() {
    let mime = std::env::args().nth(1).unwrap_or_else(|| "text/plain".to_string());
    let conn = Connection::connect_to_env().expect("connect to Wayland");
    let mut queue = conn.new_event_queue::<App>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut app = App::default();
    queue.roundtrip(&mut app).expect("roundtrip globals");

    let manager = app.manager.clone().expect("no wl_data_device_manager");
    let seat = app.seat.clone().expect("no wl_seat");
    let device = manager.get_data_device(&seat, &qh, ());
    app.device = Some(device);
    // Pump a couple of rounds so the compositor delivers the current selection offer.
    queue.roundtrip(&mut app).expect("roundtrip device");
    queue.roundtrip(&mut app).expect("roundtrip selection");

    let offer = match app.current_offer.clone() {
        Some(o) => o,
        None => {
            println!("regpaste: NO selection offer present");
            return;
        }
    };
    println!("regpaste: selection offers {:?}; requesting '{mime}'", app.offered);

    let (read_fd, write_fd) = rustix::pipe::pipe().expect("pipe");
    offer.receive(mime.clone(), write_fd.as_fd());
    conn.flush().expect("flush receive");
    drop(write_fd); // close our write end so EOF works when the source closes

    let t0 = Instant::now();
    let mut buf = Vec::new();
    match File::from(read_fd).read_to_end(&mut buf) {
        Ok(n) => println!(
            "regpaste: GOT {n} bytes in {} ms → \"{}\"",
            t0.elapsed().as_millis(),
            String::from_utf8_lossy(&buf).chars().take(60).collect::<String>()
        ),
        Err(e) => println!(
            "regpaste: READ FAILED after {} ms → {e}",
            t0.elapsed().as_millis()
        ),
    }
}
