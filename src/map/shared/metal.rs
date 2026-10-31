//! Metal's side of the shared frames: the map renders on the window's own
//! device, so its textures are wrapped for wgpu as they are, with nothing to
//! export or import.
//!
//! One device is enough here, unlike on Vulkan: Metal takes command buffers
//! from any number of queues, so MapLibre Native and wgpu each submit to their
//! own. Nor does a frame need a fence to cross between them: a Metal frame is
//! only acquirable once its GPU work has completed.
//!
//! The textures are `objc2-metal` 0.3's, which is what wgpu-hal takes; the
//! rest of the app is on 0.2, so the device crosses over as a pointer.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use maplibre_native_ffi::AcquiredFrameView;
use objc2_metal_v03::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};
use objc2_v06::rc::Retained;
use objc2_v06::runtime::ProtocolObject;
use slint::wgpu_30::wgpu;

use super::RING_DEPTH;

/// The textures' pixel format, on both sides.
const PIXEL_FORMAT: MTLPixelFormat = MTLPixelFormat::RGBA8Unorm;

/// The device the map renders with: a borrowed `id<MTLDevice>`.
pub struct Producer {
    pub device: *mut c_void,
}

/// The textures the map renders into, on the device the window draws with.
///
/// Shared between the render thread, which attached them, and every frame
/// drawn into them, so they outlive both the session and any frame still on
/// screen.
pub struct Ring {
    id: u64,
    width: u32,
    height: u32,
    textures: Vec<Retained<ProtocolObject<dyn MTLTexture>>>,
    /// The session drawing into the ring has been disposed of, so it draws
    /// nothing more and its frames can be handed back while still on screen.
    /// It only retires once they are, and the map takes no new session before.
    disposed: AtomicBool,
}

// SAFETY: Metal textures may be used from any thread; the ring only hands out
// pointers to them and never mutates them.
unsafe impl Send for Ring {}
// SAFETY: as above.
unsafe impl Sync for Ring {}

impl Ring {
    pub fn new(producer: &Producer, width: u32, height: u32) -> Result<Arc<Self>, String> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        // SAFETY: the producer's device is a live `id<MTLDevice>`, leaked for
        // the process by `metal_device`.
        let device = unsafe { &*(producer.device as *const ProtocolObject<dyn MTLDevice>) };
        // SAFETY: a 2D, single-mip descriptor of a non-zero size.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                PIXEL_FORMAT,
                width as usize,
                height as usize,
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        descriptor.setStorageMode(MTLStorageMode::Private);
        let textures = (0..RING_DEPTH)
            .map(|_| {
                device
                    .newTextureWithDescriptor(&descriptor)
                    .ok_or_else(|| "creating a ring texture failed".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Arc::new(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            textures,
            disposed: AtomicBool::new(false),
        }))
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

    /// The textures to attach, in slot order.
    pub fn textures(&self) -> Vec<*mut c_void> {
        self.textures
            .iter()
            .map(|texture| Retained::as_ptr(texture) as *mut c_void)
            .collect()
    }
}

/// Whether the window's device can take this backend's frames: it has to be
/// Metal, and the very device the map renders with.
pub(super) fn accepts(device: &wgpu::Device) -> Result<(), String> {
    // SAFETY: only reads the device's handle.
    let hal_device = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .ok_or("the window is not drawn with Metal")?;
    let window = Retained::as_ptr(hal_device.raw_device()) as *const c_void;
    if std::ptr::eq(window, super::super::renderer::metal_device()) {
        Ok(())
    } else {
        Err("the window draws on a different Metal device from the map".to_owned())
    }
}

/// The ring slot an acquired frame was drawn into.
pub fn slot(view: &AcquiredFrameView<'_>) -> maplibre_native_ffi::Result<u32> {
    view.get_metal_texture().map(|frame| frame.slot)
}

/// Wraps a ring's textures for the window's device, one per slot.
pub(super) fn import(
    window_device: &wgpu::Device,
    ring: &Ring,
) -> Result<Vec<wgpu::Texture>, String> {
    let descriptor = wgpu::TextureDescriptor {
        label: Some("map frame"),
        size: wgpu::Extent3d {
            width: ring.width,
            height: ring.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    };
    Ok(ring
        .textures
        .iter()
        .map(|texture| {
            // SAFETY: the texture is a 2D, single-mip RGBA8Unorm texture of
            // the ring's size; wgpu's copy holds its own reference to it.
            let hal_texture = unsafe {
                wgpu::hal::metal::Device::texture_from_raw(
                    texture.clone(),
                    wgpu::TextureFormat::Rgba8Unorm,
                    MTLTextureType::Type2D,
                    1,
                    1,
                    wgpu::hal::CopyExtent {
                        width: ring.width,
                        height: ring.height,
                        depth: 1,
                    },
                    None,
                )
            };
            // SAFETY: the texture is on this device — `accepts` checked — and
            // matches the descriptor. Metal has no layouts to track.
            unsafe {
                window_device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                    hal_texture,
                    &descriptor,
                    wgpu::TextureUses::RESOURCE,
                )
            }
        })
        .collect())
}
