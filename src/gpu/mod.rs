//! Vulkan context: instance, device, queue and the extension loaders rxcom uses.

pub mod present;
pub mod render;
pub mod texture;

use anyhow::{Context, Result, anyhow, bail};
use ash::vk;
use std::ffi::{CStr, c_void};

pub struct Buffer {
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
    pub size: u64,
    pub ptr: *mut u8,
}

impl Buffer {
    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            gpu.device.destroy_buffer(self.buffer, None);
            gpu.device.free_memory(self.memory, None);
        }
    }
}

pub struct GpuOptions<'a> {
    /// The libxcb connection and window to present to; None renders offscreen only.
    pub present: Option<(*mut c_void, u32)>,
    pub want_dmabuf: bool,
    /// Pick the first device whose name contains this.
    pub device_filter: Option<&'a str>,
    pub validation: bool,
}

pub struct Gpu {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub pdev: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    pub name: String,
    pub push_desc: ash::khr::push_descriptor::Device,
    pub surface_fn: ash::khr::surface::Instance,
    pub surface: vk::SurfaceKHR,
    pub swapchain_fn: Option<ash::khr::swapchain::Device>,
    /// Present when the device can import dma-bufs with explicit DRM modifiers.
    pub mem_fd: Option<ash::khr::external_memory_fd::Device>,
    /// VK_EXT_queue_family_foreign: imported images are acquired from and released
    /// to the foreign queue family around each use, as the spec asks.
    pub foreign_queue: bool,
    pub cmd_pool: vk::CommandPool,
    debug: Option<(ash::ext::debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
}

unsafe extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _ty: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user: *mut c_void,
) -> vk::Bool32 {
    let msg = unsafe { (*data).message_as_c_str() }.map_or("".into(), |m| m.to_string_lossy());
    if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        log::error!("vulkan: {msg}");
    } else if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::WARNING) {
        log::warn!("vulkan: {msg}");
    } else {
        log::debug!("vulkan: {msg}");
    }
    vk::FALSE
}

fn has_ext(list: &[vk::ExtensionProperties], name: &CStr) -> bool {
    list.iter().any(|e| e.extension_name_as_c_str() == Ok(name))
}

const DMABUF_EXTS: [&CStr; 3] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
];

