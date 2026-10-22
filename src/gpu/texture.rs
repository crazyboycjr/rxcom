//! Images the compositor samples: X pixmaps imported as dma-bufs, images filled
//! from CPU copies, and the intermediate scene image.

use super::{Buffer, Gpu, image_barrier};
use anyhow::{Context, Result, anyhow, bail};
use ash::vk;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};

/// X pixmaps of depth 24 and 32 are 32 bits per pixel, B, G, R, A/X in memory.
pub const PIXMAP_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

#[derive(Debug)]
pub struct Texture {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
    pub width: u32,
    pub height: u32,
    /// Imported from another device user (the X server): acquired from the foreign
    /// queue family before sampling and released after.
    pub external: bool,
    /// For owned images: holds valid pixels in SHADER_READ_ONLY_OPTIMAL layout.
    pub ready: bool,
}

impl Texture {
    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            gpu.device.destroy_image_view(self.view, None);
            gpu.device.destroy_image(self.image, None);
            gpu.device.free_memory(self.memory, None);
        }
    }
}

/// One dma-buf plane as DRI3 BuffersFromPixmap describes it.
pub struct DmaBuf {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    pub depth: u8,
    pub bpp: u8,
}

fn view_for(gpu: &Gpu, image: vk::Image, format: vk::Format, opaque: bool) -> Result<vk::ImageView> {
    // Depth-24 pixmaps leave the top byte undefined, so alpha reads as one.
    let a = if opaque { vk::ComponentSwizzle::ONE } else { vk::ComponentSwizzle::IDENTITY };
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .components(vk::ComponentMapping::default().a(a))
        .subresource_range(super::color_range());
    Ok(unsafe { gpu.device.create_image_view(&info, None)? })
}

