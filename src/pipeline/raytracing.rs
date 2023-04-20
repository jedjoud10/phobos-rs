use anyhow::Result;
use ash::vk;

use crate::{Allocator, Buffer, Device, MemoryType, ShaderCreateInfo};
use crate::core::device::ExtensionID;
use crate::pipeline::pipeline_layout::PipelineLayoutCreateInfo;

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct ShaderIndex {
    pub index: u32,
}

pub(crate) const fn shader_group_index(group: &ShaderGroup) -> u32 {
    match group {
        ShaderGroup::RayGeneration {
            ..
        } => 0,
        ShaderGroup::RayMiss {
            ..
        } => 1,
        ShaderGroup::RayHit {
            ..
        } => 2,
        ShaderGroup::Callable {
            ..
        } => 3,
    }
}

#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub enum ShaderGroup {
    RayGeneration {
        shader: ShaderIndex,
    },
    RayMiss {
        shader: ShaderIndex,
    },
    RayHit {
        closest_hit: Option<ShaderIndex>,
        any_hit: Option<ShaderIndex>,
    },
    Callable {
        shader: ShaderIndex,
    },
}

#[derive(Debug)]
pub struct SBTEntry {
    pub offset: u32,
    pub count: u32,
}

#[derive(Debug)]
pub struct ShaderBindingTable<A: Allocator> {
    pub(crate) buffer: Buffer<A>,
    pub(crate) ray_gen: SBTEntry,
    pub(crate) ray_miss: SBTEntry,
    pub(crate) ray_hit: SBTEntry,
    pub(crate) callable: SBTEntry,
    pub(crate) group_size: u32,
    pub(crate) regions: [vk::StridedDeviceAddressRegionKHR; 4],
}

impl<A: Allocator> ShaderBindingTable<A> {
    pub(crate) fn new(mut device: Device, mut allocator: A, pipeline: vk::Pipeline, info: &RayTracingPipelineCreateInfo) -> Result<Self> {
        device.require_extension(ExtensionID::RayTracingPipeline)?;
        let group_count = info.shader_groups.len() as u32;
        let group_handle_size = device.ray_tracing_properties()?.shader_group_handle_size;
        let group_alignment = device.ray_tracing_properties()?.shader_group_base_alignment;
        let aligned_group_size = (group_handle_size + (group_alignment - 1)) & !(group_alignment - 1);
        let sbt_size = aligned_group_size * group_count;

        let buffer = Buffer::new(
            device.clone(),
            &mut allocator,
            sbt_size as u64,
            vk::BufferUsageFlags::SHADER_BINDING_TABLE_KHR | vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryType::CpuToGpu,
        )?;
        let handles = unsafe {
            device
                .raytracing_pipeline()
                .unwrap()
                .get_ray_tracing_shader_group_handles(pipeline, 0, group_count, sbt_size as usize)?
        };

        // Copy over handles to the aligned buffer
        let mut src_pointer = handles.as_ptr();
        let mut dst_pointer = buffer.view_full().mapped_slice::<u8>()?.as_mut_ptr();
        for _group in 0..group_count {
            unsafe {
                src_pointer.copy_to(dst_pointer, aligned_group_size as usize);
                src_pointer = src_pointer.add(group_handle_size as usize);
                dst_pointer = dst_pointer.add(aligned_group_size as usize);
            }
        }

        // Now figure out the entry offsets and counts
        let ray_gen_count = info.shader_groups.iter().filter(|sh| matches!(sh, ShaderGroup::RayGeneration { .. })).count() as u64;
        let ray_miss_count = info.shader_groups.iter().filter(|sh| matches!(sh, ShaderGroup::RayMiss { .. })).count() as u64;
        let ray_hit_count = info.shader_groups.iter().filter(|sh| matches!(sh, ShaderGroup::RayHit { .. })).count() as u64;
        let callable_count = info.shader_groups.iter().filter(|sh| matches!(sh, ShaderGroup::Callable { .. })).count() as u64;

        let ray_gen_offset = 0;
        let ray_miss_offset = if ray_miss_count > 0 {
            info.shader_groups.iter().enumerate().find(|(idx, sh)| matches!(sh, ShaderGroup::RayMiss { .. })).unwrap().0 as u32
        } else {
            0
        };

        let ray_hit_offset = if ray_hit_count > 0 {
            info.shader_groups.iter().enumerate().find(|(idx, sh)| matches!(sh, ShaderGroup::RayHit { .. })).unwrap().0 as u32
        } else {
            0
        };

        let callable_offset = if callable_count > 0 {
            info.shader_groups.iter().enumerate().find(|(idx, sh)| matches!(sh, ShaderGroup::Callable { .. })).unwrap().0 as u32
        } else {
            0
        };

        let address = buffer.address();

        let size = aligned_group_size as u64;
        let stride = aligned_group_size as vk::DeviceSize;

        let regions: [vk::StridedDeviceAddressRegionKHR; 4] = [
            vk::StridedDeviceAddressRegionKHR {
                device_address: address,
                stride,
                size: (size * ray_gen_count) as vk::DeviceSize,
            },
            vk::StridedDeviceAddressRegionKHR {
                device_address: address + ray_miss_offset as u64 * size,
                stride,
                size: (size * ray_miss_count) as vk::DeviceSize,
            },
            vk::StridedDeviceAddressRegionKHR {
                device_address: address + ray_hit_offset as u64 * size,
                stride,
                size: (size * ray_hit_count) as vk::DeviceSize,
            },
            vk::StridedDeviceAddressRegionKHR {
                device_address: if callable_count > 0 { address + callable_offset as u64 * size } else { 0 },
                stride: if callable_count > 0 { stride } else { 0 },
                size: (callable_count * size) as vk::DeviceSize,
            },
        ];

        Ok(ShaderBindingTable {
            buffer,
            ray_gen: SBTEntry { offset: ray_gen_offset, count: ray_gen_count as u32 },
            ray_miss: SBTEntry { offset: ray_miss_offset, count: ray_miss_count as u32 },
            ray_hit: SBTEntry { offset: ray_hit_offset, count: ray_hit_count as u32 },
            callable: SBTEntry { offset: callable_offset, count: callable_count as u32 },
            group_size: aligned_group_size,
            regions,
        })
    }
}

