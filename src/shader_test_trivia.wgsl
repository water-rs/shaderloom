// Source trivia must not alter tokens, including Unicode identifiers.
@group(0) @binding(0) var<storage, read_write> result: array<i32>;

@compute @workgroup_size(1)
fn trivia() {
    let α = 7;
    let β = 3;
    /* outer comment /* nested comment */ still outer */
    let value = α - /* keep the two minus tokens separate */ -β;
    result[0] = value;
}
