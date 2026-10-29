//! The Vulkan presenter. The X server composes the desktop into a scene image
//! it shares with Vulkan through a dma-buf; each frame one pass draws that
//! image, 1:1 or zoomed, into a swapchain image on the overlay window, and the
//! driver flips it at vblank. Nothing here ever waits for a vblank.

use crate::x::X;
use crate::zoom::View;
use anyhow::{Context, Result, anyhow, bail};
use ash::vk::{AccessFlags2 as A, ImageLayout as L, PipelineStageFlags2 as S};
use ash::{ext, khr, vk};
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

const VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/present.vert.spv"));
const FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/present.frag.spv"));
/// Depth-24 pixmaps hold B, G, R and an unused byte.
const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;
const DMA_BUF: vk::ExternalMemoryHandleTypeFlags = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
const IGNORED: (u32, u32) = (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED);
const REQUIRED: [&CStr; 4] = [
    khr::swapchain::NAME,
    khr::external_memory_fd::NAME,
    ext::external_memory_dma_buf::NAME,
    ext::image_drm_format_modifier::NAME,
];
// VK_EXT_present_mode_fifo_latest_ready is newer than ash's headers: its name,
// present mode and feature struct.
const LATEST_READY_EXT: &CStr = c"VK_EXT_present_mode_fifo_latest_ready";
const FIFO_LATEST_READY: vk::PresentModeKHR = vk::PresentModeKHR::from_raw(1_000_361_000);

#[repr(C)]
struct LatestReadyFeature {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    enabled: vk::Bool32,
}
unsafe impl vk::ExtendsPhysicalDeviceFeatures2 for LatestReadyFeature {}
unsafe impl vk::ExtendsDeviceCreateInfo for LatestReadyFeature {}

#[repr(C)]
struct Push {
    crop: [f32; 2],
    origin: [f32; 2],
    inv_scale: f32,
    linear: u32,
}

/// The instance and the overlay's surface, destroyed after everything else.
struct Surface {
    /// Vulkan presents through a connection of its own, so its traffic never
    /// interleaves with rxcom's events.
    _wsi: XCBConnection,
    entry: ash::Entry,
    instance: ash::Instance,
    fns: khr::surface::Instance,
    surface: vk::SurfaceKHR,
    /// VK_EXT_surface_maintenance1, which present fences need.
    maintenance: bool,
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            self.fns.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

struct Swapchain {
    handle: vk::SwapchainKHR,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    /// Signalled when rendering into image i is done; its present waits on it.
    rendered: Vec<vk::Semaphore>,
}

struct Slot {
    cmd: vk::CommandBuffer,
    acquired: vk::Semaphore,
    /// The timeline value of the frame last recorded here.
    value: u64,
    /// Signalled once the presentation engine is done with this slot's last
    /// present (VK_EXT_swapchain_maintenance1), so nothing it uses is destroyed
    /// or reused too early.
    presented: vk::Fence,
}

pub struct Gpu {
    pdev: vk::PhysicalDevice,
    device: ash::Device,
    swapchain_fn: khr::swapchain::Device,
    queue: vk::Queue,
    family: u32,
    /// VK_EXT_queue_family_foreign: the scene passes to and from the X server.
    foreign: bool,
    present_fences: bool,
    mode: vk::PresentModeKHR,
    surface_format: vk::SurfaceFormatKHR,
    swapchain: Option<Swapchain>,
    scene: vk::Image,
    scene_memory: vk::DeviceMemory,
    scene_view: vk::ImageView,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    cmd_pool: vk::CommandPool,
    slots: Vec<Slot>,
    timeline: vk::Semaphore,
    frame: u64,
    acquired: Option<(u32, usize)>,
    pub route: String,
    s: Surface,
}

fn surface(x: &X, display: Option<&str>) -> Result<Surface> {
    let name = display.map(CString::new).transpose()?;
    let (wsi, _) = XCBConnection::connect(name.as_deref()).context("a second X connection for Vulkan")?;
    let entry = unsafe { ash::Entry::load() }.context("cannot load libvulkan")?;
    let app = vk::ApplicationInfo::default().application_name(c"rxcom").api_version(vk::API_VERSION_1_3);
    let mut exts = vec![khr::surface::NAME.as_ptr(), khr::xcb_surface::NAME.as_ptr()];
    let available = unsafe { entry.enumerate_instance_extension_properties(None) }.unwrap_or_default();
    let wanted = [khr::get_surface_capabilities2::NAME, ext::surface_maintenance1::NAME];
    let maintenance = wanted.iter().all(|n| available.iter().any(|e| e.extension_name_as_c_str() == Ok(*n)));
    if maintenance {
        exts.extend(wanted.iter().map(|n| n.as_ptr()));
    }
    let info = vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&exts);
    let instance = unsafe { entry.create_instance(&info, None) }.context("vkCreateInstance")?;
    let fns = khr::surface::Instance::new(&entry, &instance);
    let mut s = Surface { _wsi: wsi, entry, instance, fns, surface: vk::SurfaceKHR::null(), maintenance };
    let info =
        vk::XcbSurfaceCreateInfoKHR::default().connection(s._wsi.get_raw_xcb_connection().cast()).window(x.overlay);
    s.surface = unsafe { khr::xcb_surface::Instance::new(&s.entry, &s.instance).create_xcb_surface(&info, None) }
        .context("vkCreateXcbSurfaceKHR")?;
    Ok(s)
}

