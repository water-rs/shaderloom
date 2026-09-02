// Shaderloom fixture whose binding numbers are deliberately not contiguous, so
// the Vulkan descriptor binding wgpu allocates differs from the WGSL binding.
@group(0) @binding(0) var<uniform> scale: vec4<f32>;
@group(0) @binding(3) var<uniform> offset: vec4<f32>;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let x = f32(i32(index) - 1);
    return vec4<f32>(x, 0.0, 0.0, 1.0) * scale + offset;
}
