#version 450

layout(push_constant) uniform Push {
    vec2 scene_size;
    vec2 origin;
    float zoom;
} pc;

layout(set = 0, binding = 0) uniform sampler2D scene;

layout(location = 0) out vec4 color;

void main() {
    // Screen pixel s shows desktop point q = s / z + o, the inverse of s = (q - o) * z.
    vec2 q = gl_FragCoord.xy / pc.zoom + pc.origin;
    color = texture(scene, q / pc.scene_size);
}
