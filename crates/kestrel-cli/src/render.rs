//! The GPU's draws on the host's GPU with Vulkan (phase 5B, the plan's
//! "render with Vulkan everywhere"): `kestrel_hle::gpu::draw` describes a
//! draw (SPIR-V shaders translated from the program's Maxwell shaders,
//! vertices, uniform buffers, textures, state, the render targets'
//! pixels); this runs it and gives the pixels back.
//!
//! Each draw uploads its attachments, records one render pass, waits for
//! it and reads the attachments back: simple and exact, not fast (the
//! render targets live in guest memory, which kestrel reads and writes
//! around every draw). Pipelines, render passes and layouts are kept by a
//! hash of what made them. Windows and Linux x86_64, with ash (the owner's
//! choice, 2026-11-03).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use ash::vk;
use kestrel_hle::gpu::draw::{Draw, Renderer, Topology, Vertices};
use kestrel_hle::gpu::texture::Texture;

type Res<T> = Result<T, String>;

fn vkerr(what: &'static str) -> impl Fn(vk::Result) -> String {
    move |e| format!("{what}: {e}")
}

/// A colour target's Vulkan format (SET_COLOR_TARGET_FORMAT codes,
/// `clb197.h`; each has the same bytes in memory as the Vulkan format) and
/// whether its alpha is unused (the X8 forms).
fn color_format(code: u32) -> Res<(vk::Format, bool)> {
    use vk::Format as F;
    Ok(match code {
        0xc0 => (F::R32G32B32A32_SFLOAT, false),
        0xca => (F::R16G16B16A16_SFLOAT, false),
        0xcf => (F::B8G8R8A8_UNORM, false),
        0xd0 => (F::B8G8R8A8_SRGB, false),
        0xd1 => (F::A2B10G10R10_UNORM_PACK32, false),
        0xd5 => (F::R8G8B8A8_UNORM, false),
        0xd6 => (F::R8G8B8A8_SRGB, false),
        0xe6 => (F::B8G8R8A8_UNORM, true),
        0xe7 => (F::B8G8R8A8_SRGB, true),
        0xe8 => (F::R5G6B5_UNORM_PACK16, false),
        0xea => (F::R8G8_UNORM, false),
        0xf3 => (F::R8_UNORM, false),
        0xf9 => (F::R8G8B8A8_UNORM, true),
        0xfa => (F::R8G8B8A8_SRGB, true),
        c => return Err(format!("colour format {c:#x}")),
    })
}

/// A vertex attribute's format: COMPONENT_BIT_WIDTHS and NUMERICAL_TYPE
/// (`clb197.h`: 1 SNORM, 2 UNORM, 3 SINT, 4 UINT, 5 USCALED, 6 SSCALED, 7
/// FLOAT).
fn vertex_format(widths: u32, numerical: u32) -> Res<vk::Format> {
    use vk::Format as F;
    let pick = |f: [F; 7]| {
        if (1..=7).contains(&numerical) {
            Ok(f[numerical as usize - 1])
        } else {
            Err(format!("vertex numerical type {numerical}"))
        }
    };
    let u = F::UNDEFINED;
    let f = match widths {
        // SNORM, UNORM, SINT, UINT, USCALED, SSCALED, FLOAT
        0x01 => pick([
            u,
            u,
            F::R32G32B32A32_SINT,
            F::R32G32B32A32_UINT,
            u,
            u,
            F::R32G32B32A32_SFLOAT,
        ]),
        0x02 => pick([
            u,
            u,
            F::R32G32B32_SINT,
            F::R32G32B32_UINT,
            u,
            u,
            F::R32G32B32_SFLOAT,
        ]),
        0x04 => pick([u, u, F::R32G32_SINT, F::R32G32_UINT, u, u, F::R32G32_SFLOAT]),
        0x12 => pick([u, u, F::R32_SINT, F::R32_UINT, u, u, F::R32_SFLOAT]),
        0x03 => pick([
            F::R16G16B16A16_SNORM,
            F::R16G16B16A16_UNORM,
            F::R16G16B16A16_SINT,
            F::R16G16B16A16_UINT,
            F::R16G16B16A16_USCALED,
            F::R16G16B16A16_SSCALED,
            F::R16G16B16A16_SFLOAT,
        ]),
        0x0f => pick([
            F::R16G16_SNORM,
            F::R16G16_UNORM,
            F::R16G16_SINT,
            F::R16G16_UINT,
            F::R16G16_USCALED,
            F::R16G16_SSCALED,
            F::R16G16_SFLOAT,
        ]),
        0x1b => pick([
            F::R16_SNORM,
            F::R16_UNORM,
            F::R16_SINT,
            F::R16_UINT,
            F::R16_USCALED,
            F::R16_SSCALED,
            F::R16_SFLOAT,
        ]),
        0x0a | 0x2f => pick([
            F::R8G8B8A8_SNORM,
            F::R8G8B8A8_UNORM,
            F::R8G8B8A8_SINT,
            F::R8G8B8A8_UINT,
            F::R8G8B8A8_USCALED,
            F::R8G8B8A8_SSCALED,
            u,
        ]),
        0x18 | 0x32 => pick([
            F::R8G8_SNORM,
            F::R8G8_UNORM,
            F::R8G8_SINT,
            F::R8G8_UINT,
            F::R8G8_USCALED,
            F::R8G8_SSCALED,
            u,
        ]),
        0x1d => pick([
            F::R8_SNORM,
            F::R8_UNORM,
            F::R8_SINT,
            F::R8_UINT,
            F::R8_USCALED,
            F::R8_SSCALED,
            u,
        ]),
        w => return Err(format!("vertex component widths {w:#x}")),
    }?;
    if f == u {
        return Err(format!(
            "vertex format {widths:#x} with numerical type {numerical}"
        ));
    }
    Ok(f)
}