#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub struct RayTracingPipelineCreateInfo {
    pub(crate) name: String,
    pub(crate) layout: PipelineLayoutCreateInfo,
    pub(crate) max_recursion_depth: u32,
    pub(crate) shader_groups: Vec<ShaderGroup>,
    pub shaders: Vec<ShaderCreateInfo>,
}

impl RayTracingPipelineCreateInfo {
    // Shaders not filled out
    pub(crate) fn to_vk(&self, layout: vk::PipelineLayout) -> vk::RayTracingPipelineCreateInfoKHR {
        vk::RayTracingPipelineCreateInfoKHR {
            s_type: vk::StructureType::RAY_TRACING_PIPELINE_CREATE_INFO_KHR,
            p_next: std::ptr::null(),
            flags: Default::default(),
            stage_count: 0,
            p_stages: std::ptr::null(),
            group_count: 0,
            p_groups: std::ptr::null(),
            max_pipeline_ray_recursion_depth: self.max_recursion_depth,
            p_library_info: std::ptr::null(),
            p_library_interface: std::ptr::null(),
            p_dynamic_state: std::ptr::null(),
            layout,
            base_pipeline_handle: Default::default(),
            base_pipeline_index: 0,
        }
    }
}

pub struct RayTracingPipelineBuilder {
    inner: RayTracingPipelineCreateInfo,
}

impl RayTracingPipelineBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            inner: RayTracingPipelineCreateInfo {
                name: name.into(),
                layout: Default::default(),
                max_recursion_depth: 0,
                shader_groups: vec![],
                shaders: vec![],
            },
        }
    }

    fn add_shader(&mut self, shader: ShaderCreateInfo) -> ShaderIndex {
        if let Some((idx, shader)) = self
            .inner
            .shaders
            .iter()
            .enumerate()
            .find(|(idx, sh)| sh.code_hash() == shader.code_hash())
        {
            ShaderIndex {
                index: idx as u32,
            }
        } else {
            self.inner.shaders.push(shader);
            ShaderIndex {
                index: (self.inner.shaders.len() - 1) as u32,
            }
        }
    }

    pub fn add_shader_group(mut self, group: ShaderGroup) -> Self {
        self.inner.shader_groups.push(group);
        self
    }

    pub fn add_ray_gen_group(mut self, shader: ShaderCreateInfo) -> Self {
        let shader = self.add_shader(shader);
        self.inner.shader_groups.push(ShaderGroup::RayGeneration {
            shader,
        });
        self
    }

    pub fn add_ray_miss_group(mut self, shader: ShaderCreateInfo) -> Self {
        let shader = self.add_shader(shader);
        self.inner.shader_groups.push(ShaderGroup::RayMiss {
            shader,
        });
        self
    }

    pub fn add_ray_hit_group(mut self, closest_hit: Option<ShaderCreateInfo>, any_hit: Option<ShaderCreateInfo>) -> Self {
        let closest_hit = closest_hit.map(|sh| self.add_shader(sh));
        let any_hit = any_hit.map(|sh| self.add_shader(sh));
        self.inner.shader_groups.push(ShaderGroup::RayHit {
            closest_hit,
            any_hit,
        });
        self
    }

    pub fn add_callable_group(mut self, shader: ShaderCreateInfo) -> Self {
        let shader = self.add_shader(shader);
        self.inner.shader_groups.push(ShaderGroup::Callable {
            shader,
        });
        self
    }

    pub fn max_recursion_depth(mut self, depth: u32) -> Self {
        self.inner.max_recursion_depth = depth;
        self
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn build(mut self) -> RayTracingPipelineCreateInfo {
        // Sort shader groups by type
        self.inner.shader_groups.sort_by_key(|group| shader_group_index(group));
        self.inner
    }
}
