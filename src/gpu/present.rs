//! The swapchain on the composite overlay window.

use super::Gpu;
use anyhow::{Context, Result, anyhow};
use ash::vk;

pub struct Swapchain {
    pub swapchain: vk::SwapchainKHR,
    pub format: vk::Format,
    pub extent: vk::Extent2D,
    pub images: Vec<vk::Image>,
    pub views: Vec<vk::ImageView>,
    /// Signalled when rendering into image i is done; presentation waits on it.
    pub rendered: Vec<vk::Semaphore>,
}

impl Swapchain {
    /// Creates (or recreates, retiring `old`) a FIFO swapchain of `want` size.
    pub fn new(gpu: &Gpu, want: vk::Extent2D, old: Option<Swapchain>) -> Result<Swapchain> {
        let sfn = &gpu.surface_fn;
        let swfn = gpu.swapchain_fn.as_ref().ok_or_else(|| anyhow!("no swapchain support"))?;
        let caps = unsafe { sfn.get_physical_device_surface_capabilities(gpu.pdev, gpu.surface)? };
        let formats = unsafe { sfn.get_physical_device_surface_formats(gpu.pdev, gpu.surface)? };
        // UNORM, never sRGB: X pixel values are already display-encoded and are
        // blended as they are, like xcompmgr and picom do.
        let pick = [vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM]
            .into_iter()
            .find_map(|f| formats.iter().find(|s| s.format == f))
            .or(formats.first())
            .ok_or_else(|| anyhow!("surface reports no formats"))?;
        let extent = if caps.current_extent.width != u32::MAX { caps.current_extent } else { want };
        let mut count = caps.min_image_count.max(2);
        if caps.max_image_count != 0 {
            count = count.min(caps.max_image_count);
        }
        let alpha = [
            vk::CompositeAlphaFlagsKHR::OPAQUE,
            vk::CompositeAlphaFlagsKHR::INHERIT,
            vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
        ]
        .into_iter()
        .find(|a| caps.supported_composite_alpha.contains(*a))
        .unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);
        let old_handle = old.as_ref().map_or(vk::SwapchainKHR::null(), |o| o.swapchain);
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(gpu.surface)
            .min_image_count(count)
            .image_format(pick.format)
            .image_color_space(pick.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
            .composite_alpha(alpha)
            .present_mode(vk::PresentModeKHR::FIFO)
            .clipped(true)
            .old_swapchain(old_handle);
        let swapchain = unsafe { swfn.create_swapchain(&info, None) }.context("vkCreateSwapchainKHR")?;
        if let Some(o) = old {
            o.destroy(gpu);
        }
        let images = unsafe { swfn.get_swapchain_images(swapchain)? };
        let mut views = Vec::new();
        let mut rendered = Vec::new();
        for &image in &images {
            let info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(pick.format)
                .subresource_range(super::color_range());
            views.push(unsafe { gpu.device.create_image_view(&info, None)? });
            rendered.push(unsafe { gpu.device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)? });
        }
        log::info!(
            "swapchain {}x{} {:?}, {} images, FIFO",
            extent.width,
            extent.height,
            pick.format,
            images.len()
        );
        Ok(Swapchain { swapchain, format: pick.format, extent, images, views, rendered })
    }

    /// The caller makes sure the device is idle.
    pub fn destroy(self, gpu: &Gpu) {
        unsafe {
            for v in &self.views {
                gpu.device.destroy_image_view(*v, None);
            }
            for s in &self.rendered {
                gpu.device.destroy_semaphore(*s, None);
            }
            if let Some(f) = gpu.swapchain_fn.as_ref() {
                f.destroy_swapchain(self.swapchain, None);
            }
        }
    }
}
