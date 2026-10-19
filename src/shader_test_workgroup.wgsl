var<workgroup> first: array<u32, 3>;
var<workgroup> unused: array<u32, 20>;
var<workgroup> second: array<u32, 9>;

@compute @workgroup_size(1)
fn both() {
    first[0] = 1u;
    second[0] = 2u;
}

@compute @workgroup_size(1)
fn second_only() {
    second[0] = 3u;
}
