//! Exposes methods to make initialization of the library easier without losing flexibility.

use std::marker::PhantomData;

use anyhow::Result;
use ash::vk;

use crate::{
    Allocator, AppSettings, DebugMessenger, DefaultAllocator, Device, ExecutionManager,
    FrameManager, Instance, PhysicalDevice, Surface, Window, SurfaceSettings,
};
use crate::pool::{ResourcePool, ResourcePoolCreateInfo};

/// Struct that contains all common Phobos resources to be used at initialization
pub struct Phobos<A: Allocator> {
    pub instance: Instance,
    pub physical_device: PhysicalDevice,
    pub device: Device,
    pub allocator: A,
    pub pool: ResourcePool<A>,
    pub exec: ExecutionManager<A>,
    pub surface: Option<Surface>,
    pub frame: Option<FrameManager<A>>,
    pub debug_messenger: Option<DebugMessenger>,
}

/// Initialize the context with the default allocator
fn init(settings: &AppSettings) -> Result<Phobos<DefaultAllocator>> {
    init_with_allocator(settings, |instance, physical_device, device| {
        DefaultAllocator::new(instance, device, physical_device)
    })
}

/// Initialize the context with a custom allocator
fn init_with_allocator<
    A: Allocator + 'static,
    F: FnOnce(&Instance, &PhysicalDevice, &Device) -> Result<A>,
>(
    settings: &AppSettings,
    make_alloc: F,
) -> Result<Phobos<A>> {
    let instance = Instance::new(settings)?;
    let physical_device = PhysicalDevice::select(&instance, None, settings)?;

    let device = Device::new(&instance, &physical_device, settings)?;
    let allocator = make_alloc(&instance, &physical_device, &device)?;
    let pool_info = ResourcePoolCreateInfo {
        device: device.clone(),
        allocator: allocator.clone(),
        scratch_chunk_size: settings.scratch_chunk_size,
    };
    let pool = ResourcePool::new(pool_info)?;
    let exec = ExecutionManager::new(device.clone(), &physical_device, pool.clone())?;

    let (surface, frame) = if let Some(surface_settings) = settings.surface_settings.as_ref() {
        let mut surface = Surface::new(&instance, surface_settings.window)?;
        surface.query_details(&physical_device)?;

        let frame = FrameManager::new_with_swapchain(
            &instance,
            device.clone(),
            pool.clone(),
            surface_settings,
            &surface,
        )?;

        (Some(surface), Some(frame))
    } else {
        (None, None)
    };

    let debug_messenger = if settings.enable_validation {
        Some(DebugMessenger::new(&instance)?)
    } else {
        None
    };

    Ok(Phobos {
        instance,
        physical_device,
        surface,
        device,
        allocator,
        pool,
        exec,
        frame,
        debug_messenger,
    })
}