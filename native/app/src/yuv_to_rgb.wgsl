struct Uniforms {
    content_scale: vec4<f32>,
}

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var u_tex: texture_2d<f32>;
@group(0) @binding(2) var v_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> uni: Uniforms;

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
    let y = textureSample(y_tex, samp, uv).r;
    let u = textureSample(u_tex, samp, uv).r - 0.5;
    let v = textureSample(v_tex, samp, uv).r - 0.5;
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;
    return vec4<f32>(r, g, b, 1.0);
}
