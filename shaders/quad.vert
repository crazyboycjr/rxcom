#version 450

// One textured rectangle in target pixels; the quad comes from gl_VertexIndex
// drawn as a 4-vertex triangle strip.
layout(push_constant) uniform Push {
    vec2 target_size;
    vec2 dst_pos;
    vec2 dst_size;
    vec2 uv_scale;
    vec2 uv_offset;
    float opacity;
} pc;

layout(location = 0) out vec2 v_uv;

void main() {
    vec2 corner = vec2(float(gl_VertexIndex & 1), float(gl_VertexIndex >> 1));
    vec2 p = corner * pc.dst_size;
    v_uv = p * pc.uv_scale + pc.uv_offset;
    gl_Position = vec4((pc.dst_pos + p) / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
}
