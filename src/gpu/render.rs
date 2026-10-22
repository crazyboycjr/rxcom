//! Pipelines and command recording: textured rectangles into a target, and the
//! full-screen zoom pass that samples the composed scene.

use super::Gpu;
use super::texture::Texture;
use anyhow::Result;
use ash::vk;
use std::io::Cursor;

const QUAD_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/quad.vert.spv"));
const QUAD_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/quad.frag.spv"));
const ZOOM_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/zoom.vert.spv"));
const ZOOM_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/zoom.frag.spv"));

#[repr(C)]
#[derive(Clone, Copy)]
struct QuadPush {
    target_size: [f32; 2],
    dst_pos: [f32; 2],
    dst_size: [f32; 2],
    uv_scale: [f32; 2],
    uv_offset: [f32; 2],
    opacity: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ZoomPush {
    scene_size: [f32; 2],
    origin: [f32; 2],
    zoom: f32,
}

fn bytes<T: Copy>(v: &T) -> &[u8] {
    // SAFETY: T is a plain #[repr(C)] struct of f32s.
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

/// One textured rectangle of the scene, in target pixels.
pub struct Quad<'a> {
    pub tex: &'a Texture,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Repeat the texture over the rectangle (the root background) instead of
    /// stretching it.
    pub tiled: bool,
    pub opacity: f32,
}

pub struct Renderer {
    pub format: vk::Format,
    set_layout: vk::DescriptorSetLayout,
    quad_layout: vk::PipelineLayout,
    zoom_layout: vk::PipelineLayout,
    quad_pipeline: vk::Pipeline,
    zoom_pipeline: vk::Pipeline,
    nearest: vk::Sampler,
    repeat: vk::Sampler,
    linear: vk::Sampler,
}

fn shader(gpu: &Gpu, code: &[u8]) -> Result<vk::ShaderModule> {
    let words = ash::util::read_spv(&mut Cursor::new(code))?;
    Ok(unsafe { gpu.device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)? })
}

fn sampler(gpu: &Gpu, filter: vk::Filter, mode: vk::SamplerAddressMode) -> Result<vk::Sampler> {
    let info = vk::SamplerCreateInfo::default()
        .mag_filter(filter)
        .min_filter(filter)
        .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
        .address_mode_u(mode)
        .address_mode_v(mode)
        .address_mode_w(mode)
        .max_lod(0.0);
    Ok(unsafe { gpu.device.create_sampler(&info, None)? })
}

fn pipeline(
    gpu: &Gpu,
    layout: vk::PipelineLayout,
    format: vk::Format,
    vert: &[u8],
    frag: &[u8],
    topology: vk::PrimitiveTopology,
    blend: bool,
) -> Result<vk::Pipeline> {
    let vs = shader(gpu, vert)?;
    let fs = shader(gpu, frag)?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main"),
        vk::PipelineShaderStageCreateInfo::default().stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"main"),
    ];
    let vi = vk::PipelineVertexInputStateCreateInfo::default();
    let ia = vk::PipelineInputAssemblyStateCreateInfo::default().topology(topology);
    let vp = vk::PipelineViewportStateCreateInfo::default().viewport_count(1).scissor_count(1);
    let rs = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .line_width(1.0);
    let ms = vk::PipelineMultisampleStateCreateInfo::default().rasterization_samples(vk::SampleCountFlags::TYPE_1);
    // Premultiplied alpha: dst = src + dst * (1 - src.a).
    let att = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(blend)
        .src_color_blend_factor(vk::BlendFactor::ONE)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let cb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&att);
    let dyns = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let ds = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyns);
    let formats = [format];
    let mut rendering = vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vi)
        .input_assembly_state(&ia)
        .viewport_state(&vp)
        .rasterization_state(&rs)
        .multisample_state(&ms)
        .color_blend_state(&cb)
        .dynamic_state(&ds)
        .layout(layout)
        .push_next(&mut rendering);
    let r = unsafe { gpu.device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) };
    unsafe {
        gpu.device.destroy_shader_module(vs, None);
        gpu.device.destroy_shader_module(fs, None);
    }
    Ok(r.map_err(|(_, e)| e)?[0])
}

