@group(0) @binding(0)
var<uniform> transform: mat4x4<f32>;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) v_uv: vec2<f32>,
    @location(1) v_color: vec4<f32>,
};

@group(1) @binding(0) var glyph_tex: texture_2d<f32>;
@group(1) @binding(1) var glyph_sampler: sampler;

// Per-TextDisplay fade. Only .x is read (the alpha multiplier, 1.0 = opaque);
// it is a vec4 because uniforms must be 16-byte aligned.
@group(1) @binding(2) var<uniform> fade: vec4<f32>;

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_pos = transform * vec4<f32>(in.position, 0.0, 1.0);
    out.v_uv = in.uv;
    out.v_color = in.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(glyph_tex, glyph_sampler, in.v_uv);
    var color = in.v_color * sample;
    // Straight (non-premultiplied) alpha: the blend state scales rgb by src alpha,
    // so scaling only .a here fades the glyph without darkening it.
    color.a = color.a * fade.x;
    return color;
}