/// A texture's Vulkan format (the TIC's COMPONENTS_SIZES, data type and
/// sRGB conversion).
fn texture_format(t: &Texture) -> Res<vk::Format> {
    use vk::Format as F;
    let s = t.srgb;
    Ok(match (t.components, t.data_type) {
        (0x01, 7) => F::R32G32B32A32_SFLOAT,
        (0x03, 7) => F::R16G16B16A16_SFLOAT,
        (0x08, 2) => {
            if s {
                F::R8G8B8A8_SRGB
            } else {
                F::R8G8B8A8_UNORM
            }
        }
        (0x09, 2) => F::A2B10G10R10_UNORM_PACK32,
        (0x15, 2) => F::B5G6R5_UNORM_PACK16,
        (0x18, 2) => F::R8G8_UNORM,
        (0x1d, 2) => F::R8_UNORM,
        (0x24, 2) => {
            if s {
                F::BC1_RGBA_SRGB_BLOCK
            } else {
                F::BC1_RGBA_UNORM_BLOCK
            }
        }
        (0x25, 2) => {
            if s {
                F::BC2_SRGB_BLOCK
            } else {
                F::BC2_UNORM_BLOCK
            }
        }
        (0x26, 2) => {
            if s {
                F::BC3_SRGB_BLOCK
            } else {
                F::BC3_UNORM_BLOCK
            }
        }
        (c, d) => return Err(format!("texture format {c:#x} of data type {d}")),
    })
}

/// G80_TIC_SOURCE to Vulkan's component swizzle.
fn swizzle(source: u32) -> vk::ComponentSwizzle {
    use vk::ComponentSwizzle as S;
    match source {
        2 => S::R,
        3 => S::G,
        4 => S::B,
        5 => S::A,
        6 | 7 => S::ONE,
        _ => S::ZERO,
    }
}

fn wrap(mode: u32) -> Res<vk::SamplerAddressMode> {
    Ok(match mode {
        0 => vk::SamplerAddressMode::REPEAT,
        1 => vk::SamplerAddressMode::MIRRORED_REPEAT,
        2 => vk::SamplerAddressMode::CLAMP_TO_EDGE,
        3 => vk::SamplerAddressMode::CLAMP_TO_BORDER,
        m => return Err(format!("texture wrap mode {m}")),
    })
}

/// What a pipeline is made from (and the render pass and layouts with it).
#[derive(Hash)]
struct PipelineKey<'a> {
    vertex: &'a [u32],
    fragment: &'a [u32],
    topology: i32,
    buffers: Vec<(u32, bool)>,
    attributes: Vec<(u32, u32, u32, i32)>,
    uniforms: Vec<u32>,
    textures: Vec<u32>,
    colors: Vec<Option<(i32, u8)>>,
    depth: bool,
    depth_test: Option<u32>,
    depth_write: bool,
    cull: u32,
    front_ccw: bool,
}

struct Pipeline {
    render_pass: vk::RenderPass,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
}

/// What one draw creates, destroyed after it.
#[derive(Default)]
struct Scratch {
    buffers: Vec<(vk::Buffer, vk::DeviceMemory)>,
    images: Vec<(vk::Image, vk::DeviceMemory)>,
    views: Vec<vk::ImageView>,
    samplers: Vec<vk::Sampler>,
    framebuffer: Option<vk::Framebuffer>,
    pool: Option<vk::DescriptorPool>,
}

pub struct VulkanRenderer {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    memory: vk::PhysicalDeviceMemoryProperties,
    pipelines: HashMap<u64, Pipeline>,
    pub name: String,
}

// The handles are only used from the thread that owns the renderer.
unsafe impl Send for VulkanRenderer {}

