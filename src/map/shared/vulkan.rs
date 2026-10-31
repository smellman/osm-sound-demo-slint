//! Vulkan's side of the shared frames: the map's device renders into images
//! whose memory is exported as file descriptors, and the window's device
//! imports that memory.
//!
//! The two are separate devices because they cannot share one: this GPU has a
//! single graphics queue, MapLibre Native submits to it from its own thread,
//! and wgpu offers no lock to take around its own submissions.

use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ash::vk;
use maplibre_native_ffi::AcquiredFrameView;
use slint::wgpu_30::wgpu;

use super::RING_DEPTH;

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

    pub(super) fn id(&self) -> u64 {
        self.id
    }

    pub(super) fn disposed(&self) -> bool {
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

/// Whether the window's device can take this backend's frames.
pub(super) fn accepts(device: &wgpu::Device) -> Result<(), String> {
    // SAFETY: only asks which backend the device is on.
    if unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }.is_some() {
        Ok(())
    } else {
        Err("the window is not drawn with Vulkan".to_owned())
    }
}

/// The ring slot an acquired frame was drawn into.
pub fn slot(view: &AcquiredFrameView<'_>) -> maplibre_native_ffi::Result<u32> {
    view.get_vulkan_texture().map(|frame| frame.slot)
}

/// Imports a ring's memory into the window's device, one texture per slot.
pub(super) fn import(
    window_device: &wgpu::Device,
    ring: &Ring,
) -> Result<Vec<wgpu::Texture>, String> {
    let exports = ring
        .exports
        .lock()
        .unwrap()
        .take()
        .ok_or("the ring was already imported")?;
    // SAFETY: the device is only used to create and wrap images below.
    let hal_device = unsafe { window_device.as_hal::<wgpu::hal::api::Vulkan>() }
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
                window_device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
                        hal_texture,
                        &descriptor,
                        wgpu::TextureUses::RESOURCE,
                    )
            }
        })
        .collect())
}
