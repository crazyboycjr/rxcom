#version 450

layout(push_constant) uniform Push {
    vec2 target_size;
    vec2 dst_pos;
    vec2 dst_size;
    vec2 uv_scale;
    vec2 uv_offset;
    float opacity;
} pc;

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 color;

void main() {
    // Window contents are premultiplied, so opacity scales all four channels.
    color = texture(tex, v_uv) * pc.opacity;
}
