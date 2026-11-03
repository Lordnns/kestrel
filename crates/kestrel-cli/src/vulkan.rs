//! Vulkan presentation (the owner's choice of `ash`, 2026-11-03): the
//! window shows the program's frames through the host GPU's Vulkan driver,
//! as Dolphin's Vulkan video backend does, with softbuffer as the fallback.
//! Windows and Linux x86_64 only for now (PLAN.md section 3).
//!
//! A frame is the 0x00RRGGBB pixels `kestrel_hle::display` decoded. Each
//! one is copied into a host-visible buffer, from there into an image of
//! the frame's size, and blitted with nearest filtering into the window's
//! swapchain image over the background, as large as fits with its shape
//! kept: the same picture `window::scale` paints in software (the test
//! below renders both and compares them). The GPU's own work (phase 5B)
//! will render into images of its own and reuse this last step.

use crate::window::{fit, BACKGROUND};
use ash::vk;
use kestrel_hle::display::Frame;
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use winit::window::Window;

/// What can go wrong: a message for the "note:" line that says why the
/// window falls back to softbuffer.
type Res<T> = Result<T, String>;

fn vkerr(what: &str) -> impl Fn(vk::Result) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// A buffer the CPU writes and the GPU reads (or the other way), mapped
/// for good.
struct HostBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    size: u64,
}

/// An image in device memory.
struct Image {
    image: vk::Image,
    memory: vk::DeviceMemory,
    width: u32,
    height: u32,
}

struct Swapchain {
    handle: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    extent: vk::Extent2D,
    /// Signalled when the frame drawn into each image is done, for
    /// presentation to wait on.
    rendered: Vec<vk::Semaphore>,
}

/// The surface side: present to a window.
struct Surface {
    loader: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    swapchain_fn: ash::khr::swapchain::Device,
    swapchain: Option<Swapchain>,
    acquired: vk::Semaphore,
    /// The swapchain must be made again (resized, out of date).
    stale: bool,
}

pub struct Vulkan {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// The format of the frame image and of the window's images.
    format: vk::Format,
    staging: Option<HostBuffer>,
    frame_image: Option<Image>,
    surface: Option<Surface>,
    /// The GPU and driver, for the "note:" line.
    pub name: String,
}

/// Formats kestrel draws in, best first: 8 bits a channel, without sRGB
/// encoding first (the frame's bytes are copied as they are).
const FORMATS: [vk::Format; 4] = [
    vk::Format::B8G8R8A8_UNORM,
    vk::Format::R8G8B8A8_UNORM,
    vk::Format::B8G8R8A8_SRGB,
    vk::Format::R8G8B8A8_SRGB,
];

fn is_srgb(f: vk::Format) -> bool {
    f == vk::Format::B8G8R8A8_SRGB || f == vk::Format::R8G8B8A8_SRGB
}

/// The background as the clear colour of `format`: an sRGB format encodes
/// what it is given, so it gets the linear value whose encoding is the
/// background's bytes.
fn clear_colour(format: vk::Format) -> vk::ClearColorValue {
    let channel = |shift: u32| {
        let c = ((BACKGROUND >> shift) & 0xff) as f32 / 255.0;
        if is_srgb(format) {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        } else {
            c
        }
    };
    vk::ClearColorValue {
        float32: [channel(16), channel(8), channel(0), 0.0],
    }
}