impl Renderer {
    pub fn new(gpu: &Gpu, format: vk::Format) -> Result<Renderer> {
        let dev = &gpu.device;
        let binding = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let set_layout = unsafe {
            dev.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default()
                    .flags(vk::DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR_KHR)
                    .bindings(&binding),
                None,
            )?
        };
        let sets = [set_layout];
        let layout_with = |size: usize, stages: vk::ShaderStageFlags| -> Result<vk::PipelineLayout> {
            let range = [vk::PushConstantRange::default().stage_flags(stages).size(size as u32)];
            Ok(unsafe {
                dev.create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&sets).push_constant_ranges(&range),
                    None,
                )?
            })
        };
        let quad_layout = layout_with(
            std::mem::size_of::<QuadPush>(),
            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
        )?;
        let zoom_layout = layout_with(std::mem::size_of::<ZoomPush>(), vk::ShaderStageFlags::FRAGMENT)?;
        let quad_pipeline = pipeline(
            gpu,
            quad_layout,
            format,
            QUAD_VERT,
            QUAD_FRAG,
            vk::PrimitiveTopology::TRIANGLE_STRIP,
            true,
        )?;
        let zoom_pipeline = pipeline(
            gpu,
            zoom_layout,
            format,
            ZOOM_VERT,
            ZOOM_FRAG,
            vk::PrimitiveTopology::TRIANGLE_LIST,
            false,
        )?;
        Ok(Renderer {
            format,
            set_layout,
            quad_layout,
            zoom_layout,
            quad_pipeline,
            zoom_pipeline,
            nearest: sampler(gpu, vk::Filter::NEAREST, vk::SamplerAddressMode::CLAMP_TO_EDGE)?,
            repeat: sampler(gpu, vk::Filter::NEAREST, vk::SamplerAddressMode::REPEAT)?,
            linear: sampler(gpu, vk::Filter::LINEAR, vk::SamplerAddressMode::CLAMP_TO_EDGE)?,
        })
    }

    pub fn destroy(&self, gpu: &Gpu) {
        let dev = &gpu.device;
        unsafe {
            dev.destroy_pipeline(self.quad_pipeline, None);
            dev.destroy_pipeline(self.zoom_pipeline, None);
            dev.destroy_pipeline_layout(self.quad_layout, None);
            dev.destroy_pipeline_layout(self.zoom_layout, None);
            dev.destroy_descriptor_set_layout(self.set_layout, None);
            dev.destroy_sampler(self.nearest, None);
            dev.destroy_sampler(self.repeat, None);
            dev.destroy_sampler(self.linear, None);
        }
    }

    fn begin(&self, gpu: &Gpu, cmd: vk::CommandBuffer, view: vk::ImageView, extent: vk::Extent2D, clear: Option<[f32; 4]>) {
        let att = [vk::RenderingAttachmentInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(if clear.is_some() { vk::AttachmentLoadOp::CLEAR } else { vk::AttachmentLoadOp::DONT_CARE })
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue { color: vk::ClearColorValue { float32: clear.unwrap_or_default() } })];
        let area = vk::Rect2D { offset: vk::Offset2D::default(), extent };
        unsafe {
            gpu.device.cmd_begin_rendering(
                cmd,
                &vk::RenderingInfo::default().render_area(area).layer_count(1).color_attachments(&att),
            );
            gpu.device.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            gpu.device.cmd_set_scissor(cmd, 0, &[area]);
        }
    }

    fn push_texture(&self, gpu: &Gpu, cmd: vk::CommandBuffer, layout: vk::PipelineLayout, view: vk::ImageView, s: vk::Sampler) {
        let info = [vk::DescriptorImageInfo::default()
            .sampler(s)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let write = [vk::WriteDescriptorSet::default()
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&info)];
        unsafe {
            gpu.push_desc.cmd_push_descriptor_set(cmd, vk::PipelineBindPoint::GRAPHICS, layout, 0, &write);
        }
    }

    /// Draws the quads bottom to top over `clear` into a target that must be in
    /// COLOR_ATTACHMENT_OPTIMAL layout.
    pub fn record_scene(
        &self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        target: vk::ImageView,
        extent: vk::Extent2D,
        clear: [f32; 4],
        quads: &[Quad],
    ) {
        self.begin(gpu, cmd, target, extent, Some(clear));
        let dev = &gpu.device;
        unsafe { dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.quad_pipeline) };
        for q in quads {
            let (tw, th) = (q.tex.width as f32, q.tex.height as f32);
            let push = QuadPush {
                target_size: [extent.width as f32, extent.height as f32],
                dst_pos: [q.x, q.y],
                dst_size: [q.width, q.height],
                uv_scale: if q.tiled { [1.0 / tw, 1.0 / th] } else { [1.0 / q.width, 1.0 / q.height] },
                uv_offset: [0.0, 0.0],
                opacity: q.opacity,
            };
            let s = if q.tiled { self.repeat } else { self.nearest };
            self.push_texture(gpu, cmd, self.quad_layout, q.tex.view, s);
            unsafe {
                dev.cmd_push_constants(
                    cmd,
                    self.quad_layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    bytes(&push),
                );
                dev.cmd_draw(cmd, 4, 1, 0, 0);
            }
        }
        unsafe { dev.cmd_end_rendering(cmd) };
    }

    /// Fills the target with the scene magnified by `zoom` around `origin`; the
    /// scene must be in SHADER_READ_ONLY_OPTIMAL layout.
    #[allow(clippy::too_many_arguments)]
    pub fn record_zoom(
        &self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        target: vk::ImageView,
        extent: vk::Extent2D,
        scene: &Texture,
        zoom: f64,
        origin: (f64, f64),
    ) {
        self.begin(gpu, cmd, target, extent, None);
        let dev = &gpu.device;
        let push = ZoomPush {
            scene_size: [scene.width as f32, scene.height as f32],
            origin: [origin.0 as f32, origin.1 as f32],
            zoom: zoom as f32,
        };
        unsafe { dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.zoom_pipeline) };
        self.push_texture(gpu, cmd, self.zoom_layout, scene.view, self.linear);
        unsafe {
            dev.cmd_push_constants(cmd, self.zoom_layout, vk::ShaderStageFlags::FRAGMENT, 0, bytes(&push));
            dev.cmd_draw(cmd, 3, 1, 0, 0);
            dev.cmd_end_rendering(cmd);
        }
    }
}

