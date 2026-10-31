//! Frames handed to Slint as textures instead of being read back.
//!
//! The map's Vulkan device renders into a ring of images whose memory is
//! exported as file descriptors. The window's own device — Skia drawing through
//! wgpu — imports that memory once per ring and wraps each image as a
//! `wgpu::Texture`, which `slint::Image` takes as it is. A frame then costs the
//! UI thread nothing: no 9 MB readback, no copy into a pixel buffer, and no
//! upload for the window renderer to make again.
//!
//! The two are separate devices because they cannot share one: this GPU has a
//! single graphics queue, MapLibre Native submits to it from its own thread,
//! and wgpu offers no lock to take around its own submissions.
//!
//! Each frame is acquired from the session while the UI shows it, so MapLibre
//! Native never draws into an image that is on screen. The UI releases it only
//! once the GPU has finished every draw that sampled it; see [`Bridge`].

use std::cell::RefCell;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ash::vk;
use maplibre_native_ffi::AcquiredFrameHandle;
use slint::wgpu_30::wgpu;

/// Images in a ring. One is drawn into, one is on screen, one or two wait for
/// the GPU to finish sampling them, and one may sit in the frame channel.
pub const RING_DEPTH: usize = 5;

pub const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
const HANDLE_TYPE: vk::ExternalMemoryHandleTypeFlags = vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD;

/// What the images are created for, identically on both devices: imported
/// memory has to back an image made the same way as the one it was exported
/// from.
fn usage() -> vk::ImageUsageFlags {
    vk::ImageUsageFlags::COLOR_ATTACHMENT
        | vk::ImageUsageFlags::SAMPLED
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::TRANSFER_DST
}

fn image_info<'a>(
    width: u32,
    height: u32,
    external: &'a mut vk::ExternalMemoryImageCreateInfo<'a>,
) -> vk::ImageCreateInfo<'a> {
    vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(FORMAT)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage())
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(external)
}

fn device_local_type(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    allowed: u32,
) -> Option<u32> {
    // SAFETY: physical_device belongs to instance.
    let properties = unsafe { instance.get_physical_device_memory_properties(physical_device) };
    (0..properties.memory_type_count).find(|&index| {
        allowed & (1 << index) != 0
            && properties.memory_types[index as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    })
}

/// The device the map renders with, as the ring needs it.
pub struct Producer<'a> {
    pub instance: &'a ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: &'a ash::Device,
}

struct Slot {
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
}

/// One slot's memory, for the window's device to import.
struct Export {
    fd: OwnedFd,
    size: u64,
}

/// The images the map renders into, on the map's own device.
///
/// Shared between the render thread, which attached them, and every frame
/// drawn into them, so they outlive both the session and any frame still on
/// screen.
pub struct Ring {
    id: u64,
    width: u32,
    height: u32,
    device: ash::Device,
    slots: Vec<Slot>,
    /// Taken by the UI the first time it sees a frame of this ring.
    exports: Mutex<Option<Vec<Export>>>,
    /// The session drawing into the ring has been disposed of, so it draws
    /// nothing more and its frames can be handed back while still on screen.
    /// It only retires once they are, and the map takes no new session before.
    disposed: AtomicBool,
}

