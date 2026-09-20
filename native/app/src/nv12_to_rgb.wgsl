struct Uniforms {
    content_scale: vec4<f32>,
    range: vec4<f32>,
    matrix: vec4<f32>,
    mode: vec4<f32>,
}

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> uni: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) viewport: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var out: VsOut;
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.viewport = vec2<f32>(x, y);
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let uv = (in.viewport - uni.content_scale.zw) * uni.content_scale.xy;
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    let raw_y = textureSample(y_tex, samp, uv).r;
    let raw_uv = textureSample(uv_tex, samp, uv).rg;
    let raw_u = raw_uv.r;
    let raw_v = raw_uv.g;
    let y = (raw_y - uni.range.y) * uni.range.x;
    let cb = (raw_u - uni.range.w) * uni.range.z;
    let cr = (raw_v - uni.range.w) * uni.range.z;
    let mode = uni.mode.x;

    var rgb: vec3<f32>;
    if (mode < 0.5) {
        rgb = vec3<f32>(
            y + uni.matrix.x * cr,
            y + uni.matrix.y * cb + uni.matrix.z * cr,
            y + uni.matrix.w * cb,
        );
    } else if (mode < 1.5) {
        rgb = vec3<f32>(y - cb + cr, y + cb, y - cb - cr);
    } else if (mode < 2.5) {
        let r = y + select(1.7184, 0.9936, cr >= 0.0) * cr;
        let b = y + select(1.9404, 1.5816, cb >= 0.0) * cb;
        let g = (y - 0.2627 * r - 0.0593 * b) / 0.6780;
        rgb = vec3<f32>(r, g, b);
    } else {
        let component_scale = uni.range.x;
        let component_offset = uni.range.y;
        rgb = vec3<f32>(
            (raw_v - component_offset) * component_scale,
            y,
            (raw_u - component_offset) * component_scale,
        );
    }
    return vec4<f32>(rgb, 1.0);
}
