#version 450

// picom-zoom's mapping: screen pixel s shows the scene at crop + (s - origin) / scale,
// both in pixel-centre coordinates, so gl_FragCoord can be used as it is.
layout(push_constant) uniform View {
    vec2 crop;
    vec2 origin;
    float inv_scale;
    uint linear;
} v;

layout(binding = 0) uniform sampler2D scene;
layout(location = 0) out vec4 color;

void main() {
    vec2 q = v.crop + (gl_FragCoord.xy - v.origin) * v.inv_scale;
    ivec2 size = textureSize(scene, 0);
    vec3 c = v.linear != 0u ? textureLod(scene, q / vec2(size), 0.0).rgb
                            : texelFetch(scene, clamp(ivec2(floor(q)), ivec2(0), size - 1), 0).rgb;
    color = vec4(c, 1.0);
}