impl Gpu {
    pub fn new(opts: &GpuOptions) -> Result<Gpu> {
        let entry = unsafe { ash::Entry::load() }.context("cannot load libvulkan")?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"rxcom")
            .api_version(vk::API_VERSION_1_3);
        let mut inst_exts = vec![ash::khr::surface::NAME.as_ptr(), ash::khr::xcb_surface::NAME.as_ptr()];
        let mut layers = Vec::new();
        if opts.validation {
            inst_exts.push(ash::ext::debug_utils::NAME.as_ptr());
            layers.push(c"VK_LAYER_KHRONOS_validation".as_ptr());
        }
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app)
                    .enabled_extension_names(&inst_exts)
                    .enabled_layer_names(&layers),
                None,
            )
        }
        .context("vkCreateInstance")?;

        let debug = if opts.validation {
            let du = ash::ext::debug_utils::Instance::new(&entry, &instance);
            let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
                .message_severity(
                    vk::DebugUtilsMessageSeverityFlagsEXT::ERROR | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING,
                )
                .message_type(
                    vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                        | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                        | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
                )
                .pfn_user_callback(Some(debug_callback));
            let m = unsafe { du.create_debug_utils_messenger(&info, None)? };
            Some((du, m))
        } else {
            None
        };

        let surface_fn = ash::khr::surface::Instance::new(&entry, &instance);
        let surface = match opts.present {
            Some((conn, window)) => {
                let xcb = ash::khr::xcb_surface::Instance::new(&entry, &instance);
                let info = vk::XcbSurfaceCreateInfoKHR::default().connection(conn.cast()).window(window);
                unsafe { xcb.create_xcb_surface(&info, None) }.context("vkCreateXcbSurfaceKHR")?
            }
            None => vk::SurfaceKHR::null(),
        };

        let mut best: Option<(i32, vk::PhysicalDevice, u32, bool, bool, String)> = None;
        for pd in unsafe { instance.enumerate_physical_devices()? } {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let name = props.device_name_as_c_str().map_or("?".into(), |n| n.to_string_lossy().into_owned());
            if props.api_version < vk::API_VERSION_1_3 {
                log::debug!("skipping {name}: Vulkan 1.3 needed");
                continue;
            }
            if let Some(f) = opts.device_filter {
                if !name.to_lowercase().contains(&f.to_lowercase()) {
                    continue;
                }
            }
            let exts = unsafe { instance.enumerate_device_extension_properties(pd)? };
            if !has_ext(&exts, ash::khr::push_descriptor::NAME) {
                log::debug!("skipping {name}: no VK_KHR_push_descriptor");
                continue;
            }
            if opts.present.is_some() && !has_ext(&exts, ash::khr::swapchain::NAME) {
                continue;
            }
            let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            let qf = families.iter().enumerate().position(|(i, f)| {
                f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                    && (surface == vk::SurfaceKHR::null()
                        || unsafe { surface_fn.get_physical_device_surface_support(pd, i as u32, surface) }
                            .unwrap_or(false))
            });
            let Some(qf) = qf else {
                log::debug!("skipping {name}: no graphics queue that can present to the overlay");
                continue;
            };
            let dmabuf = DMABUF_EXTS.iter().all(|e| has_ext(&exts, e));
            let foreign = has_ext(&exts, ash::ext::queue_family_foreign::NAME);
            let mut score = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 30,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 20,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 10,
                _ => 0,
            };
            if opts.want_dmabuf && dmabuf {
                score += 5;
            }
            log::debug!("candidate {name}: score {score}, dma-buf import {dmabuf}");
            if best.as_ref().is_none_or(|b| score > b.0) {
                best = Some((score, pd, qf as u32, dmabuf, foreign, name));
            }
        }
        let Some((_, pdev, queue_family, dmabuf, foreign_queue, name)) = best else {
            bail!("no suitable Vulkan device (need Vulkan 1.3, push descriptors and presentation to the overlay)");
        };

        let mut dev_exts = vec![ash::khr::push_descriptor::NAME.as_ptr()];
        if opts.present.is_some() {
            dev_exts.push(ash::khr::swapchain::NAME.as_ptr());
        }
        let use_dmabuf = opts.want_dmabuf && dmabuf;
        if use_dmabuf {
            dev_exts.extend(DMABUF_EXTS.iter().map(|e| e.as_ptr()));
            if foreign_queue {
                dev_exts.push(ash::ext::queue_family_foreign::NAME.as_ptr());
            }
        }
        let prio = [1.0f32];
        let qinfo = [vk::DeviceQueueCreateInfo::default().queue_family_index(queue_family).queue_priorities(&prio)];
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true).synchronization2(true);
        let device = unsafe {
            instance.create_device(
                pdev,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&qinfo)
                    .enabled_extension_names(&dev_exts)
                    .push_next(&mut f13),
                None,
            )
        }
        .context("vkCreateDevice")?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pdev) };
        let cmd_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?
        };
        log::info!(
            "Vulkan device: {name} (dma-buf import {}, foreign queue {})",
            if use_dmabuf { "yes" } else { "no" },
            foreign_queue && use_dmabuf
        );
        Ok(Gpu {
            push_desc: ash::khr::push_descriptor::Device::new(&instance, &device),
            swapchain_fn: opts.present.map(|_| ash::khr::swapchain::Device::new(&instance, &device)),
            mem_fd: use_dmabuf.then(|| ash::khr::external_memory_fd::Device::new(&instance, &device)),
            foreign_queue: foreign_queue && use_dmabuf,
            entry,
            instance,
            pdev,
            device,
            queue,
            queue_family,
            mem_props,
            name,
            surface_fn,
            surface,
            cmd_pool,
            debug,
        })
    }

    pub fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|&i| {
            bits & (1 << i) != 0 && self.mem_props.memory_types[i as usize].property_flags.contains(flags)
        })
    }

    /// A host-visible, coherent, persistently mapped buffer.
    pub fn host_buffer(&self, size: u64, usage: vk::BufferUsageFlags) -> Result<Buffer> {
        unsafe {
            let buffer = self.device.create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(usage).sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )?;
            let req = self.device.get_buffer_memory_requirements(buffer);
            let ty = self
                .memory_type(
                    req.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .ok_or_else(|| anyhow!("no host-visible memory type"))?;
            let memory = self
                .device
                .allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None)?;
            self.device.bind_buffer_memory(buffer, memory, 0)?;
            let ptr = self.device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8;
            Ok(Buffer { buffer, memory, size, ptr })
        }
    }

    /// Records commands with `f`, submits them and waits. For setup and tests only.
    pub fn one_shot(&self, f: impl FnOnce(vk::CommandBuffer)) -> Result<()> {
        unsafe {
            let cmd = self.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            self.device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            f(cmd);
            self.device.end_command_buffer(cmd)?;
            let fence = self.device.create_fence(&vk::FenceCreateInfo::default(), None)?;
            let cmds = [cmd];
            let r = self
                .device
                .queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&cmds)], fence)
                .and_then(|_| self.device.wait_for_fences(&[fence], true, u64::MAX));
            self.device.destroy_fence(fence, None);
            self.device.free_command_buffers(self.cmd_pool, &cmds);
            r?;
        }
        Ok(())
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_device(None);
            if self.surface != vk::SurfaceKHR::null() {
                self.surface_fn.destroy_surface(self.surface, None);
            }
            if let Some((du, m)) = self.debug.take() {
                du.destroy_debug_utils_messenger(m, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

/// A full-image color subresource range.
pub fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

/// Records a single image layout transition with synchronization2.
#[allow(clippy::too_many_arguments)]
pub fn image_barrier(
    gpu: &Gpu,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    (src_stage, src_access, old): (vk::PipelineStageFlags2, vk::AccessFlags2, vk::ImageLayout),
    (dst_stage, dst_access, new): (vk::PipelineStageFlags2, vk::AccessFlags2, vk::ImageLayout),
    queues: Option<(u32, u32)>,
) {
    let (sq, dq) = queues.unwrap_or((vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED));
    let b = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src_stage)
        .src_access_mask(src_access)
        .old_layout(old)
        .dst_stage_mask(dst_stage)
        .dst_access_mask(dst_access)
        .new_layout(new)
        .src_queue_family_index(sq)
        .dst_queue_family_index(dq)
        .image(image)
        .subresource_range(color_range())];
    unsafe {
        gpu.device
            .cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&b));
    }
}
