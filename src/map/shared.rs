//! Frames handed to Slint as textures instead of being read back.
//!
//! The map renders into a ring of textures the window's device can sample —
//! Skia drawing through wgpu — and each is wrapped as a `wgpu::Texture`, which
//! `slint::Image` takes as it is. A frame then costs the UI thread nothing: no
//! readback, no copy into a pixel buffer, and no upload for the window
//! renderer to make again. How the ring reaches the window's device is the
//! backend's business: Vulkan exports the images' memory and imports it into
//! a second device (see `vulkan`), Metal renders on the window's own device
//! (see `metal`).
//!
//! Each frame is acquired from the session while the UI shows it, so MapLibre
//! Native never draws into an image that is on screen. The UI releases it only
//! once the GPU has finished every draw that sampled it; see [`Bridge`].

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use maplibre_native_ffi::AcquiredFrameHandle;
use slint::wgpu_30::wgpu;

#[cfg(feature = "metal")]
mod metal;
#[cfg(feature = "vulkan")]
mod vulkan;
#[cfg(feature = "metal")]
use metal as backend;
#[cfg(feature = "metal")]
pub use metal::*;
#[cfg(feature = "vulkan")]
use vulkan as backend;
#[cfg(feature = "vulkan")]
pub use vulkan::*;

/// Images in a ring. One is drawn into, one is on screen, one or two wait for
/// the GPU to finish sampling them, and one may sit in the frame channel.
pub const RING_DEPTH: usize = 5;

/// A frame drawn into a slot of a ring, acquired so the session leaves the
/// slot alone until it is dropped.
pub struct SharedFrame {
    ring: Arc<Ring>,
    slot: usize,
    handle: Option<AcquiredFrameHandle>,
}

impl SharedFrame {
    pub fn new(ring: Arc<Ring>, slot: usize, handle: AcquiredFrameHandle) -> Self {
        Self {
            ring,
            slot,
            handle: Some(handle),
        }
    }
}

impl Drop for SharedFrame {
    /// Hands the slot back. A frame is only dropped once nothing the GPU still
    /// has to do reads it, which is what a release without a fence states.
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take()
            && let Err((handle, error)) = handle.release(None)
        {
            eprintln!("releasing a map frame failed: {error}");
            // Dropping it discards it, which quarantines the slot rather than
            // let the map draw into an image that may still be read.
            drop(handle);
        }
    }
}

/// The window's wgpu device, once Slint has set it up, if it can take the
/// map's frames.
struct Consumer {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

/// The UI side: imports each ring into the window's device and keeps every
/// frame acquired until the GPU has finished the draws that sampled it.
#[derive(Default)]
pub struct Bridge {
    consumer: Option<Consumer>,
    /// Slint has set the window's renderer up, so `consumer` is final.
    set_up: bool,
    /// The ring the textures below were imported from, and the textures.
    imported: Option<(u64, Vec<wgpu::Texture>)>,
    /// The frame Slint is showing.
    shown: Option<SharedFrame>,
    /// Frames taken off screen since the last draw. A draw already submitted
    /// may still be sampling them.
    retiring: Vec<SharedFrame>,
}

impl Bridge {
    /// Installs the rendering notifier through which the bridge learns the
    /// window's device and when each draw has been submitted.
    pub fn attach(bridge: &Rc<RefCell<Self>>, window: &slint::Window) {
        let bridge = Rc::downgrade(bridge);
        let result = window.set_rendering_notifier(move |state, api| {
            let Some(bridge) = bridge.upgrade() else {
                return;
            };
            bridge.borrow_mut().notify(state, api);
        });
        if let Err(error) = result {
            eprintln!("watching the window's rendering failed: {error:?}");
        }
    }

    fn notify(&mut self, state: slint::RenderingState, api: &slint::GraphicsAPI) {
        match state {
            slint::RenderingState::RenderingSetup => {
                self.set_up = true;
                if let slint::GraphicsAPI::WGPU30 { device, queue, .. } = api {
                    match backend::accepts(device) {
                        Ok(()) => {
                            self.consumer = Some(Consumer {
                                device: device.clone(),
                                queue: queue.clone(),
                            });
                        }
                        Err(reason) => eprintln!("{reason}; map frames cannot be shared"),
                    }
                }
            }
            slint::RenderingState::AfterRendering => self.retire(),
            slint::RenderingState::RenderingTeardown => {
                self.retire();
                self.consumer = None;
                self.imported = None;
            }
            _ => {}
        }
    }

    /// Releases the frames taken off screen once the GPU work submitted so far
    /// has finished.
    ///
    /// This runs while Slint is still drawing, before Skia submits this draw,
    /// so it covers the draws before it — and only those can have sampled a
    /// frame that left the screen before this one began. Skia submits to the
    /// same queue as wgpu, and a queue signals completion of a submission only
    /// once everything submitted to it earlier has completed too.
    fn retire(&mut self) {
        if self.retiring.is_empty() {
            return;
        }
        let frames = std::mem::take(&mut self.retiring);
        match &self.consumer {
            Some(consumer) => {
                consumer.queue.on_submitted_work_done(move || drop(frames));
                let _ = consumer.device.poll(wgpu::PollType::Poll);
            }
            // No device, so nothing can be sampling them.
            None => drop(frames),
        }
    }

    /// Whether the window has been set up with a renderer that cannot take
    /// shared frames.
    pub fn refused(&self) -> bool {
        self.set_up && self.consumer.is_none()
    }

    /// Hands back every frame of a disposed session at once, the one on
    /// screen included: nothing draws into them any more, and the session
    /// waits for them before the map can take its replacement. The image stays
    /// on screen, since the window's device holds its own reference to the
    /// memory.
    pub fn let_go_of_disposed(&mut self) {
        if self
            .shown
            .as_ref()
            .is_some_and(|frame| frame.ring.disposed())
        {
            self.shown = None;
        }
        self.retiring.retain(|frame| !frame.ring.disposed());
    }

    /// Hands back every frame, for when the map is shutting down.
    pub fn let_go_of_all(&mut self) {
        self.shown = None;
        self.retiring.clear();
    }

    /// Puts a frame on screen, returning the image to show.
    pub fn show(&mut self, frame: SharedFrame) -> Option<slint::Image> {
        let consumer = self.consumer.as_ref()?;
        if self.imported.as_ref().map(|(id, _)| *id) != Some(frame.ring.id()) {
            // The old ring's textures go when nothing samples them any more;
            // wgpu holds on to them until its submitted work is done.
            self.imported = None;
            match backend::import(&consumer.device, &frame.ring) {
                Ok(textures) => self.imported = Some((frame.ring.id(), textures)),
                Err(error) => {
                    eprintln!("importing the map's images failed: {error}");
                    return None;
                }
            }
        }
        let (_, textures) = self.imported.as_ref()?;
        let texture = textures.get(frame.slot)?.clone();
        let image = match slint::Image::try_from(texture) {
            Ok(image) => image,
            Err(error) => {
                eprintln!("wrapping a map frame failed: {error:?}");
                return None;
            }
        };
        if let Some(previous) = self.shown.replace(frame) {
            self.retiring.push(previous);
        }
        Some(image)
    }
}