impl Vulkan {
    /// Vulkan for `window`: an instance with the surface extensions its
    /// platform needs, a device that can draw and present to it.
    pub fn for_window(window: &Window) -> Res<Self> {
        let display = window.display_handle().map_err(|e| e.to_string())?.as_raw();
        let handle = window.window_handle().map_err(|e| e.to_string())?.as_raw();
        let platform: &std::ffi::CStr = match display {
            RawDisplayHandle::Windows(_) => ash::khr::win32_surface::NAME,
            RawDisplayHandle::Xlib(_) => ash::khr::xlib_surface::NAME,
            RawDisplayHandle::Xcb(_) => ash::khr::xcb_surface::NAME,
            RawDisplayHandle::Wayland(_) => ash::khr::wayland_surface::NAME,
            other => return Err(format!("no Vulkan surface for this window ({other:?})")),
        };
        let extensions = [ash::khr::surface::NAME.as_ptr(), platform.as_ptr()];
        let (entry, instance) = Self::instance(&extensions)?;
        // The window's surface.
        let surface = unsafe {
            match (display, handle) {
                (RawDisplayHandle::Windows(_), RawWindowHandle::Win32(w)) => {
                    let info = vk::Win32SurfaceCreateInfoKHR::default()
                        .hwnd(w.hwnd.get())
                        .hinstance(w.hinstance.map_or(0, |h| h.get()));
                    ash::khr::win32_surface::Instance::new(&entry, &instance)
                        .create_win32_surface(&info, None)
                }
                (RawDisplayHandle::Xlib(d), RawWindowHandle::Xlib(w)) => {
                    let dpy = d.display.map_or(std::ptr::null_mut(), |p| p.as_ptr());
                    let info = vk::XlibSurfaceCreateInfoKHR::default()
                        .dpy(dpy.cast())
                        .window(w.window);
                    ash::khr::xlib_surface::Instance::new(&entry, &instance)
                        .create_xlib_surface(&info, None)
                }
                (RawDisplayHandle::Xcb(d), RawWindowHandle::Xcb(w)) => {
                    let c = d.connection.map_or(std::ptr::null_mut(), |p| p.as_ptr());
                    let info = vk::XcbSurfaceCreateInfoKHR::default()
                        .connection(c.cast())
                        .window(w.window.get());
                    ash::khr::xcb_surface::Instance::new(&entry, &instance)
                        .create_xcb_surface(&info, None)
                }
                (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) => {
                    let info = vk::WaylandSurfaceCreateInfoKHR::default()
                        .display(d.display.as_ptr().cast())
                        .surface(w.surface.as_ptr().cast());
                    ash::khr::wayland_surface::Instance::new(&entry, &instance)
                        .create_wayland_surface(&info, None)
                }
                _ => {
                    instance.destroy_instance(None);
                    return Err("the window's handles do not match".into());
                }
            }
        };
        let surface = match surface {
            Ok(s) => s,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(format!("vkCreate*SurfaceKHR: {e}"));
            }
        };
        let loader = ash::khr::surface::Instance::new(&entry, &instance);
        let made = Self::device(entry, instance, Some((&loader, surface)));
        let mut v = match made {
            Ok(v) => v,
            Err(failed) => {
                let (e, instance) = *failed;
                unsafe {
                    loader.destroy_surface(surface, None);
                    instance.destroy_instance(None);
                }
                return Err(e);
            }
        };
        let swapchain_fn = ash::khr::swapchain::Device::new(&v.instance, &v.device);
        let acquired = unsafe {
            v.device
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                .map_err(vkerr("vkCreateSemaphore"))?
        };
        // The window's format: the first of FORMATS it offers.
        let offered = unsafe {
            loader
                .get_physical_device_surface_formats(v.physical, surface)
                .map_err(vkerr("vkGetPhysicalDeviceSurfaceFormatsKHR"))?
        };
        v.format = FORMATS
            .into_iter()
            .find(|f| offered.iter().any(|o| o.format == *f))
            .ok_or("the window offers no 8-bit RGBA or BGRA format")?;
        v.check_format()?;
        v.surface = Some(Surface {
            loader,
            surface,
            swapchain_fn,
            swapchain: None,
            acquired,
            stale: true,
        });
        Ok(v)
    }

    #[cfg(test)]
    /// Vulkan without a window: frames are drawn into an image read back
    /// (the test below, and later the GPU's own tests).
    pub fn offscreen() -> Res<Self> {
        let (entry, instance) = Self::instance(&[])?;
        let mut v = Self::device(entry, instance, None).map_err(|failed| {
            let (e, instance) = *failed;
            unsafe { instance.destroy_instance(None) };
            e
        })?;
        v.format = vk::Format::B8G8R8A8_UNORM;
        v.check_format()?;
        Ok(v)
    }

    fn instance(extensions: &[*const std::ffi::c_char]) -> Res<(ash::Entry, ash::Instance)> {
        // Loads vulkan-1.dll or libvulkan.so.1, which the GPU's driver
        // installs.
        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| format!("no Vulkan library on this computer ({e})"))?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"kestrel")
            .engine_name(c"kestrel")
            .api_version(vk::make_api_version(0, 1, 1, 0));
        let info = vk::InstanceCreateInfo::default()
            .application_info(&app)
            .enabled_extension_names(extensions);
        let instance =
            unsafe { entry.create_instance(&info, None) }.map_err(vkerr("vkCreateInstance"))?;
        Ok((entry, instance))
    }

    /// The first GPU with a queue that can draw (blits need a graphics
    /// queue) and, with a surface, present to it.
    fn device(
        entry: ash::Entry,
        instance: ash::Instance,
        surface: Option<(&ash::khr::surface::Instance, vk::SurfaceKHR)>,
    ) -> Result<Self, Box<(String, ash::Instance)>> {
        let fail = |e: String, instance| Err(Box::new((e, instance)));
        let physicals = match unsafe { instance.enumerate_physical_devices() } {
            Ok(p) => p,
            Err(e) => return fail(format!("vkEnumeratePhysicalDevices: {e}"), instance),
        };
        let mut found = None;
        for p in physicals {
            let families = unsafe { instance.get_physical_device_queue_family_properties(p) };
            for (i, f) in families.iter().enumerate() {
                let draws = f.queue_flags.contains(vk::QueueFlags::GRAPHICS);
                let presents = surface.map_or(true, |(loader, s)| unsafe {
                    loader
                        .get_physical_device_surface_support(p, i as u32, s)
                        .unwrap_or(false)
                });
                if draws && presents && found.is_none() {
                    found = Some((p, i as u32));
                }
            }
        }
        let Some((physical, family)) = found else {
            return fail("no Vulkan GPU that can show the window".into(), instance);
        };
        let priorities = [1.0];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&priorities)];
        let swapchain = [ash::khr::swapchain::NAME.as_ptr()];
        let extensions: &[_] = if surface.is_some() { &swapchain } else { &[] };
        let info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(extensions);
        let device = match unsafe { instance.create_device(physical, &info, None) } {
            Ok(d) => d,
            Err(e) => return fail(format!("vkCreateDevice: {e}"), instance),
        };
        let props = unsafe { instance.get_physical_device_properties(physical) };
        let name = props
            .device_name_as_c_str()
            .map_or("a Vulkan GPU".into(), |n| n.to_string_lossy().into_owned());
        let queue = unsafe { device.get_device_queue(family, 0) };
        let made = unsafe {
            let pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            );
            pool.and_then(|pool| {
                let cmd = device.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )?[0];
                let fence = device.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )?;
                Ok((pool, cmd, fence))
            })
        };
        let (pool, cmd, fence) = match made {
            Ok(m) => m,
            Err(e) => {
                unsafe { device.destroy_device(None) };
                return fail(format!("setting up commands: {e}"), instance);
            }
        };
        Ok(Vulkan {
            _entry: entry,
            instance,
            physical,
            device,
            queue,
            pool,
            cmd,
            fence,
            format: vk::Format::UNDEFINED,
            staging: None,
            frame_image: None,
            surface: None,
            name,
        })
    }

    /// The format must take copies and blits both ways with optimal tiling.
    fn check_format(&self) -> Res<()> {
        let props = unsafe {
            self.instance
                .get_physical_device_format_properties(self.physical, self.format)
        };
        let need = vk::FormatFeatureFlags::BLIT_SRC
            | vk::FormatFeatureFlags::BLIT_DST
            | vk::FormatFeatureFlags::TRANSFER_SRC
            | vk::FormatFeatureFlags::TRANSFER_DST;
        if props.optimal_tiling_features.contains(need) {
            Ok(())
        } else {
            Err(format!("{:?} cannot be blitted on this GPU", self.format))
        }
    }

    fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> Res<u32> {
        let props = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical)
        };
        (0..props.memory_type_count)
            .find(|&i| {
                bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(want)
            })
            .ok_or_else(|| format!("no Vulkan memory that is {want:?}"))
    }

    fn host_buffer(&self, size: u64, usage: vk::BufferUsageFlags) -> Res<HostBuffer> {
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
            self.device
                .bind_buffer_memory(buffer, memory, 0)
                .map_err(vkerr("vkBindBufferMemory"))?;
            let ptr = self
                .device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
                .map_err(vkerr("vkMapMemory"))?
                .cast();
            Ok(HostBuffer {
                buffer,
                memory,
                ptr,
                size,
            })
        }
    }

    fn image(&self, width: u32, height: u32, usage: vk::ImageUsageFlags) -> Res<Image> {
        unsafe {
            let image = self
                .device
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(self.format)
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
            self.device
                .bind_image_memory(image, memory, 0)
                .map_err(vkerr("vkBindImageMemory"))?;
            Ok(Image {
                image,
                memory,
                width,
                height,
            })
        }
    }

    fn drop_host_buffer(&self, b: HostBuffer) {
        unsafe {
            self.device.destroy_buffer(b.buffer, None);
            self.device.free_memory(b.memory, None);
        }
    }

    fn drop_image(&self, i: Image) {
        unsafe {
            self.device.destroy_image(i.image, None);
            self.device.free_memory(i.memory, None);
        }
    }

    /// Put `frame`'s pixels in the staging buffer and make the frame image
    /// its size. The previous frame's commands must be done (the fence).
    fn upload(&mut self, frame: &Frame) -> Res<()> {
        let size = frame.pixels.len() as u64 * 4;
        if self.staging.as_ref().map_or(true, |b| b.size < size) {
            if let Some(old) = self.staging.take() {
                self.drop_host_buffer(old);
            }
            self.staging = Some(self.host_buffer(
                size.max(4),
                vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
            )?);
        }
        let staging = self.staging.as_ref().expect("made");
        // 0x00RRGGBB little-endian is B, G, R, 0: BGRA's byte order. For an
        // RGBA format the blue and red bytes trade places.
        let rgba = matches!(
            self.format,
            vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB
        );
        let bytes = unsafe { std::slice::from_raw_parts_mut(staging.ptr, size as usize) };
        for (out, &p) in bytes.chunks_exact_mut(4).zip(&frame.pixels) {
            let p = if rgba {
                (p & 0xff00) | (p >> 16 & 0xff) | (p & 0xff) << 16
            } else {
                p
            };
            out.copy_from_slice(&p.to_le_bytes());
        }
        let fits = self
            .frame_image
            .as_ref()
            .is_some_and(|i| (i.width, i.height) == (frame.width, frame.height));
        if !fits {
            if let Some(old) = self.frame_image.take() {
                self.drop_image(old);
            }
            self.frame_image = Some(self.image(
                frame.width,
                frame.height,
                vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
            )?);
        }
        Ok(())
    }

    /// Record the commands that draw `frame` (or only the background) into
    /// `target`, `extent` big, which ends in `final_layout`.
    fn record(
        &self,
        frame: Option<&Frame>,
        target: vk::Image,
        extent: vk::Extent2D,
        final_layout: vk::ImageLayout,
    ) -> Res<()> {
        let d = &self.device;
        let cmd = self.cmd;
        let whole = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1);
        let barrier = |image, from, to, src: vk::AccessFlags, dst: vk::AccessFlags| {
            vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(from)
                .new_layout(to)
                .src_access_mask(src)
                .dst_access_mask(dst)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .subresource_range(whole)
        };
        let transfer = vk::PipelineStageFlags::TRANSFER;
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .map_err(vkerr("vkResetCommandBuffer"))?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(vkerr("vkBeginCommandBuffer"))?;
            // The window's image: cleared to the background.
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                transfer,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    target,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                )],
            );
            d.cmd_clear_color_image(
                cmd,
                target,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &clear_colour(self.format),
                &[whole],
            );
            if let (Some(frame), Some(image), Some(staging)) =
                (frame, &self.frame_image, &self.staging)
            {
                // The frame's pixels into its image.
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    transfer,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier(
                        image.image,
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        vk::AccessFlags::empty(),
                        vk::AccessFlags::TRANSFER_WRITE,
                    )],
                );
                d.cmd_copy_buffer_to_image(
                    cmd,
                    staging.buffer,
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy::default()
                        .image_subresource(layers)
                        .image_extent(vk::Extent3D {
                            width: frame.width,
                            height: frame.height,
                            depth: 1,
                        })],
                );
                // Both images ready for the blit: the clear and the copy
                // done.
                d.cmd_pipeline_barrier(
                    cmd,
                    transfer,
                    transfer,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[
                        barrier(
                            image.image,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            vk::AccessFlags::TRANSFER_WRITE,
                            vk::AccessFlags::TRANSFER_READ,
                        ),
                        barrier(
                            target,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::AccessFlags::TRANSFER_WRITE,
                            vk::AccessFlags::TRANSFER_WRITE,
                        ),
                    ],
                );
                // As large as fits with its shape kept, centred, nearest
                // pixels: `window::fit`, as the software path.
                if let Some((x, y, w, h)) = fit(
                    frame.width as usize,
                    frame.height as usize,
                    extent.width as usize,
                    extent.height as usize,
                ) {
                    let corner = |x: usize, y: usize| vk::Offset3D {
                        x: x as i32,
                        y: y as i32,
                        z: 0,
                    };
                    let blit = vk::ImageBlit::default()
                        .src_subresource(layers)
                        .src_offsets([corner(0, 0), {
                            let mut c = corner(frame.width as usize, frame.height as usize);
                            c.z = 1;
                            c
                        }])
                        .dst_subresource(layers)
                        .dst_offsets([corner(x, y), {
                            let mut c = corner(x + w, y + h);
                            c.z = 1;
                            c
                        }]);
                    d.cmd_blit_image(
                        cmd,
                        image.image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        target,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[blit],
                        vk::Filter::NEAREST,
                    );
                }
            }
            let (dst_access, dst_stage) = if final_layout == vk::ImageLayout::PRESENT_SRC_KHR {
                (
                    vk::AccessFlags::empty(),
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                )
            } else {
                (vk::AccessFlags::TRANSFER_READ, transfer)
            };
            d.cmd_pipeline_barrier(
                cmd,
                transfer,
                dst_stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    target,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    final_layout,
                    vk::AccessFlags::TRANSFER_WRITE,
                    dst_access,
                )],
            );
            Ok(())
        }
    }

    /// Wait for the last frame's commands, so their buffers can be reused.
    fn wait_last(&self) -> Res<()> {
        unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .map_err(vkerr("vkWaitForFences"))?;
            self.device
                .reset_fences(&[self.fence])
                .map_err(vkerr("vkResetFences"))
        }
    }

    /// Make the window's swapchain again, `width` x `height` unless the
    /// surface says its size.
    fn make_swapchain(&mut self, width: u32, height: u32) -> Res<()> {
        let s = self.surface.as_mut().expect("a window");
        unsafe {
            self.device
                .device_wait_idle()
                .map_err(vkerr("vkDeviceWaitIdle"))?;
            let caps = s
                .loader
                .get_physical_device_surface_capabilities(self.physical, s.surface)
                .map_err(vkerr("vkGetPhysicalDeviceSurfaceCapabilitiesKHR"))?;
            if !caps
                .supported_usage_flags
                .contains(vk::ImageUsageFlags::TRANSFER_DST)
            {
                return Err("the window's images cannot be copied into".into());
            }
            let extent = if caps.current_extent.width != u32::MAX {
                caps.current_extent
            } else {
                vk::Extent2D {
                    width: width.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                    height: height
                        .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
                }
            };
            let mut count = caps.min_image_count + 1;
            if caps.max_image_count > 0 {
                count = count.min(caps.max_image_count);
            }
            let alpha = [
                vk::CompositeAlphaFlagsKHR::OPAQUE,
                vk::CompositeAlphaFlagsKHR::INHERIT,
                vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
                vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
            ]
            .into_iter()
            .find(|a| caps.supported_composite_alpha.contains(*a))
            .unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);
            let old = s.swapchain.take();
            let info = vk::SwapchainCreateInfoKHR::default()
                .surface(s.surface)
                .min_image_count(count)
                .image_format(self.format)
                .image_color_space(vk::ColorSpaceKHR::SRGB_NONLINEAR)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::TRANSFER_DST)
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                .pre_transform(caps.current_transform)
                .composite_alpha(alpha)
                // FIFO waits for the display's vsync, and every driver has
                // it.
                .present_mode(vk::PresentModeKHR::FIFO)
                .clipped(true)
                .old_swapchain(old.as_ref().map_or(vk::SwapchainKHR::null(), |o| o.handle));
            let made = s.swapchain_fn.create_swapchain(&info, None);
            if let Some(old) = old {
                for r in old.rendered {
                    self.device.destroy_semaphore(r, None);
                }
                s.swapchain_fn.destroy_swapchain(old.handle, None);
            }
            let handle = made.map_err(vkerr("vkCreateSwapchainKHR"))?;
            let images = s
                .swapchain_fn
                .get_swapchain_images(handle)
                .map_err(vkerr("vkGetSwapchainImagesKHR"))?;
            let mut rendered = Vec::new();
            for _ in &images {
                rendered.push(
                    self.device
                        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                        .map_err(vkerr("vkCreateSemaphore"))?,
                );
            }
            s.swapchain = Some(Swapchain {
                handle,
                images,
                extent,
                rendered,
            });
            s.stale = false;
        }
        Ok(())
    }

    /// The window changed size: make the swapchain again before the next
    /// frame.
    pub fn resized(&mut self) {
        if let Some(s) = &mut self.surface {
            s.stale = true;
        }
    }

    /// Show `frame` (or only the background) in the window, `width` x
    /// `height` pixels. Waits for the display's vsync when the driver's
    /// queue is full (FIFO).
    pub fn present(&mut self, frame: Option<&Frame>, width: u32, height: u32) -> Res<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        if self.surface.as_ref().is_some_and(|s| s.stale) {
            self.make_swapchain(width, height)?;
        }
        self.wait_last()?;
        let s = self.surface.as_ref().expect("a window");
        let sc = s.swapchain.as_ref().expect("made");
        let acquired = unsafe {
            s.swapchain_fn
                .acquire_next_image(sc.handle, u64::MAX, s.acquired, vk::Fence::null())
        };
        let index = match acquired {
            Ok((i, suboptimal)) => {
                if suboptimal {
                    self.resized();
                }
                i
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                // Nothing was submitted: signal the fence again so the next
                // frame does not wait for ever.
                self.resized();
                unsafe {
                    self.device
                        .queue_submit(self.queue, &[], self.fence)
                        .map_err(vkerr("vkQueueSubmit"))?;
                }
                return Ok(());
            }
            Err(e) => return Err(format!("vkAcquireNextImageKHR: {e}")),
        };
        if let Some(f) = frame {
            self.upload(f)?;
        }
        let s = self.surface.as_ref().expect("a window");
        let sc = s.swapchain.as_ref().expect("made");
        let (target, extent, rendered) = (
            sc.images[index as usize],
            sc.extent,
            sc.rendered[index as usize],
        );
        self.record(frame, target, extent, vk::ImageLayout::PRESENT_SRC_KHR)?;
        let s = self.surface.as_ref().expect("a window");
        let sc = s.swapchain.as_ref().expect("made");
        unsafe {
            self.device
                .end_command_buffer(self.cmd)
                .map_err(vkerr("vkEndCommandBuffer"))?;
            let waits = [s.acquired];
            let stages = [vk::PipelineStageFlags::TRANSFER];
            let cmds = [self.cmd];
            let signals = [rendered];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&cmds)
                .signal_semaphores(&signals);
            self.device
                .queue_submit(self.queue, &[submit], self.fence)
                .map_err(vkerr("vkQueueSubmit"))?;
            let chains = [sc.handle];
            let indices = [index];
            let present = vk::PresentInfoKHR::default()
                .wait_semaphores(&signals)
                .swapchains(&chains)
                .image_indices(&indices);
            match s.swapchain_fn.queue_present(self.queue, &present) {
                Ok(false) => {}
                Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.resized(),
                Err(e) => return Err(format!("vkQueuePresentKHR: {e}")),
            }
        }
        Ok(())
    }

    #[cfg(test)]
    /// Draw `frame` as the window would, into a `width` x `height` image,
    /// and read it back as 0x00RRGGBB pixels.
    pub fn render_offscreen(
        &mut self,
        frame: Option<&Frame>,
        width: u32,
        height: u32,
    ) -> Res<Vec<u32>> {
        self.wait_last()?;
        if let Some(f) = frame {
            self.upload(f)?;
        }
        let target = self.image(
            width,
            height,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
        )?;
        let readback = self.host_buffer(
            width as u64 * height as u64 * 4,
            vk::BufferUsageFlags::TRANSFER_DST,
        )?;
        let extent = vk::Extent2D { width, height };
        self.record(
            frame,
            target.image,
            extent,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        )?;
        let pixels = unsafe {
            self.device.cmd_copy_image_to_buffer(
                self.cmd,
                target.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                readback.buffer,
                &[vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })],
            );
            self.device
                .end_command_buffer(self.cmd)
                .map_err(vkerr("vkEndCommandBuffer"))?;
            let cmds = [self.cmd];
            self.device
                .queue_submit(
                    self.queue,
                    &[vk::SubmitInfo::default().command_buffers(&cmds)],
                    self.fence,
                )
                .map_err(vkerr("vkQueueSubmit"))?;
            self.device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .map_err(vkerr("vkWaitForFences"))?;
            let bytes = std::slice::from_raw_parts(readback.ptr, readback.size as usize);
            // Reset the fence the next frame waits on, signalled again.
            self.device
                .reset_fences(&[self.fence])
                .map_err(vkerr("vkResetFences"))?;
            self.device
                .queue_submit(self.queue, &[], self.fence)
                .map_err(vkerr("vkQueueSubmit"))?;
            bytes
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect()
        };
        self.drop_host_buffer(readback);
        self.drop_image(target);
        Ok(pixels)
    }
}

