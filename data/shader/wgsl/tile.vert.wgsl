struct In {
	@location(0) pos: vec2<u32>,
#ifdef TW_TEXTURED
	@location(1) tex3d: vec2<u32>,
#endif
}

struct Out {
    @builtin(position) position: vec4<f32>,
#ifdef TW_TEXTURED
	@location(0) @interpolate(linear) tex: vec3<f32>,
#endif
}

struct PosBO {
	pos: mat4x2<f32>,
}
var<push_constant> g_pos: PosBO;

@vertex
fn main(in: In) -> Out {
	var out = Out();
	out.position = vec4(g_pos.pos * vec4(vec2<f32>(f32(in.pos.x), f32(in.pos.y)), 0.0, 1.0), 0.0, 1.0);

#ifdef TW_TEXTURED
	var x = (in.tex3d.x >> 0) & 1;
	var y = (in.tex3d.x >> 1) & 1;
	out.tex = vec3<f32>(f32(x), f32(y), f32(in.tex3d.y));
#endif
	return out;
}