impl Gpu {
    /// Which features the device has for sampling this format with this DRM modifier,
    /// or None when the modifier is unknown or needs more than one plane.
    pub fn modifier_features(&self, modifier: u64) -> Option<vk::FormatFeatureFlags> {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { self.instance.get_physical_device_format_properties2(self.pdev, PIXMAP_FORMAT, &mut props) };
        let n = list.drm_format_modifier_count as usize;
        let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); n];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { self.instance.get_physical_device_format_properties2(self.pdev, PIXMAP_FORMAT, &mut props) };
        mods.iter()
            .find(|m| m.drm_format_modifier == modifier && m.drm_format_modifier_plane_count == 1)
            .map(|m| m.drm_format_modifier_tiling_features)
    }

    /// Wraps a pixmap's dma-buf in a sampled image without copying. The fd is
    /// consumed whether or not the import succeeds.
    pub fn import_dmabuf(&self, buf: DmaBuf) -> Result<Texture> {
        let mem_fd = self.mem_fd.as_ref().ok_or_else(|| anyhow!("device cannot import dma-bufs"))?;
        if buf.bpp != 32 || !(buf.depth == 24 || buf.depth == 32) {
            bail!("unsupported pixmap format: depth {} bpp {}", buf.depth, buf.bpp);
        }
        let feats = self
            .modifier_features(buf.modifier)
            .ok_or_else(|| anyhow!("modifier 0x{:016x} not supported for sampling", buf.modifier))?;
        if !feats.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE) {
            bail!("modifier 0x{:016x} cannot be sampled", buf.modifier);
        }
        let dev = &self.device;
        let plane = [vk::SubresourceLayout {
            offset: buf.offset as u64,
            size: 0,
            row_pitch: buf.stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        }];
        let mut modinfo = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(buf.modifier)
            .plane_layouts(&plane);
        let mut ext = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(PIXMAP_FORMAT)
            .extent(vk::Extent3D { width: buf.width, height: buf.height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext)
            .push_next(&mut modinfo);
        let image = unsafe { dev.create_image(&info, None) }.context("vkCreateImage for dma-buf")?;

        let result = (|| -> Result<Texture> {
            let mut dedicated = vk::MemoryDedicatedRequirements::default();
            let mut req2 = vk::MemoryRequirements2::default().push_next(&mut dedicated);
            unsafe {
                dev.get_image_memory_requirements2(&vk::ImageMemoryRequirementsInfo2::default().image(image), &mut req2)
            };
            let req = req2.memory_requirements;
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            unsafe {
                mem_fd.get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    buf.fd.as_raw_fd(),
                    &mut fd_props,
                )
            }
            .context("vkGetMemoryFdPropertiesKHR")?;
            let bits = req.memory_type_bits & fd_props.memory_type_bits;
            let ty = self
                .memory_type(bits, vk::MemoryPropertyFlags::empty())
                .ok_or_else(|| anyhow!("no memory type fits the dma-buf"))?;
            // The fd names the whole buffer object; its size bounds the import.
            let fd_size = unsafe { libc::lseek(buf.fd.as_raw_fd(), 0, libc::SEEK_END) };
            let size = if fd_size > 0 { (fd_size as u64).max(req.size) } else { req.size };
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(buf.fd.as_raw_fd());
            let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let mut alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(size)
                .memory_type_index(ty)
                .push_next(&mut import);
            if dedicated.requires_dedicated_allocation != 0 || dedicated.prefers_dedicated_allocation != 0 {
                alloc = alloc.push_next(&mut ded);
            }
            let memory = unsafe { dev.allocate_memory(&alloc, None) }.context("vkAllocateMemory (dma-buf import)")?;
            // Vulkan owns the fd once the import succeeded.
            let _ = buf.fd.into_raw_fd();
            if let Err(e) = unsafe { dev.bind_image_memory(image, memory, 0) } {
                unsafe { dev.free_memory(memory, None) };
                return Err(e).context("vkBindImageMemory (dma-buf)");
            }
            let view = match view_for(self, image, PIXMAP_FORMAT, buf.depth == 24) {
                Ok(v) => v,
                Err(e) => {
                    unsafe { dev.free_memory(memory, None) };
                    return Err(e);
                }
            };
            Ok(Texture {
                image,
                memory,
                view,
                width: buf.width,
                height: buf.height,
                external: true,
                ready: true,
            })
        })();
        if result.is_err() {
            unsafe { dev.destroy_image(image, None) };
        }
        result
    }

    /// A device-local image of the given size and usage, in UNDEFINED layout.
    pub fn create_texture(
        &self,
        width: u32,
        height: u32,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        opaque: bool,
    ) -> Result<Texture> {
        let dev = &self.device;
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        unsafe {
            let image = dev.create_image(&info, None)?;
            let req = dev.get_image_memory_requirements(image);
            let ty = self
                .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .or_else(|| self.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::empty()))
                .ok_or_else(|| anyhow!("no memory type for a {width}x{height} image"))?;
            let memory = dev
                .allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None)
                .inspect_err(|_| dev.destroy_image(image, None))?;
            dev.bind_image_memory(image, memory, 0)?;
            let view = view_for(self, image, format, opaque)?;
            Ok(Texture { image, memory, view, width, height, external: false, ready: false })
        }
    }

    /// An image filled from CPU copies of a pixmap of the given depth.
    pub fn create_upload_texture(&self, width: u32, height: u32, depth: u8) -> Result<Texture> {
        self.create_texture(
            width,
            height,
            PIXMAP_FORMAT,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            depth != 32,
        )
    }

    /// Records a full overwrite of `tex` from `buf` at `offset` (tightly packed rows).
    pub fn record_upload(&self, cmd: vk::CommandBuffer, tex: &mut Texture, buf: &Buffer, offset: u64) {
        image_barrier(
            self,
            cmd,
            tex.image,
            (vk::PipelineStageFlags2::FRAGMENT_SHADER, vk::AccessFlags2::NONE, vk::ImageLayout::UNDEFINED),
            (
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            ),
            None,
        );
        let region = [vk::BufferImageCopy::default()
            .buffer_offset(offset)
            .image_subresource(
                vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1),
            )
            .image_extent(vk::Extent3D { width: tex.width, height: tex.height, depth: 1 })];
        unsafe {
            self.device.cmd_copy_buffer_to_image(
                cmd,
                buf.buffer,
                tex.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region,
            )
        };
        image_barrier(
            self,
            cmd,
            tex.image,
            (vk::PipelineStageFlags2::COPY, vk::AccessFlags2::TRANSFER_WRITE, vk::ImageLayout::TRANSFER_DST_OPTIMAL),
            (
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            ),
            None,
        );
        tex.ready = true;
    }

    /// Takes an imported image from the X server for sampling (before use)...
    pub fn record_acquire(&self, cmd: vk::CommandBuffer, tex: &Texture) {
        let queues = self.foreign_queue.then_some((vk::QUEUE_FAMILY_FOREIGN_EXT, self.queue_family));
        image_barrier(
            self,
            cmd,
            tex.image,
            (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE, vk::ImageLayout::GENERAL),
            (
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            ),
            queues,
        );
    }

    /// ...and hands it back afterwards.
    pub fn record_release(&self, cmd: vk::CommandBuffer, tex: &Texture) {
        let queues = self.foreign_queue.then_some((self.queue_family, vk::QUEUE_FAMILY_FOREIGN_EXT));
        image_barrier(
            self,
            cmd,
            tex.image,
            (
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::NONE,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            ),
            (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE, vk::ImageLayout::GENERAL),
            queues,
        );
    }
}