impl Drop for Vulkan {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            if let Some(s) = self.surface.take() {
                if let Some(sc) = s.swapchain {
                    for r in sc.rendered {
                        self.device.destroy_semaphore(r, None);
                    }
                    s.swapchain_fn.destroy_swapchain(sc.handle, None);
                }
                self.device.destroy_semaphore(s.acquired, None);
                s.loader.destroy_surface(s.surface, None);
            }
            if let Some(b) = self.staging.take() {
                self.drop_host_buffer(b);
            }
            if let Some(i) = self.frame_image.take() {
                self.drop_image(i);
            }
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::{fit, scale};

    /// Vulkan for a test, or None (with a note) where this computer has no
    /// Vulkan driver, unless KESTREL_REQUIRE_VULKAN says it must (CI's Linux
    /// x86_64 job installs lavapipe, Mesa's software driver, and sets it).
    fn vulkan() -> Option<Vulkan> {
        match Vulkan::offscreen() {
            Ok(v) => Some(v),
            Err(e) if std::env::var_os("KESTREL_REQUIRE_VULKAN").is_some() => {
                panic!("KESTREL_REQUIRE_VULKAN is set and Vulkan is not usable: {e}")
            }
            Err(e) => {
                eprintln!("note: Vulkan not usable here ({e}); the Vulkan test does not run");
                None
            }
        }
    }