impl VulkanRenderer {
    /// The first Vulkan GPU with a graphics queue, without a window.
    pub fn new() -> Res<Self> {
        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| format!("no Vulkan library on this computer ({e})"))?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"kestrel")
            .engine_name(c"kestrel")
            .api_version(vk::make_api_version(0, 1, 1, 0));
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
        }
        .map_err(vkerr("vkCreateInstance"))?;
        let found = unsafe { instance.enumerate_physical_devices() }
            .unwrap_or_default()
            .into_iter()
            .find_map(|p| {
                let families = unsafe { instance.get_physical_device_queue_family_properties(p) };
                families
                    .iter()
                    .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                    .map(|i| (p, i as u32))
            });
        let Some((physical, family)) = found else {
            unsafe { instance.destroy_instance(None) };
            return Err("no Vulkan GPU with a graphics queue".into());
        };
        let supported = unsafe { instance.get_physical_device_features(physical) };
        let features = vk::PhysicalDeviceFeatures::default()
            .texture_compression_bc(supported.texture_compression_bc == vk::TRUE);
        let priorities = [1.0];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&priorities)];
        let device = match unsafe {
            instance.create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queues)
                    .enabled_features(&features),
                None,
            )
        } {
            Ok(d) => d,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(format!("vkCreateDevice: {e}"));
            }
        };
        let props = unsafe { instance.get_physical_device_properties(physical) };
        let name = props
            .device_name_as_c_str()
            .map_or("a Vulkan GPU".into(), |n| n.to_string_lossy().into_owned());
        let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
        let queue = unsafe { device.get_device_queue(family, 0) };
        let (pool, cmd, fence) = unsafe {
            let pool = device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(family)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .map_err(vkerr("vkCreateCommandPool"))?;
            let cmd = device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(vkerr("vkAllocateCommandBuffers"))?[0];
            let fence = device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(vkerr("vkCreateFence"))?;
            (pool, cmd, fence)
        };
        Ok(VulkanRenderer {
            _entry: entry,
            instance,
            device,
            queue,
            pool,
            cmd,
            fence,
            memory,
            pipelines: HashMap::new(),
            name,
        })
    }

    fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> Res<u32> {
        (0..self.memory.memory_type_count)
            .find(|&i| {
                bits & (1 << i) != 0
                    && self.memory.memory_types[i as usize]
                        .property_flags
                        .contains(want)
            })
            .ok_or_else(|| format!("no Vulkan memory that is {want:?}"))
    }

    /// A host-visible buffer holding `data` (at least `size` bytes).
    fn buffer(
        &self,
        s: &mut Scratch,
        usage: vk::BufferUsageFlags,
        data: &[u8],
        size: usize,
    ) -> Res<vk::Buffer> {
        let size = size.max(data.len()).max(4) as u64;
        unsafe {
            let buffer = self
                .device
                .create_buffer(
                    &vk::BufferCreateInfo::default().size(size).usage(usage),
                    None,
                )
                .map_err(vkerr("vkCreateBuffer"))?;
            let req = self.device.get_buffer_memory_requirements(buffer);
            let kind = self.memory_type(
                req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            let memory = self
                .device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(kind),
                    None,
                )
                .map_err(vkerr("vkAllocateMemory"))?;
            s.buffers.push((buffer, memory));
            self.device
                .bind_buffer_memory(buffer, memory, 0)
                .map_err(vkerr("vkBindBufferMemory"))?;
            let ptr: *mut u8 = self
                .device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
                .map_err(vkerr("vkMapMemory"))?
                .cast();
            std::ptr::write_bytes(ptr, 0, size as usize);
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            Ok(buffer)
        }
    }

    /// The bytes of buffer `index` in `s` (mapped again to read back).
    fn read_buffer(&self, s: &Scratch, buffer: vk::Buffer, len: usize) -> Res<Vec<u8>> {
        let &(_, memory) = s.buffers.iter().find(|b| b.0 == buffer).ok_or("buffer")?;
        unsafe {
            let ptr: *const u8 = self
                .device
                .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .map_err(vkerr("vkMapMemory"))?
                .cast();
            let mut out = vec![0u8; len];
            std::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), len);
            self.device.unmap_memory(memory);
            Ok(out)
        }
    }

    fn image(
        &self,
        s: &mut Scratch,
        format: vk::Format,
        width: u32,
        height: u32,
        usage: vk::ImageUsageFlags,
    ) -> Res<vk::Image> {
        unsafe {
            let image = self
                .device
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(format)
                        .extent(vk::Extent3D {
                            width,
                            height,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(usage)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )
                .map_err(vkerr("vkCreateImage"))?;
            let req = self.device.get_image_memory_requirements(image);
            let kind =
                self.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
            let memory = self
                .device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(kind),
                    None,
                )
                .map_err(vkerr("vkAllocateMemory"))?;
            s.images.push((image, memory));
            self.device
                .bind_image_memory(image, memory, 0)
                .map_err(vkerr("vkBindImageMemory"))?;
            Ok(image)
        }
    }

    fn view(
        &self,
        s: &mut Scratch,
        image: vk::Image,
        format: vk::Format,
        aspect: vk::ImageAspectFlags,
        components: vk::ComponentMapping,
    ) -> Res<vk::ImageView> {
        let v = unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .components(components)
                    .subresource_range(range(aspect)),
                None,
            )
        }
        .map_err(vkerr("vkCreateImageView"))?;
        s.views.push(v);
        Ok(v)
    }

    fn barrier(
        &self,
        image: vk::Image,
        aspect: vk::ImageAspectFlags,
        from: vk::ImageLayout,
        to: vk::ImageLayout,
    ) {
        let b = vk::ImageMemoryBarrier::default()
            .image(image)
            .old_layout(from)
            .new_layout(to)
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .subresource_range(range(aspect));
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[b],
            )
        };
    }

    fn copy_in(
        &self,
        buffer: vk::Buffer,
        image: vk::Image,
        aspect: vk::ImageAspectFlags,
        width: u32,
        height: u32,
    ) {
        let region = vk::BufferImageCopy::default()
            .image_subresource(layers(aspect))
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        unsafe {
            self.device.cmd_copy_buffer_to_image(
                self.cmd,
                buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            )
        };
    }

    fn copy_out(
        &self,
        image: vk::Image,
        buffer: vk::Buffer,
        aspect: vk::ImageAspectFlags,
        width: u32,
        height: u32,
    ) {
        let region = vk::BufferImageCopy::default()
            .image_subresource(layers(aspect))
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        unsafe {
            self.device.cmd_copy_image_to_buffer(
                self.cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[region],
            )
        };
    }

    fn shader(&self, words: &[u32]) -> Res<vk::ShaderModule> {
        unsafe {
            self.device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(words), None)
        }
        .map_err(vkerr("vkCreateShaderModule"))
    }

    /// The pipeline (and render pass and layouts) for `d`.
    fn pipeline(&mut self, d: &Draw, formats: &[Option<vk::Format>]) -> Res<u64> {
        let topology = match d.topology {
            Topology::Points => vk::PrimitiveTopology::POINT_LIST,
            Topology::Lines => vk::PrimitiveTopology::LINE_LIST,
            Topology::LineStrip => vk::PrimitiveTopology::LINE_STRIP,
            Topology::Triangles => vk::PrimitiveTopology::TRIANGLE_LIST,
            Topology::TriangleStrip => vk::PrimitiveTopology::TRIANGLE_STRIP,
            Topology::TriangleFan => vk::PrimitiveTopology::TRIANGLE_FAN,
        };
        let mut attributes = Vec::new();
        for a in &d.attributes {
            attributes.push((
                a.location,
                a.buffer,
                a.offset,
                vertex_format(a.widths, a.numerical)?.as_raw(),
            ));
        }
        let key = PipelineKey {
            vertex: &d.vertex_spirv,
            fragment: &d.fragment_spirv,
            topology: topology.as_raw(),
            buffers: d
                .buffers
                .iter()
                .map(|b| (b.stride, b.per_instance))
                .collect(),
            attributes,
            uniforms: d.uniforms.iter().map(|u| u.0).collect(),
            textures: d.textures.iter().map(|t| t.0).collect(),
            colors: d
                .colors
                .iter()
                .zip(formats)
                .map(|(c, f)| c.as_ref().zip(*f).map(|(c, f)| (f.as_raw(), c.write_mask)))
                .collect(),
            depth: d.depth.is_some(),
            depth_test: d.depth_test.map(|c| c.0),
            depth_write: d.depth_write,
            cull: d.cull,
            front_ccw: d.front_ccw,
        };
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        let id = h.finish();
        if self.pipelines.contains_key(&id) {
            return Ok(id);
        }
        let dev = &self.device;
        // The render pass: every attachment loaded and stored (the guest's
        // pixels are uploaded before and read back after).
        let mut descs = Vec::new();
        let mut refs = Vec::new();
        for c in &key.colors {
            match c {
                Some((f, _)) => {
                    refs.push(
                        vk::AttachmentReference::default()
                            .attachment(descs.len() as u32)
                            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL),
                    );
                    descs.push(attachment(
                        vk::Format::from_raw(*f),
                        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    ));
                }
                None => {
                    refs.push(vk::AttachmentReference::default().attachment(vk::ATTACHMENT_UNUSED))
                }
            }
        }
        let depth_ref = vk::AttachmentReference::default()
            .attachment(descs.len() as u32)
            .layout(vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL);
        if key.depth {
            descs.push(attachment(
                vk::Format::D32_SFLOAT,
                vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
            ));
        }
        let mut subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&refs);
        if key.depth {
            subpass = subpass.depth_stencil_attachment(&depth_ref);
        }
        let subpasses = [subpass];
        let render_pass = unsafe {
            dev.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&descs)
                    .subpasses(&subpasses),
                None,
            )
        }
        .map_err(vkerr("vkCreateRenderPass"))?;
        // Descriptors: uniform buffers below binding 64 are the vertex
        // shader's, from 64 the pixel shader's; textures are the pixel
        // shader's.
        let mut bindings = Vec::new();
        for &b in &key.uniforms {
            let stage = if b < 64 {
                vk::ShaderStageFlags::VERTEX
            } else {
                vk::ShaderStageFlags::FRAGMENT
            };
            bindings.push(
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(stage),
            );
        }
        for &b in &key.textures {
            bindings.push(
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            );
        }
        let set_layout = unsafe {
            dev.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .map_err(vkerr("vkCreateDescriptorSetLayout"))?;
        let set_layouts = [set_layout];
        let layout = unsafe {
            dev.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                None,
            )
        }
        .map_err(vkerr("vkCreatePipelineLayout"))?;
        let vs = self.shader(key.vertex)?;
        let fs = self.shader(key.fragment)?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vs)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fs)
                .name(c"main"),
        ];
        let vb: Vec<_> = key
            .buffers
            .iter()
            .enumerate()
            .map(|(i, &(stride, inst))| {
                vk::VertexInputBindingDescription::default()
                    .binding(i as u32)
                    .stride(stride)
                    .input_rate(if inst {
                        vk::VertexInputRate::INSTANCE
                    } else {
                        vk::VertexInputRate::VERTEX
                    })
            })
            .collect();
        let va: Vec<_> = key
            .attributes
            .iter()
            .map(|&(loc, b, off, f)| {
                vk::VertexInputAttributeDescription::default()
                    .location(loc)
                    .binding(b)
                    .offset(off)
                    .format(vk::Format::from_raw(f))
            })
            .collect();
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vb)
            .vertex_attribute_descriptions(&va);
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default().topology(topology);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::from_raw(key.cull))
            .front_face(if key.front_ccw {
                vk::FrontFace::COUNTER_CLOCKWISE
            } else {
                vk::FrontFace::CLOCKWISE
            })
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let depth = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(key.depth_test.is_some() || key.depth_write)
            .depth_write_enable(key.depth_write)
            .depth_compare_op(vk::CompareOp::from_raw(key.depth_test.unwrap_or(7) as i32));
        let blends: Vec<_> = key
            .colors
            .iter()
            .map(|c| {
                vk::PipelineColorBlendAttachmentState::default().color_write_mask(
                    vk::ColorComponentFlags::from_raw(c.map_or(0, |c| c.1 as u32)),
                )
            })
            .collect();
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blends);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .render_pass(render_pass);
        let made =
            unsafe { dev.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) };
        unsafe {
            dev.destroy_shader_module(vs, None);
            dev.destroy_shader_module(fs, None);
        }
        let pipeline = made.map_err(|(_, e)| format!("vkCreateGraphicsPipelines: {e}"))?[0];
        self.pipelines.insert(
            id,
            Pipeline {
                render_pass,
                set_layout,
                layout,
                pipeline,
            },
        );
        Ok(id)
    }

    fn run(&mut self, d: &mut Draw, s: &mut Scratch) -> Res<()> {
        if std::env::var_os("KESTREL_GPU_DEBUG").is_some() {
            eprintln!(
                "draw {:?} {:?} inst {} viewport {:?} scissor {:?} cull {} ccw {} depth {:?}/{} buffers {:?} attrs {:?} uniforms {:?} textures {:?} colors {:?} depth {:?}",
                d.topology,
                match &d.vertices { Vertices::Array { first, count } => format!("array {first}+{count}"), Vertices::Indexed(i) => format!("indexed {}", i.len()) },
                d.instance, d.viewport, d.scissor, d.cull, d.front_ccw, d.depth_test, d.depth_write,
                d.buffers.iter().map(|b| (b.stride, b.per_instance, b.data.len())).collect::<Vec<_>>(),
                d.attributes,
                d.uniforms.iter().map(|u| (u.0, u.1.len())).collect::<Vec<_>>(),
                d.textures.iter().map(|t| (t.0, t.1.width, t.1.height, t.1.components)).collect::<Vec<_>>(),
                d.colors.iter().map(|c| c.as_ref().map(|c| (c.format, c.width, c.height, c.write_mask))).collect::<Vec<_>>(),
                d.depth.as_ref().map(|z| (z.width, z.height)),
            );
        }
        use vk::BufferUsageFlags as B;
        use vk::ImageUsageFlags as U;
        let color = vk::ImageAspectFlags::COLOR;
        let depth_aspect = vk::ImageAspectFlags::DEPTH;
        let mut formats = Vec::new();
        for c in &mut d.colors {
            formats.push(match c {
                Some(c) => {
                    let (f, x) = color_format(c.format)?;
                    if x {
                        c.write_mask &= 7;
                    }
                    Some(f)
                }
                None => None,
            });
        }
        let id = self.pipeline(d, &formats)?;
        let dev = self.device.clone();
        unsafe {
            dev.begin_command_buffer(self.cmd, &vk::CommandBufferBeginInfo::default())
                .map_err(vkerr("vkBeginCommandBuffer"))?;
        }
        // Attachments: uploaded, then laid out for the render pass.
        let (mut width, mut height) = (u32::MAX, u32::MAX);
        let mut views = Vec::new();
        let mut readback = Vec::new();
        for (c, f) in d.colors.iter().zip(&formats) {
            let (Some(c), Some(f)) = (c, f) else { continue };
            let image = self.image(
                s,
                *f,
                c.width,
                c.height,
                U::COLOR_ATTACHMENT | U::TRANSFER_SRC | U::TRANSFER_DST,
            )?;
            let up = self.buffer(s, B::TRANSFER_SRC | B::TRANSFER_DST, &c.pixels, 0)?;
            self.barrier(
                image,
                color,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            self.copy_in(up, image, color, c.width, c.height);
            self.barrier(
                image,
                color,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            );
            views.push(self.view(s, image, *f, color, vk::ComponentMapping::default())?);
            readback.push((image, up, color, c.width, c.height));
            width = width.min(c.width);
            height = height.min(c.height);
        }
        if let Some(z) = &d.depth {
            let f = vk::Format::D32_SFLOAT;
            let image = self.image(
                s,
                f,
                z.width,
                z.height,
                U::DEPTH_STENCIL_ATTACHMENT | U::TRANSFER_SRC | U::TRANSFER_DST,
            )?;
            let bytes: Vec<u8> = z.values.iter().flat_map(|v| v.to_le_bytes()).collect();
            let up = self.buffer(s, B::TRANSFER_SRC | B::TRANSFER_DST, &bytes, 0)?;
            self.barrier(
                image,
                depth_aspect,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            self.copy_in(up, image, depth_aspect, z.width, z.height);
            self.barrier(
                image,
                depth_aspect,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
            );
            views.push(self.view(s, image, f, depth_aspect, vk::ComponentMapping::default())?);
            readback.push((image, up, depth_aspect, z.width, z.height));
            width = width.min(z.width);
            height = height.min(z.height);
        }
        if width == u32::MAX {
            return Err("a draw with no attachments".into());
        }
        // Textures.
        let mut tex = Vec::new();
        for (binding, t) in &d.textures {
            let f = texture_format(t)?;
            let image = self.image(s, f, t.width, t.height, U::SAMPLED | U::TRANSFER_DST)?;
            let up = self.buffer(s, B::TRANSFER_SRC, &t.data, 0)?;
            self.barrier(
                image,
                color,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            self.copy_in(up, image, color, t.width, t.height);
            self.barrier(
                image,
                color,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            );
            let components = vk::ComponentMapping {
                r: swizzle(t.swizzle[0]),
                g: swizzle(t.swizzle[1]),
                b: swizzle(t.swizzle[2]),
                a: swizzle(t.swizzle[3]),
            };
            let view = self.view(s, image, f, color, components)?;
            let filter = |l| {
                if l {
                    vk::Filter::LINEAR
                } else {
                    vk::Filter::NEAREST
                }
            };
            let sampler = unsafe {
                dev.create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(filter(t.mag_linear))
                        .min_filter(filter(t.min_linear))
                        .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                        .address_mode_u(wrap(t.wrap[0])?)
                        .address_mode_v(wrap(t.wrap[1])?)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .max_lod(0.0),
                    None,
                )
            }
            .map_err(vkerr("vkCreateSampler"))?;
            s.samplers.push(sampler);
            tex.push((*binding, view, sampler));
        }
        let p = &self.pipelines[&id];
        let (render_pass, set_layout, layout, pipeline) =
            (p.render_pass, p.set_layout, p.layout, p.pipeline);
        // Descriptors.
        let sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(d.uniforms.len().max(1) as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(tex.len().max(1) as u32),
        ];
        let pool = unsafe {
            dev.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&sizes),
                None,
            )
        }
        .map_err(vkerr("vkCreateDescriptorPool"))?;
        s.pool = Some(pool);
        let set_layouts = [set_layout];
        let set = unsafe {
            dev.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&set_layouts),
            )
        }
        .map_err(vkerr("vkAllocateDescriptorSets"))?[0];
        let mut buffer_infos = Vec::new();
        for (binding, data) in &d.uniforms {
            let b = self.buffer(s, B::UNIFORM_BUFFER, data, 0x1_0000)?;
            buffer_infos.push((
                *binding,
                [vk::DescriptorBufferInfo::default()
                    .buffer(b)
                    .range(0x1_0000)],
            ));
        }
        let image_infos: Vec<_> = tex
            .iter()
            .map(|&(b, view, sampler)| {
                (
                    b,
                    [vk::DescriptorImageInfo::default()
                        .image_view(view)
                        .sampler(sampler)
                        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
                )
            })
            .collect();
        let mut writes = Vec::new();
        for (b, info) in &buffer_infos {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(*b)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(info),
            );
        }
        for (b, info) in &image_infos {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(*b)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(info),
            );
        }
        unsafe { dev.update_descriptor_sets(&writes, &[]) };
        // Vertex and index buffers.
        let mut vbs = Vec::new();
        for b in &d.buffers {
            vbs.push(self.buffer(s, B::VERTEX_BUFFER, &b.data, 0)?);
        }
        let index = match &d.vertices {
            Vertices::Indexed(ix) => {
                let bytes: Vec<u8> = ix.iter().flat_map(|i| i.to_le_bytes()).collect();
                Some((self.buffer(s, B::INDEX_BUFFER, &bytes, 0)?, ix.len() as u32))
            }
            Vertices::Array { .. } => None,
        };
        let framebuffer = unsafe {
            dev.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(render_pass)
                    .attachments(&views)
                    .width(width)
                    .height(height)
                    .layers(1),
                None,
            )
        }
        .map_err(vkerr("vkCreateFramebuffer"))?;
        s.framebuffer = Some(framebuffer);
        let v = d.viewport;
        let sc = d.scissor;
        let sx = sc[0].min(width);
        let sy = sc[1].min(height);
        let scissor = vk::Rect2D {
            offset: vk::Offset2D {
                x: sx as i32,
                y: sy as i32,
            },
            extent: vk::Extent2D {
                width: sc[2].min(width - sx),
                height: sc[3].min(height - sy),
            },
        };
        unsafe {
            dev.cmd_begin_render_pass(
                self.cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(render_pass)
                    .framebuffer(framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent: vk::Extent2D { width, height },
                    }),
                vk::SubpassContents::INLINE,
            );
            dev.cmd_bind_pipeline(self.cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
            dev.cmd_set_viewport(
                self.cmd,
                0,
                &[vk::Viewport {
                    x: v.x,
                    y: v.y,
                    width: v.width,
                    height: v.height,
                    min_depth: v.min_depth,
                    max_depth: v.max_depth,
                }],
            );
            dev.cmd_set_scissor(self.cmd, 0, &[scissor]);
            dev.cmd_bind_descriptor_sets(
                self.cmd,
                vk::PipelineBindPoint::GRAPHICS,
                layout,
                0,
                &[set],
                &[],
            );
            if !vbs.is_empty() {
                let offsets = vec![0u64; vbs.len()];
                dev.cmd_bind_vertex_buffers(self.cmd, 0, &vbs, &offsets);
            }
            match (&d.vertices, index) {
                (Vertices::Array { first, count }, _) => {
                    dev.cmd_draw(self.cmd, *count, 1, *first, d.instance)
                }
                (Vertices::Indexed(_), Some((b, n))) => {
                    dev.cmd_bind_index_buffer(self.cmd, b, 0, vk::IndexType::UINT32);
                    dev.cmd_draw_indexed(self.cmd, n, 1, 0, 0, d.instance);
                }
                _ => {}
            }
            dev.cmd_end_render_pass(self.cmd);
        }
        // Read the attachments back (the render pass left them in
        // TRANSFER_SRC_OPTIMAL).
        for &(image, buffer, aspect, w, h) in &readback {
            self.copy_out(image, buffer, aspect, w, h);
        }
        unsafe {
            dev.end_command_buffer(self.cmd)
                .map_err(vkerr("vkEndCommandBuffer"))?;
            let cmds = [self.cmd];
            dev.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                self.fence,
            )
            .map_err(vkerr("vkQueueSubmit"))?;
            dev.wait_for_fences(&[self.fence], true, u64::MAX)
                .map_err(vkerr("vkWaitForFences"))?;
            dev.reset_fences(&[self.fence])
                .map_err(vkerr("vkResetFences"))?;
        }
        let mut k = 0;
        for c in d.colors.iter_mut().flatten() {
            let (_, buffer, ..) = readback[k];
            let len = c.pixels.len();
            c.pixels = self.read_buffer(s, buffer, len)?;
            k += 1;
        }
        if let Some(z) = &mut d.depth {
            let (_, buffer, ..) = readback[k];
            let bytes = self.read_buffer(s, buffer, z.values.len() * 4)?;
            z.values = bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                .collect();
        }
        Ok(())
    }

    fn free(&self, s: Scratch) {
        unsafe {
            let dev = &self.device;
            if let Some(f) = s.framebuffer {
                dev.destroy_framebuffer(f, None);
            }
            if let Some(p) = s.pool {
                dev.destroy_descriptor_pool(p, None);
            }
            for v in s.samplers {
                dev.destroy_sampler(v, None);
            }
            for v in s.views {
                dev.destroy_image_view(v, None);
            }
            for (i, m) in s.images {
                dev.destroy_image(i, None);
                dev.free_memory(m, None);
            }
            for (b, m) in s.buffers {
                dev.destroy_buffer(b, None);
                dev.free_memory(m, None);
            }
        }
    }
}

