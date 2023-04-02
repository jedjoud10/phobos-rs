use anyhow::Result;
use ash::vk;

use crate::{Allocator, Buffer, Device, domain, ExecutionManager, Fence, IncompleteCmdBuffer, MemoryType, TransferCmdBuffer};

/// Perform a staged upload to a GPU buffer. Returns a fence that can be awaited to obtain the resulting buffer.
pub fn staged_buffer_upload<T: Copy, A: Allocator + 'static>(
    device: Device,
    mut allocator: A,
    exec: ExecutionManager,
    data: &[T],
) -> Result<Fence<Buffer<A>>> {
    let staging = Buffer::new(
        device.clone(),
        &mut allocator,
        data.len() as u64 * std::mem::size_of::<T>() as u64,
        vk::BufferUsageFlags::TRANSFER_SRC,
        MemoryType::CpuToGpu,
    )?;

    let mut staging_view = staging.view_full();
    staging_view.mapped_slice()?.copy_from_slice(data);

    let buffer = Buffer::new_device_local(
        device.clone(),
        &mut allocator,
        staging.size(),
        vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::VERTEX_BUFFER,
    )?;
    let view = buffer.view_full();

    let cmd = exec
        .on_domain::<domain::Transfer>(None, None)?
        .copy_buffer(&staging_view, &view)?
        .finish()?;

    Ok(exec
        .submit(cmd)?
        .with_cleanup(move || {
            drop(staging);
        })
        .attach_value(buffer))
}