    /// Whether `got` is `frame` drawn into a `ww` x `wh` window as
    /// `window::scale` draws it: the bars exactly, and inside the frame each
    /// window pixel the frame pixel under its centre. Where that centre
    /// falls exactly on the edge between two frame pixels, either may be
    /// taken: the Vulkan specification lets a nearest blit compute the
    /// source coordinate with floating-point error ("Copying Data Between
    /// Images", vkCmdBlitImage), and lavapipe takes the left or upper one
    /// where `scale`'s exact arithmetic takes the right or lower one.
    fn check(frame: &Frame, ww: u32, wh: u32, got: &[u32]) -> Result<(), String> {
        let (fw, fh) = (frame.width as usize, frame.height as usize);
        let (ww, wh) = (ww as usize, wh as usize);
        let mut want = vec![0; ww * wh];
        scale(frame, &mut want, ww, wh);
        let (x0, y0, w, h) = fit(fw, fh, ww, wh).unwrap();
        // The frame pixels a window coordinate may take: the one under its
        // centre, and the one before when the centre is on an edge.
        let under = |i: usize, n: usize, size: usize| {
            let k = (2 * i + 1) * n / (2 * size);
            let edge = (2 * i + 1) * n % (2 * size) == 0;
            if edge && k > 0 {
                vec![k, k - 1]
            } else {
                vec![k]
            }
        };
        for y in 0..wh {
            for x in 0..ww {
                let g = got[y * ww + x];
                let inside = (x0..x0 + w).contains(&x) && (y0..y0 + h).contains(&y);
                let ok = if inside {
                    under(y - y0, fh, h).iter().any(|&sy| {
                        under(x - x0, fw, w)
                            .iter()
                            .any(|&sx| frame.pixels[sy * fw + sx] == g)
                    })
                } else {
                    g == want[y * ww + x]
                };
                if !ok {
                    return Err(format!(
                        "pixel ({x}, {y}): {g:#08x}, software {:#08x}",
                        want[y * ww + x]
                    ));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn vulkan_draws_frames_as_the_software_path_does() {
        let Some(mut v) = vulkan() else { return };
        eprintln!("Vulkan on {}", v.name);
        // Frames of several shapes into windows of several shapes: bars
        // above and below, at the sides, scaled down and up, by whole
        // numbers and by fractions.
        let frame = |w: u32, h: u32| Frame {
            width: w,
            height: h,
            pixels: (0..w * h)
                .map(|i| i.wrapping_mul(0x9e37_79b9) >> 8)
                .collect(),
        };
        for (fw, fh, ww, wh) in [
            (2, 1, 4, 4),
            (1, 2, 4, 2),
            (4, 4, 2, 2),
            (96, 40, 160, 90),
            (1280, 720, 1920, 1080),
            (1280, 720, 1000, 700),
            (32, 8, 33, 17),
        ] {
            let f = frame(fw, fh);
            let got = v.render_offscreen(Some(&f), ww, wh).unwrap();
            if let Err(e) = check(&f, ww, wh, &got) {
                panic!("{fw}x{fh} into {ww}x{wh}: {e}");
            }
        }
        // No frame yet: the background only.
        let got = v.render_offscreen(None, 8, 8).unwrap();
        assert!(got.iter().all(|&p| p == BACKGROUND));
    }
}
