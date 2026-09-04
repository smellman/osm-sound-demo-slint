//! Wiring between the Slint `MMapAdapter` global and the headless renderer.

use std::cell::RefCell;
use std::rc::Rc;

use slint::ComponentHandle;

use crate::AppWindow;
use crate::MMapAdapter;

mod renderer;
pub use renderer::{MapLibre, create_map};

/// Publishes the newest rendered frame and the camera state to the UI, and
/// reports whether a new frame arrived.
pub fn push_state(ui: &AppWindow, map: &mut MapLibre) -> bool {
    let adapter = ui.global::<MMapAdapter>();

    let frame = map.take_frame();
    if let Some(frame) = &frame {
        let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            &frame.rgba,
            frame.width,
            frame.height,
        );
        adapter.set_frame(slint::Image::from_rgba8(pixels));
    }

    let camera = map.camera();
    adapter.set_current_lat(camera.lat as f32);
    adapter.set_current_lon(camera.lon as f32);
    adapter.set_current_zoom(camera.zoom as f32);
    adapter.set_current_bearing(camera.bearing as f32);
    adapter.set_current_pitch(camera.pitch as f32);
    adapter.set_style_loaded(map.style_loaded());
    adapter.set_map_idle(map.map_idle());

    frame.is_some()
}

/// Connects the map's input and command callbacks. The per-frame `tick` is
/// owned by [`crate::app`], which also feeds the audio levels into the map.
pub fn init(ui: &AppWindow, map: &Rc<RefCell<MapLibre>>) {
    let ui_handle = ui.as_weak();
    let adapter = ui.global::<MMapAdapter>();

    // `MMapView` publishes its declared style and camera on init, before the
    // backend exists; adopt them so the map opens where the UI asked for.
    if adapter.get_initial_config_set() {
        map.borrow_mut().apply_initial(
            adapter.get_initial_style_url().as_str(),
            f64::from(adapter.get_initial_lat()),
            f64::from(adapter.get_initial_lon()),
            f64::from(adapter.get_initial_zoom()),
            f64::from(adapter.get_initial_bearing()),
            f64::from(adapter.get_initial_pitch()),
        );
    }

    ui.on_map_size_changed({
        let map = Rc::downgrade(map);
        let ui_handle = ui_handle.clone();
        move || {
            if let (Some(map), Some(ui)) = (map.upgrade(), ui_handle.upgrade()) {
                map.borrow_mut().resize(ui.get_map_size());
            }
        }
    });

    adapter.on_mouse_pressed({
        let map = Rc::downgrade(map);
        move |x, y| {
            if let Some(map) = map.upgrade() {
                map.borrow_mut().mouse_pressed(x, y);
            }
        }
    });

    adapter.on_mouse_released({
        let map = Rc::downgrade(map);
        move |_x, _y| {
            if let Some(map) = map.upgrade() {
                map.borrow_mut().mouse_released();
            }
        }
    });

    adapter.on_mouse_moved({
        let map = Rc::downgrade(map);
        move |x, y| {
            if let Some(map) = map.upgrade() {
                map.borrow_mut().mouse_moved(x, y);
            }
        }
    });

    adapter.on_double_clicked({
        let map = Rc::downgrade(map);
        move |_x, _y, shift| {
            if let Some(map) = map.upgrade() {
                map.borrow_mut().double_clicked(shift);
            }
        }
    });

    adapter.on_wheel_zoomed({
        let map = Rc::downgrade(map);
        move |_x, _y, delta| {
            if let Some(map) = map.upgrade() {
                map.borrow_mut().wheel_zoomed(delta);
            }
        }
    });

    adapter.on_request_style_change({
        let map = Rc::downgrade(map);
        move |style_url| {
            if let Some(map) = map.upgrade() {
                map.borrow_mut().load_style(&style_url);
            }
        }
    });

    adapter.on_request_fly_to({
        let map = Rc::downgrade(map);
        move |lat, lon, zoom| {
            if let Some(map) = map.upgrade() {
                map.borrow_mut()
                    .fly_to(f64::from(lat), f64::from(lon), f64::from(zoom));
            }
        }
    });

    adapter.on_request_zoom_change({
        let map = Rc::downgrade(map);
        move |zoom| {
            if let Some(map) = map.upgrade() {
                map.borrow_mut().set_zoom(f64::from(zoom));
            }
        }
    });

    adapter.on_request_pitch_change({
        let map = Rc::downgrade(map);
        move |pitch| {
            if let Some(map) = map.upgrade() {
                map.borrow_mut().set_pitch(f64::from(pitch));
            }
        }
    });

    adapter.on_request_bearing_change({
        let map = Rc::downgrade(map);
        move |bearing| {
            if let Some(map) = map.upgrade() {
                map.borrow_mut().set_bearing(f64::from(bearing));
            }
        }
    });
}
