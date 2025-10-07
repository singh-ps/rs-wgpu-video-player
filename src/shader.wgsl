struct VsOut {
  @builtin(position) pos : vec4<f32>,
  @location(0) uv : vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vidx : u32) -> VsOut {
  // full-screen triangle
  var pos = array<vec2<f32>, 3>(
      vec2<f32>(-1.0, -1.0),
      vec2<f32>( 3.0, -1.0),
      vec2<f32>(-1.0,  3.0)
  );
  let p = pos[vidx];
  var out : VsOut;
  out.pos = vec4<f32>(p, 0.0, 1.0);
  out.uv = 0.5 * (p + vec2<f32>(1.0, 1.0));
  return out;
}

@group(0) @binding(0) var tex0 : texture_2d<f32>;
@group(0) @binding(1) var samp : sampler;

@fragment
fn fs_main(@location(0) uv_in : vec2<f32>) -> @location(0) vec4<f32> {
  return textureSample(tex0, samp, uv_in);
}