/// A Vulkan 1.3 device that presents to the overlay and shares dma-bufs, with
/// what it offers beyond that: the foreign queue family, present fences and
/// FIFO_LATEST_READY.
fn create_device(s: &Surface) -> Result<(vk::PhysicalDevice, u32, ash::Device, bool, bool, vk::PresentModeKHR)> {
    let mut best = None;
    for pd in unsafe { s.instance.enumerate_physical_devices()? } {
        let props = unsafe { s.instance.get_physical_device_properties(pd) };
        let exts = unsafe { s.instance.enumerate_device_extension_properties(pd)? };
        let has = |n: &CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(n));
        let families = unsafe { s.instance.get_physical_device_queue_family_properties(pd) };
        let family = (0..families.len() as u32).find(|&i| {
            families[i as usize].queue_flags.contains(vk::QueueFlags::GRAPHICS)
                && unsafe { s.fns.get_physical_device_surface_support(pd, i, s.surface) }.unwrap_or(false)
        });
        let score = (props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU) as u8;
        if let Some(f) = family
            && props.api_version >= vk::API_VERSION_1_3
            && REQUIRED.iter().all(|n| has(n))
            && best.as_ref().is_none_or(|b: &(u8, _, _, _)| score > b.0)
        {
            best = Some((score, pd, f, exts));
        }
    }
    let (_, pdev, family, exts) = best.context("no Vulkan 1.3 device presents to the overlay and shares dma-bufs")?;
    let has = |n: &CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(n));
    let mut names: Vec<*const c_char> = REQUIRED.iter().map(|n| n.as_ptr()).collect();
    let foreign = has(ext::queue_family_foreign::NAME);
    if foreign {
        names.push(ext::queue_family_foreign::NAME.as_ptr());
    }
    let mut latest = LatestReadyFeature {
        s_type: vk::StructureType::from_raw(1_000_361_000),
        p_next: std::ptr::null_mut(),
        enabled: 0,
    };
    let mut fences = vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT::default();
    let mut f = vk::PhysicalDeviceFeatures2::default();
    if has(LATEST_READY_EXT) {
        f = f.push_next(&mut latest);
    }
    if s.maintenance && has(ext::swapchain_maintenance1::NAME) {
        f = f.push_next(&mut fences);
    }
    unsafe { s.instance.get_physical_device_features2(pdev, &mut f) };
    (latest.p_next, fences.p_next) = (std::ptr::null_mut(), std::ptr::null_mut());
    let modes = unsafe { s.fns.get_physical_device_surface_present_modes(pdev, s.surface)? };
    let latest_ready = latest.enabled == vk::TRUE && modes.contains(&FIFO_LATEST_READY);
    let present_fences = fences.swapchain_maintenance1 == vk::TRUE;
    let mut f12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
    let mut f13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true).synchronization2(true);
    let mut info = vk::DeviceCreateInfo::default().push_next(&mut f12).push_next(&mut f13);
    if latest_ready {
        names.push(LATEST_READY_EXT.as_ptr());
        info = info.push_next(&mut latest);
    }
    if present_fences {
        names.push(ext::swapchain_maintenance1::NAME.as_ptr());
        info = info.push_next(&mut fences);
    }
    let prio = [1.0f32];
    let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
    let info = info.queue_create_infos(&queues).enabled_extension_names(&names);
    let device = unsafe { s.instance.create_device(pdev, &info, None) }.context("vkCreateDevice")?;
    let mode = if latest_ready { FIFO_LATEST_READY } else { vk::PresentModeKHR::FIFO };
    Ok((pdev, family, device, foreign, present_fences, mode))
}