impl Renderer for VulkanRenderer {
    fn draw(&mut self, d: &mut Draw) -> Result<(), String> {
        let mut s = Scratch::default();
        let t = std::time::Instant::now();
        let r = self.run(d, &mut s);
        if std::env::var_os("KESTREL_GPU_DEBUG").is_some() {
            eprintln!("vulkan draw {:?}", t.elapsed());
        }
        if r.is_err() {
            // A recording cut short: reset the command buffer.
            unsafe {
                let _ = self.device.device_wait_idle();
                let _ = self
                    .device
                    .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty());
            }
        }
        self.free(s);
        r
    }
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            for p in self.pipelines.values() {
                self.device.destroy_pipeline(p.pipeline, None);
                self.device.destroy_pipeline_layout(p.layout, None);
                self.device
                    .destroy_descriptor_set_layout(p.set_layout, None);
                self.device.destroy_render_pass(p.render_pass, None);
            }
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn range(aspect: vk::ImageAspectFlags) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: aspect,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

fn layers(aspect: vk::ImageAspectFlags) -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers {
        aspect_mask: aspect,
        mip_level: 0,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// An attachment kept: loaded from and stored to its image, left ready to
/// be copied out.
fn attachment(format: vk::Format, layout: vk::ImageLayout) -> vk::AttachmentDescription {
    vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(layout)
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
}

/// The renderer a run is given: Vulkan made on the first draw (most
/// programs never draw), or the reason there is none.
#[derive(Default)]
pub struct Lazy(Option<Result<VulkanRenderer, String>>);

impl Renderer for Lazy {
    fn draw(&mut self, d: &mut Draw) -> Result<(), String> {
        let made = self.0.get_or_insert_with(|| {
            let r = VulkanRenderer::new();
            if let Ok(v) = &r {
                eprintln!("note: GPU draws: Vulkan on {}", v.name);
            }
            r
        });
        match made {
            Ok(v) => v.draw(d),
            Err(e) => Err(format!("no Vulkan GPU to draw with ({e})")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_hle::gpu::draw::{
        ColorAttachment, Compare, VertexAttribute, VertexBuffer, Viewport,
    };
    use kestrel_hle::gpu::shader::ir::{Interp, Op, Operand, Program, RZ};
    use kestrel_hle::gpu::shader::spirv::{emit, AttrType, Options};
    use kestrel_hle::gpu::shader::{Header, Stage};

    /// The renderer, or None (with a note) without Vulkan, unless
    /// KESTREL_REQUIRE_VULKAN says it must work (CI's Linux x86_64 job,
    /// with lavapipe).
    fn renderer() -> Option<VulkanRenderer> {
        match VulkanRenderer::new() {
            Ok(r) => Some(r),
            Err(e) if std::env::var_os("KESTREL_REQUIRE_VULKAN").is_some() => {
                panic!("KESTREL_REQUIRE_VULKAN is set and Vulkan is not usable: {e}")
            }
            Err(e) => {
                eprintln!("note: Vulkan not usable here ({e}); the renderer test does not run");
                None
            }
        }
    }

    fn header(stage: Stage) -> Header {
        Header {
            stage,
            omap_targets: if stage == Stage::Fragment { 0xf } else { 0 },
            writes_depth: false,
            writes_sample_mask: false,
            kills_pixels: false,
        }
    }

    #[test]
    fn a_triangle_covering_the_target_writes_the_channels_asked() {
        let Some(mut r) = renderer() else { return };
        // Vertex shader: position from attribute 0, colour (attribute 1)
        // to generic output 0. Pixel shader: the colour, flat, to target 0.
        let vs = Program {
            header: header(Stage::Vertex),
            ops: vec![
                Op::LdAttr {
                    dst: 0,
                    count: 4,
                    addr: 0x80,
                },
                Op::StAttr {
                    src: 0,
                    count: 4,
                    addr: 0x70,
                },
                Op::LdAttr {
                    dst: 4,
                    count: 4,
                    addr: 0x90,
                },
                Op::StAttr {
                    src: 4,
                    count: 4,
                    addr: 0x80,
                },
                Op::Exit,
            ],
        };
        let fs = Program {
            header: header(Stage::Fragment),
            ops: (0..4)
                .map(|c| Op::Ipa {
                    dst: c,
                    addr: 0x80 + 4 * c as u16,
                    mode: Interp::Flat,
                    mult: RZ,
                    sat: false,
                })
                .chain([
                    Op::Mov {
                        dst: 4,
                        src: Operand::Reg(0),
                    },
                    Op::Exit,
                ])
                .collect(),
        };
        let opts = |base| Options {
            vertex_inputs: [AttrType::Float; 32],
            z_minus_one_to_one: false,
            binding_base: base,
        };
        let vm = emit(&vs, &opts(0)).unwrap();
        let fm = emit(&fs, &opts(64)).unwrap();
        // One triangle past the edges: (-1, -1), (3, -1), (-1, 3); flat
        // colour green from the provoking (first) vertex.
        let verts: [[f32; 8]; 3] = [
            [-1.0, -1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0],
            [3.0, -1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0],
            [-1.0, 3.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0],
        ];
        let data: Vec<u8> = verts
            .iter()
            .flatten()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        // The target holds blue 0x55 everywhere; blue is not written.
        let pixels: Vec<u8> = (0..16 * 16).flat_map(|_| [0, 0, 0x55, 0]).collect();
        let mut d = Draw {
            vertex_spirv: vm.words,
            fragment_spirv: fm.words,
            topology: Topology::Triangles,
            vertices: Vertices::Array { first: 0, count: 3 },
            instance: 0,
            buffers: vec![VertexBuffer {
                stride: 32,
                per_instance: false,
                data,
            }],
            attributes: vec![
                VertexAttribute {
                    location: 0,
                    buffer: 0,
                    offset: 0,
                    widths: 1,
                    numerical: 7,
                },
                VertexAttribute {
                    location: 1,
                    buffer: 0,
                    offset: 16,
                    widths: 1,
                    numerical: 7,
                },
            ],
            uniforms: vec![],
            textures: vec![],
            viewport: Viewport {
                x: 0.0,
                y: 0.0,
                width: 16.0,
                height: 16.0,
                min_depth: 0.0,
                max_depth: 1.0,
            },
            scissor: [0, 0, 16, 16],
            cull: 0,
            front_ccw: true,
            depth_test: None::<Compare>,
            depth_write: false,
            colors: vec![Some(ColorAttachment {
                format: 0xd5,
                width: 16,
                height: 16,
                write_mask: 0b1011,
                pixels,
            })],
            depth: None,
        };
        r.draw(&mut d).unwrap();
        let got = &d.colors[0].as_ref().unwrap().pixels;
        for (i, px) in got.chunks_exact(4).enumerate() {
            assert_eq!(px, [0, 255, 0x55, 255], "pixel {i}");
        }
    }
}