/// Renders quads into a new offscreen image and reads it back as BGRA rows.
/// Used by the component tests; the compositor renders into swapchain images.
pub fn render_offscreen(gpu: &Gpu, renderer: &Renderer, width: u32, height: u32, clear: [f32; 4], quads: &[Quad]) -> Result<Vec<u8>> {
    use super::image_barrier;
    let target = gpu.create_texture(
        width,
        height,
        renderer.format,
        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
        false,
    )?;
    let size = width as u64 * height as u64 * 4;
    let buf = gpu.host_buffer(size, vk::BufferUsageFlags::TRANSFER_DST)?;
    let extent = vk::Extent2D { width, height };
    let mut external: Vec<&Texture> = Vec::new();
    for q in quads.iter().filter(|q| q.tex.external) {
        if !external.iter().any(|t| t.image == q.tex.image) {
            external.push(q.tex);
        }
    }
    let r = gpu.one_shot(|cmd| {
        for t in &external {
            gpu.record_acquire(cmd, t);
        }
        image_barrier(
            gpu,
            cmd,
            target.image,
            (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE, vk::ImageLayout::UNDEFINED),
            (
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            ),
            None,
        );
        renderer.record_scene(gpu, cmd, target.view, extent, clear, quads);
        image_barrier(
            gpu,
            cmd,
            target.image,
            (
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            ),
            (vk::PipelineStageFlags2::COPY, vk::AccessFlags2::TRANSFER_READ, vk::ImageLayout::TRANSFER_SRC_OPTIMAL),
            None,
        );
        let region = [vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1),
            )
            .image_extent(vk::Extent3D { width, height, depth: 1 })];
        unsafe {
            gpu.device.cmd_copy_image_to_buffer(
                cmd,
                target.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buf.buffer,
                &region,
            )
        };
        for t in &external {
            gpu.record_release(cmd, t);
        }
    });
    let out = r.map(|_| unsafe { std::slice::from_raw_parts(buf.ptr, size as usize) }.to_vec());
    buf.destroy(gpu);
    target.destroy(gpu);
    out
}