type State = (vk::PipelineStageFlags2, vk::AccessFlags2, vk::ImageLayout);

fn barrier(
    image: vk::Image,
    from: State,
    to: State,
    (src_queue, dst_queue): (u32, u32),
) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .image(image)
        .src_stage_mask(from.0)
        .src_access_mask(from.1)
        .old_layout(from.2)
        .dst_stage_mask(to.0)
        .dst_access_mask(to.1)
        .new_layout(to.2)
        .src_queue_family_index(src_queue)
        .dst_queue_family_index(dst_queue)
        .subresource_range(color_range())
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1)
}

fn view(d: &ash::Device, image: vk::Image, format: vk::Format) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(color_range());
    Ok(unsafe { d.create_image_view(&info, None)? })
}

impl Gpu {
    /// Sets Vulkan up to present on the overlay window and shares a screen-sized
    /// scene with the X server. Returns the presenter and the scene's pixmap.
    pub fn new(x: &X, display: Option<&str>) -> Result<(Gpu, u32)> {
        let s = surface(x, display)?;
        let (pdev, family, device, foreign, present_fences, mode) = create_device(&s)?;
        let formats = unsafe { s.fns.get_physical_device_surface_formats(pdev, s.surface) }.unwrap_or_default();
        let surface_format =
            formats.iter().find(|f| f.format == FORMAT).or(formats.first()).copied().unwrap_or_default();
        let props = unsafe { s.instance.get_physical_device_properties(pdev) };
        let mut gpu = Gpu {
            swapchain_fn: khr::swapchain::Device::new(&s.instance, &device),
            queue: unsafe { device.get_device_queue(family, 0) },
            pdev,
            device,
            family,
            foreign,
            present_fences,
            mode,
            surface_format,
            swapchain: None,
            scene: vk::Image::null(),
            scene_memory: vk::DeviceMemory::null(),
            scene_view: vk::ImageView::null(),
            sampler: vk::Sampler::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            cmd_pool: vk::CommandPool::null(),
            slots: Vec::new(),
            timeline: vk::Semaphore::null(),
            frame: 0,
            acquired: None,
            route: String::new(),
            s,
        };
        // From here on Drop releases whatever has been created.
        let pixmap = gpu.share_scene(x)?;
        if let Err(e) = gpu.setup() {
            let _ = x.conn.free_pixmap(pixmap);
            return Err(e);
        }
        let name = props.device_name_as_c_str().map_or("?".into(), |n| n.to_string_lossy());
        gpu.route = format!("Vulkan on {name}, {}", gpu.route);
        Ok((gpu, pixmap))
    }

    fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Result<u32> {
        let props = unsafe { self.s.instance.get_physical_device_memory_properties(self.pdev) };
        (0..props.memory_type_count)
            .find(|&i| bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(flags))
            .ok_or_else(|| anyhow!("no memory type for the scene"))
    }

