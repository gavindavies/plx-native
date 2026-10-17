// THE VIDEO PICTURE — a decoder's Y'CbCr planes converted to RGB here, so no CPU pass converts or
// scales them (`gfx::video`). The vertex half is `vs_src.vert`: `v_uv` is the unit quad.
//
// Each plane's texture is as wide as the plane's STRIDE (GLES2's calls have no row length), so its
// coordinate is the quad's scaled into the picture's own columns and clamped half a texel inside
// the last one: the padding beyond the picture never reaches GL_LINEAR. `u_*crop` = (scale.xy,
// max.xy).
//
// PLX_NV12: the chroma is one interleaved Cb/Cr plane uploaded as a two-channel texture (Cb in .r,
// Cr in .g). Otherwise (I420) Cb and Cr are two one-channel planes.
//
// `u_r`/`u_g`/`u_b` are the matrix rows `gfx::video::YuvMatrix` builds — range expansion and the
// stream's coefficients folded into one dot product per channel.
//
// PRECISION: coordinates are highp (a 3840-texel plane is beyond fp16's integer step), the colour
// mediump, as in `fs_src.frag`.
precision mediump float;
varying highp vec2 v_uv;
uniform sampler2D u_y;
uniform sampler2D u_cb;
uniform highp vec4 u_ycrop;
uniform highp vec4 u_ccrop;
uniform vec4 u_r;
uniform vec4 u_g;
uniform vec4 u_b;
#ifndef PLX_NV12
uniform sampler2D u_cr;
#endif
void main(){
  highp vec2 yc = min(v_uv * u_ycrop.xy, u_ycrop.zw);
  highp vec2 cc = min(v_uv * u_ccrop.xy, u_ccrop.zw);
#ifdef PLX_NV12
  vec2 c = texture2D(u_cb, cc).rg;
#else
  vec2 c = vec2(texture2D(u_cb, cc).r, texture2D(u_cr, cc).r);
#endif
  vec4 yuv = vec4(texture2D(u_y, yc).r, c, 1.0);
  gl_FragColor = vec4(dot(u_r, yuv), dot(u_g, yuv), dot(u_b, yuv), 1.0);
}