impl Ring {
    pub fn new(producer: &Producer, width: u32, height: u32) -> Result<Arc<Self>, String> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let mut ring = Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            device: producer.device.clone(),
            slots: Vec::with_capacity(RING_DEPTH),
            exports: Mutex::new(None),
            disposed: AtomicBool::new(false),
        };
        let fd_device =
            ash::khr::external_memory_fd::Device::new(producer.instance, producer.device);
        let mut exports = Vec::with_capacity(RING_DEPTH);
        for _ in 0..RING_DEPTH {
            // Pushed as it goes, so a failure part-way still frees what was made.
            let (slot, export) = ring.make_slot(producer, &fd_device)?;
            ring.slots.push(slot);
            exports.push(export);
        }
        *ring.exports.get_mut().unwrap() = Some(exports);
        Ok(Arc::new(ring))
    }

    fn make_slot(
        &self,
        producer: &Producer,
        fd_device: &ash::khr::external_memory_fd::Device,
    ) -> Result<(Slot, Export), String> {
        let device = producer.device;
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(HANDLE_TYPE);
        let info = image_info(self.width, self.height, &mut external);
        // SAFETY: info describes a valid 2D image.
        let image = unsafe { device.create_image(&info, None) }.map_err(|e| e.to_string())?;
        let mut slot = Slot {
            image,
            view: vk::ImageView::null(),
            memory: vk::DeviceMemory::null(),
        };
        let result = (|| {
            // SAFETY: image was created on device above.
            let requirements = unsafe { device.get_image_memory_requirements(image) };
            let memory_type = device_local_type(
                producer.instance,
                producer.physical_device,
                requirements.memory_type_bits,
            )
            .ok_or("no device-local memory type for the ring")?;
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(HANDLE_TYPE);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let allocate = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            // SAFETY: the chain names this device's image and a type it allows.
            slot.memory =
                unsafe { device.allocate_memory(&allocate, None) }.map_err(|e| e.to_string())?;
            // SAFETY: the memory was allocated for this image.
            unsafe { device.bind_image_memory(image, slot.memory, 0) }
                .map_err(|e| e.to_string())?;
            let view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(FORMAT)
                .subresource_range(color_range());
            // SAFETY: image is bound and the view matches it.
            slot.view =
                unsafe { device.create_image_view(&view_info, None) }.map_err(|e| e.to_string())?;
            let get_fd = vk::MemoryGetFdInfoKHR::default()
                .memory(slot.memory)
                .handle_type(HANDLE_TYPE);
            // SAFETY: the memory was allocated exportable as this handle type.
            let fd = unsafe { fd_device.get_memory_fd(&get_fd) }.map_err(|e| e.to_string())?;
            // SAFETY: vkGetMemoryFdKHR hands over a new descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            Ok(Export {
                fd,
                size: requirements.size,
            })
        })();
        match result {
            Ok(export) => Ok((slot, export)),
            Err(error) => {
                // SAFETY: nothing has used the slot yet.
                unsafe { destroy_slot(device, &slot) };
                Err(error)
            }
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Marks the ring's session as disposed of; see `disposed`.
    pub fn dispose(&self) {
        self.disposed.store(true, Ordering::Release);
    }

    fn disposed(&self) -> bool {
        self.disposed.load(Ordering::Acquire)
    }

    /// The images and views to attach, in slot order.
    pub fn textures(&self) -> Vec<(u64, u64)> {
        use vk::Handle;
        self.slots
            .iter()
            .map(|slot| (slot.image.as_raw(), slot.view.as_raw()))
            .collect()
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// # Safety
/// The GPU must be done with the slot.
unsafe fn destroy_slot(device: &ash::Device, slot: &Slot) {
    unsafe {
        if slot.view != vk::ImageView::null() {
            device.destroy_image_view(slot.view, None);
        }
        device.destroy_image(slot.image, None);
        if slot.memory != vk::DeviceMemory::null() {
            device.free_memory(slot.memory, None);
        }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // The last reference goes once the session that drew into the ring
        // has detached and every frame of it has been released, so neither
        // device is using the images any more. The imported copies on the
        // window's device have their own memory objects and go with their
        // textures.
        for slot in &self.slots {
            // SAFETY: as above.
            unsafe { destroy_slot(&self.device, slot) };
        }
    }
}

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

/// The window's wgpu device, once Slint has set it up, if it is Vulkan.
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
                    // SAFETY: only asks which backend the device is on.
                    let vulkan = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }.is_some();
                    if vulkan {
                        self.consumer = Some(Consumer {
                            device: device.clone(),
                            queue: queue.clone(),
                        });
                    } else {
                        eprintln!(
                            "the window is not drawn with Vulkan; map frames cannot be shared"
                        );
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
        if self.imported.as_ref().map(|(id, _)| *id) != Some(frame.ring.id) {
            // The old ring's textures go when nothing samples them any more;
            // wgpu holds on to them until its submitted work is done.
            self.imported = None;
            match import(consumer, &frame.ring) {
                Ok(textures) => self.imported = Some((frame.ring.id, textures)),
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

/// Imports a ring's memory into the window's device, one texture per slot.
fn import(consumer: &Consumer, ring: &Ring) -> Result<Vec<wgpu::Texture>, String> {
    let exports = ring
        .exports
        .lock()
        .unwrap()
        .take()
        .ok_or("the ring was already imported")?;
    // SAFETY: the device is only used to create and wrap images below.
    let hal_device = unsafe { consumer.device.as_hal::<wgpu::hal::api::Vulkan>() }
        .ok_or("the window's device is not Vulkan")?;
    let device = hal_device.raw_device().clone();
    let instance = hal_device.shared_instance().raw_instance();
    let physical_device = hal_device.raw_physical_device();

    let size = wgpu::Extent3d {
        width: ring.width,
        height: ring.height,
        depth_or_array_layers: 1,
    };
    let mut hal_textures = Vec::with_capacity(exports.len());
    for export in exports {
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(HANDLE_TYPE);
        let info = image_info(ring.width, ring.height, &mut external);
        // SAFETY: info describes a valid 2D image.
        let image = unsafe { device.create_image(&info, None) }.map_err(|e| e.to_string())?;
        // SAFETY: image was created on device above.
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let Some(memory_type) =
            device_local_type(instance, physical_device, requirements.memory_type_bits)
        else {
            // SAFETY: unused.
            unsafe { device.destroy_image(image, None) };
            return Err("no device-local memory type to import into".to_owned());
        };
        // Ownership of the descriptor passes to the driver on success only.
        let fd = export.fd.as_raw_fd();
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(HANDLE_TYPE)
            .fd(fd);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(export.size)
            .memory_type_index(memory_type)
            .push_next(&mut import)
            .push_next(&mut dedicated);
        // SAFETY: fd is a live opaque descriptor of an allocation made for an
        // identical image on the same physical device.
        let memory = match unsafe { device.allocate_memory(&allocate, None) } {
            Ok(memory) => {
                let _ = export.fd.into_raw_fd();
                memory
            }
            Err(error) => {
                // SAFETY: unused.
                unsafe { device.destroy_image(image, None) };
                return Err(error.to_string());
            }
        };
        // SAFETY: the memory was imported for this image.
        if let Err(error) = unsafe { device.bind_image_memory(image, memory, 0) } {
            // SAFETY: unused.
            unsafe {
                device.destroy_image(image, None);
                device.free_memory(memory, None);
            }
            return Err(error.to_string());
        }

        let hal_descriptor = wgpu::hal::TextureDescriptor {
            label: Some("map frame"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COLOR_TARGET,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        let drop_device = device.clone();
        let on_drop: wgpu::hal::DropCallback = Box::new(move || {
            // SAFETY: wgpu calls this once its work on the texture is done.
            unsafe {
                drop_device.destroy_image(image, None);
                drop_device.free_memory(memory, None);
            }
        });
        // SAFETY: image was created as hal_descriptor says, and outlives the
        // texture through on_drop.
        let hal_texture = unsafe {
            hal_device.texture_from_raw(
                image,
                &hal_descriptor,
                Some(on_drop),
                wgpu::hal::vulkan::TextureMemory::External,
            )
        };
        hal_textures.push(hal_texture);
    }
    // Let go of the hal device before wgpu takes the textures over.
    drop(hal_device);

    let descriptor = wgpu::TextureDescriptor {
        label: Some("map frame"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    };
    Ok(hal_textures
        .into_iter()
        .map(|hal_texture| {
            // SAFETY: the texture is on this device, matches the descriptor,
            // and every frame acquired from it is in SHADER_READ_ONLY_OPTIMAL,
            // which is what RESOURCE stands for.
            unsafe {
                consumer
                    .device
                    .create_texture_from_hal::<wgpu::hal::api::Vulkan>(
                        hal_texture,
                        &descriptor,
                        wgpu::TextureUses::RESOURCE,
                    )
            }
        })
        .collect())
}