    fn create_scene_image(&mut self, w: u32, h: u32, mut modifiers: impl vk::ExtendsImageCreateInfo) -> Result<()> {
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(DMA_BUF);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D { width: w, height: h, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .push_next(&mut external)
            .push_next(&mut modifiers);
        self.scene = unsafe { self.device.create_image(&info, None) }.context("vkCreateImage for the scene")?;
        Ok(())
    }

    /// The DRM modifiers Vulkan can sample and export a w x h scene with.
    fn exportable_modifiers(&self, w: u32, h: u32) -> Vec<u64> {
        let query = |list: &mut vk::DrmFormatModifierPropertiesListEXT| unsafe {
            let mut props = vk::FormatProperties2::default().push_next(list);
            self.s.instance.get_physical_device_format_properties2(self.pdev, FORMAT, &mut props);
        };
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        query(&mut list);
        let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
        query(&mut vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods));
        let exportable = |modifier: u64| {
            let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default().drm_format_modifier(modifier);
            let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(DMA_BUF);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(FORMAT)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .push_next(&mut drm)
                .push_next(&mut ext);
            let mut external = vk::ExternalImageFormatProperties::default();
            let mut props = vk::ImageFormatProperties2::default().push_next(&mut external);
            let ok =
                unsafe { self.s.instance.get_physical_device_image_format_properties2(self.pdev, &info, &mut props) };
            let max = props.image_format_properties.max_extent;
            let features = external.external_memory_properties.external_memory_features;
            ok.is_ok()
                && max.width >= w
                && max.height >= h
                && features.contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
        };
        mods.iter()
            .filter(|m| m.drm_format_modifier_plane_count == 1)
            .filter(|m| m.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE))
            .map(|m| m.drm_format_modifier)
            .filter(|&m| exportable(m))
            .collect()
    }

    /// A Vulkan image with a DRM modifier both sides take, given to the X server
    /// with PixmapFromBuffers; failing that, a pixmap of the X server's own,
    /// exported once with BuffersFromPixmap.
    fn share_scene(&mut self, x: &X) -> Result<u32> {
        let (w, h) = (x.width as u32, x.height as u32);
        let theirs = x.modifiers()?;
        let mods: Vec<u64> = self.exportable_modifiers(w, h).into_iter().filter(|m| theirs.contains(m)).collect();
        let pixmap = match self.export_scene(x, w, h, &mods) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("the X server does not take a Vulkan image ({e:#}); importing an X pixmap instead");
                self.destroy_scene();
                self.import_scene(x, w, h)?
            }
        };
        self.scene_view = view(&self.device, self.scene, FORMAT)?;
        Ok(pixmap)
    }

    fn export_scene(&mut self, x: &X, w: u32, h: u32, mods: &[u64]) -> Result<u32> {
        if mods.is_empty() {
            bail!("no DRM modifier both Vulkan and the X server take");
        }
        self.create_scene_image(
            w,
            h,
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(mods),
        )?;
        let (d, instance) = (&self.device, &self.s.instance);
        let req = unsafe { d.get_image_memory_requirements(self.scene) };
        let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(DMA_BUF);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(self.scene);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(self.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?)
            .push_next(&mut export)
            .push_next(&mut dedicated);
        self.scene_memory = unsafe { d.allocate_memory(&alloc, None) }.context("allocating the scene")?;
        let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
        let plane = vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT);
        let fd_info = vk::MemoryGetFdInfoKHR::default().memory(self.scene_memory).handle_type(DMA_BUF);
        let (layout, fd) = unsafe {
            d.bind_image_memory(self.scene, self.scene_memory, 0)?;
            ext::image_drm_format_modifier::Device::new(instance, d)
                .get_image_drm_format_modifier_properties(self.scene, &mut props)?;
            let fd = khr::external_memory_fd::Device::new(instance, d).get_memory_fd(&fd_info)?;
            (d.get_image_subresource_layout(self.scene, plane), OwnedFd::from_raw_fd(fd))
        };
        let m = props.drm_format_modifier;
        let pixmap = x.import_pixmap(fd, (w as u16, h as u16), layout.row_pitch as u32, layout.offset as u32, m)?;
        self.route = format!("a Vulkan image the X server imports (PixmapFromBuffers, modifier 0x{m:016x})");
        Ok(pixmap)
    }

    fn import_scene(&mut self, x: &X, w: u32, h: u32) -> Result<u32> {
        let pixmap = x.conn.generate_id()?;
        x.conn.create_pixmap(24, pixmap, x.root, w as u16, h as u16)?;
        let r = (|| -> Result<()> {
            let (fd, stride, offset, m) = x.buffer_from_pixmap(pixmap)?;
            let plane = [vk::SubresourceLayout { row_pitch: stride as u64, ..Default::default() }];
            let explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(m);
            self.create_scene_image(w, h, explicit.plane_layouts(&plane))?;
            let d = &self.device;
            let req = unsafe { d.get_image_memory_requirements(self.scene) };
            // NVIDIA mishandles a plane offset, so the pixmap's offset goes into
            // the bind, which the spec treats as equivalent (DESIGN.md 2.1).
            if !(offset as u64).is_multiple_of(req.alignment) {
                bail!("pixmap offset {offset} breaks the alignment {}", req.alignment);
            }
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            let mem_fd = khr::external_memory_fd::Device::new(&self.s.instance, d);
            unsafe { mem_fd.get_memory_fd_properties(DMA_BUF, fd.as_raw_fd(), &mut fd_props)? };
            let ty =
                self.memory_type(req.memory_type_bits & fd_props.memory_type_bits, vk::MemoryPropertyFlags::empty())?;
            let size =
                (unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) }.max(0) as u64).max(offset as u64 + req.size);
            let mut import = vk::ImportMemoryFdInfoKHR::default().handle_type(DMA_BUF).fd(fd.as_raw_fd());
            let alloc =
                vk::MemoryAllocateInfo::default().allocation_size(size).memory_type_index(ty).push_next(&mut import);
            self.scene_memory = unsafe { d.allocate_memory(&alloc, None) }.context("importing the scene")?;
            // Vulkan owns the fd once the import succeeded.
            let _ = fd.into_raw_fd();
            unsafe { d.bind_image_memory(self.scene, self.scene_memory, offset as u64)? };
            self.route = format!("an X pixmap Vulkan imports (BuffersFromPixmap, modifier 0x{m:016x})");
            Ok(())
        })();
        if r.is_err() {
            let _ = x.conn.free_pixmap(pixmap);
        }
        r.map(|_| pixmap)
    }

    fn destroy_scene(&mut self) {
        unsafe {
            self.device.destroy_image_view(self.scene_view, None);
            self.device.destroy_image(self.scene, None);
            self.device.free_memory(self.scene_memory, None);
        }
        (self.scene_view, self.scene, self.scene_memory) = Default::default();
    }

    /// The pipeline, its descriptor, the command buffers and the timeline.
    fn setup(&mut self) -> Result<()> {
        let d = &self.device;
        unsafe {
            let info = vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE);
            self.sampler = d.create_sampler(&info, None)?;
            let samplers = [self.sampler];
            let ty = vk::DescriptorType::COMBINED_IMAGE_SAMPLER;
            let binding = [vk::DescriptorSetLayoutBinding::default()
                .descriptor_type(ty)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .immutable_samplers(&samplers)];
            let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&binding);
            self.set_layout = d.create_descriptor_set_layout(&info, None)?;
            let sizes = [vk::DescriptorPoolSize { ty, descriptor_count: 1 }];
            self.pool = d.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&sizes),
                None,
            )?;
            let layouts = [self.set_layout];
            let alloc = vk::DescriptorSetAllocateInfo::default().descriptor_pool(self.pool).set_layouts(&layouts);
            self.set = d.allocate_descriptor_sets(&alloc)?[0];
            let image = [vk::DescriptorImageInfo::default()
                .image_view(self.scene_view)
                .image_layout(L::SHADER_READ_ONLY_OPTIMAL)];
            let write = vk::WriteDescriptorSet::default().dst_set(self.set).descriptor_type(ty).image_info(&image);
            d.update_descriptor_sets(&[write], &[]);
            let size = std::mem::size_of::<Push>() as u32;
            let range = [vk::PushConstantRange::default().stage_flags(vk::ShaderStageFlags::FRAGMENT).size(size)];
            let info = vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts).push_constant_ranges(&range);
            self.layout = d.create_pipeline_layout(&info, None)?;
            self.pipeline = self.create_pipeline()?;
            let flags = vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER;
            let info = vk::CommandPoolCreateInfo::default().queue_family_index(self.family).flags(flags);
            self.cmd_pool = d.create_command_pool(&info, None)?;
            let mut timeline = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
            self.timeline = d.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut timeline), None)?;
            // One more slot than swapchain images can be queued at once.
            for _ in 0..5 {
                let alloc =
                    vk::CommandBufferAllocateInfo::default().command_pool(self.cmd_pool).command_buffer_count(1);
                let cmd = d.allocate_command_buffers(&alloc)?[0];
                let acquired = d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
                let signalled = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
                self.slots.push(Slot { cmd, acquired, value: 0, presented: d.create_fence(&signalled, None)? });
            }
        }
        Ok(())
    }

    fn create_pipeline(&self) -> Result<vk::Pipeline> {
        let d = &self.device;
        let module = |code: &[u8]| -> Result<vk::ShaderModule> {
            let words = ash::util::read_spv(&mut std::io::Cursor::new(code))?;
            Ok(unsafe { d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)? })
        };
        let (vs, fs) = (module(VERT)?, module(FRAG)?);
        let stage = |s, m| vk::PipelineShaderStageCreateInfo::default().stage(s).module(m).name(c"main");
        let stages = [stage(vk::ShaderStageFlags::VERTEX, vs), stage(vk::ShaderStageFlags::FRAGMENT, fs)];
        let input = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly =
            vk::PipelineInputAssemblyStateCreateInfo::default().topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default().line_width(1.0);
        let samples =
            vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let attachment =
            [vk::PipelineColorBlendAttachmentState::default().color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachment);
        let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
        let formats = [self.surface_format.format];
        let mut rendering = vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&samples)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(self.layout)
            .push_next(&mut rendering);
        let r = unsafe { d.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) };
        unsafe {
            d.destroy_shader_module(vs, None);
            d.destroy_shader_module(fs, None);
        }
        Ok(r.map_err(|(_, e)| e)?[0])
    }

    fn create_swapchain(&mut self) -> Result<()> {
        let mut caps = unsafe { self.s.fns.get_physical_device_surface_capabilities(self.pdev, self.s.surface)? };
        if caps.current_extent.width == u32::MAX {
            caps.current_extent = caps.max_image_extent;
        }
        let mut count = (caps.min_image_count + 1).max(3);
        if caps.max_image_count != 0 {
            count = count.min(caps.max_image_count);
        }
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.s.surface)
            .min_image_count(count)
            .image_format(self.surface_format.format)
            .image_color_space(self.surface_format.color_space)
            .image_extent(caps.current_extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(self.mode)
            .clipped(true);
        let d = &self.device;
        let handle = unsafe { self.swapchain_fn.create_swapchain(&info, None) }.context("vkCreateSwapchainKHR")?;
        let images = unsafe { self.swapchain_fn.get_swapchain_images(handle)? };
        let mut sc = Swapchain { handle, extent: caps.current_extent, views: Vec::new(), rendered: Vec::new(), images };
        for &image in &sc.images {
            sc.views.push(view(d, image, self.surface_format.format)?);
            sc.rendered.push(unsafe { d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? });
        }
        log::info!(
            "swapchain {}x{}, {} images, {}{}",
            sc.extent.width,
            sc.extent.height,
            sc.images.len(),
            if self.mode == FIFO_LATEST_READY { "FIFO_LATEST_READY" } else { "FIFO" },
            if self.present_fences { ", present fences" } else { "" }
        );
        self.swapchain = Some(sc);
        Ok(())
    }

    /// Destroys the swapchain once the GPU (for at most a second) and the
    /// presentation engine (for at most 100 ms) are done with it: before rxcom
    /// unredirects, and when the driver calls it out of date.
    pub fn drop_swapchain(&mut self) {
        let d = &self.device;
        let (timeline, frame) = ([self.timeline], [self.frame]);
        let fences: Vec<vk::Fence> = self.slots.iter().map(|s| s.presented).collect();
        unsafe {
            if self.timeline != vk::Semaphore::null() {
                let _ = d.wait_semaphores(
                    &vk::SemaphoreWaitInfo::default().semaphores(&timeline).values(&frame),
                    1_000_000_000,
                );
            }
            let _ = if self.present_fences {
                d.wait_for_fences(&fences, true, 100_000_000)
            } else {
                d.queue_wait_idle(self.queue)
            };
            if let Some(sc) = self.swapchain.take() {
                for (&v, &s) in sc.views.iter().zip(&sc.rendered) {
                    d.destroy_image_view(v, None);
                    d.destroy_semaphore(s, None);
                }
                self.swapchain_fn.destroy_swapchain(sc.handle, None);
            }
            // An acquired image's semaphore stays signalled; replace it.
            if let Some((_, slot)) = self.acquired.take()
                && let Ok(s) = d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            {
                d.destroy_semaphore(std::mem::replace(&mut self.slots[slot].acquired, s), None);
            }
        }
    }

    /// Whether the GPU has finished reading the scene, so X may draw into it.
    pub fn scene_free(&self) -> Result<bool> {
        Ok(unsafe { self.device.get_semaphore_counter_value(self.timeline)? } >= self.frame)
    }

    /// Takes a swapchain image for the next frame without waiting. False when
    /// none is free: the presentation engine holds them all until a vblank.
    pub fn acquire(&mut self) -> Result<bool> {
        if self.acquired.is_some() {
            return Ok(true);
        }
        if self.swapchain.is_none() {
            self.create_swapchain()?;
        }
        let slot = (self.frame % self.slots.len() as u64) as usize;
        let s = &self.slots[slot];
        let presented = !self.present_fences || unsafe { self.device.get_fence_status(s.presented)? };
        if !presented || unsafe { self.device.get_semaphore_counter_value(self.timeline)? } < s.value {
            return Ok(false);
        }
        let sc = self.swapchain.as_ref().unwrap();
        match unsafe { self.swapchain_fn.acquire_next_image(sc.handle, 0, s.acquired, vk::Fence::null()) } {
            Ok((image, _)) => self.acquired = Some((image, slot)),
            Err(vk::Result::NOT_READY | vk::Result::TIMEOUT) => return Ok(false),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                log::info!("swapchain out of date at acquire");
                self.drop_swapchain();
                return Ok(false);
            }
            Err(e) => return Err(e).context("vkAcquireNextImageKHR"),
        }
        Ok(true)
    }

    /// Draws the scene with `view` into the acquired image and presents it.
    pub fn draw(&mut self, view: View, linear: bool) -> Result<()> {
        let (image, slot) = self.acquired.take().context("no image acquired")?;
        let sc = self.swapchain.as_ref().context("no swapchain")?;
        let (d, cmd, i) = (&self.device, self.slots[slot].cmd, image as usize);
        let ours = if self.foreign { (vk::QUEUE_FAMILY_FOREIGN_EXT, self.family) } else { IGNORED };
        let theirs = (ours.1, ours.0);
        // The X server leaves the scene in GENERAL; the GPU samples it, draws the
        // swapchain image and hands it to the presentation engine.
        let x_owned = (S::NONE, A::NONE, L::GENERAL);
        let sampled = (S::FRAGMENT_SHADER, A::SHADER_SAMPLED_READ, L::SHADER_READ_ONLY_OPTIMAL);
        let fresh = (S::COLOR_ATTACHMENT_OUTPUT, A::NONE, L::UNDEFINED);
        let target = (S::COLOR_ATTACHMENT_OUTPUT, A::COLOR_ATTACHMENT_WRITE, L::COLOR_ATTACHMENT_OPTIMAL);
        let shown = (S::NONE, A::NONE, L::PRESENT_SRC_KHR);
        let before = [barrier(self.scene, x_owned, sampled, ours), barrier(sc.images[i], fresh, target, IGNORED)];
        let after = [barrier(self.scene, sampled, x_owned, theirs), barrier(sc.images[i], target, shown, IGNORED)];
        let push = Push {
            crop: view.crop.map(|c| c as f32),
            origin: view.origin.map(|o| o as f32),
            inv_scale: (1.0 / view.scale) as f32,
            linear: linear as u32,
        };
        // SAFETY: Push is a plain #[repr(C)] struct of 4-byte fields.
        let push = unsafe { std::slice::from_raw_parts((&push as *const Push).cast::<u8>(), size_of::<Push>()) };
        let area = vk::Rect2D { offset: vk::Offset2D::default(), extent: sc.extent };
        let (width, height) = (sc.extent.width as f32, sc.extent.height as f32);
        let attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(sc.views[i])
            .image_layout(L::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)];
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            let begin = vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            d.begin_command_buffer(cmd, &begin)?;
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&before));
            d.cmd_begin_rendering(
                cmd,
                &vk::RenderingInfo::default().render_area(area).layer_count(1).color_attachments(&attachment),
            );
            d.cmd_set_viewport(cmd, 0, &[vk::Viewport { width, height, max_depth: 1.0, ..Default::default() }]);
            d.cmd_set_scissor(cmd, 0, &[area]);
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            d.cmd_bind_descriptor_sets(cmd, vk::PipelineBindPoint::GRAPHICS, self.layout, 0, &[self.set], &[]);
            d.cmd_push_constants(cmd, self.layout, vk::ShaderStageFlags::FRAGMENT, 0, push);
            d.cmd_draw(cmd, 3, 1, 0, 0);
            d.cmd_end_rendering(cmd);
            d.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&after));
            d.end_command_buffer(cmd)?;

            self.frame += 1;
            self.slots[slot].value = self.frame;
            let waits = [vk::SemaphoreSubmitInfo::default().semaphore(self.slots[slot].acquired).stage_mask(target.0)];
            let signals = [
                vk::SemaphoreSubmitInfo::default().semaphore(sc.rendered[i]).stage_mask(S::ALL_COMMANDS),
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(self.timeline)
                    .value(self.frame)
                    .stage_mask(S::ALL_COMMANDS),
            ];
            let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            let submit = vk::SubmitInfo2::default()
                .wait_semaphore_infos(&waits)
                .command_buffer_infos(&cmds)
                .signal_semaphore_infos(&signals);
            d.queue_submit2(self.queue, &[submit], vk::Fence::null()).context("vkQueueSubmit2")?;
            let (wait, swapchains, images, fence) =
                ([sc.rendered[i]], [sc.handle], [image], [self.slots[slot].presented]);
            let mut present =
                vk::PresentInfoKHR::default().wait_semaphores(&wait).swapchains(&swapchains).image_indices(&images);
            let mut fence_info = vk::SwapchainPresentFenceInfoEXT::default().fences(&fence);
            if self.present_fences {
                d.reset_fences(&fence)?;
                present = present.push_next(&mut fence_info);
            }
            match self.swapchain_fn.queue_present(self.queue, &present) {
                Ok(false) => {}
                Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    log::info!("swapchain suboptimal or out of date at present");
                    self.drop_swapchain();
                }
                Err(e) => return Err(e).context("vkQueuePresentKHR"),
            }
        }
        Ok(())
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.drop_swapchain();
        self.destroy_scene();
        let d = &self.device;
        unsafe {
            for s in &self.slots {
                d.destroy_semaphore(s.acquired, None);
                d.destroy_fence(s.presented, None);
            }
            d.destroy_semaphore(self.timeline, None);
            d.destroy_command_pool(self.cmd_pool, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_descriptor_pool(self.pool, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_device(None);
        }
    }
}
