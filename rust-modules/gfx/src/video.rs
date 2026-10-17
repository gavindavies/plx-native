//! **The video picture on the GPU**: a decoder's Y'CbCr planes uploaded as they come and converted
//! to RGB in `shaders/fs_video.frag`, instead of a CPU `videoconvert ! videoscale` to RGBA ahead of
//! the upload. On the dual-core laptop PiTV runs on, that conversion was most of the player's CPU;
//! here it is one textured quad, and a 4K picture is scaled by the same draw.
//!
//! Two layouts, the ones decoders hand over in system memory: NV12 (VA-API, and the hardware
//! decoders generally) and I420 (FFmpeg's software decoders). Each plane's texture is as wide as
//! the plane's stride, because the renderer keeps to GLES2's calls, which cannot unpack a row
//! shorter than the data; the shader samples only the picture's own columns.
//!
//! Main-thread only, like all GL here.
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint, c_void};

use plx_base::surface::{LOGICAL_H as SCR_H, LOGICAL_W as SCR_W};

const GL_TEXTURE_2D: c_uint = 0x0DE1;
const GL_TEXTURE0: c_uint = 0x84C0;
const GL_TEXTURE1: c_uint = 0x84C1;
/// Unit 3: nothing else in the renderer samples from it (0 is every program's own texture, 1 the
/// glass's sharp source, 2 the dither tile, bound for the life of the process).
const GL_TEXTURE3: c_uint = 0x84C3;
// One- and two-channel textures. The context is desktop GL core (`app::boot`), where the GLES2
// LUMINANCE formats do not exist; these sample as `.r` and `.rg`.
const GL_RED: c_uint = 0x1903;
const GL_RG: c_uint = 0x8227;
const GL_R8: c_int = 0x8229;
const GL_RG8: c_int = 0x822B;
const GL_UNSIGNED_BYTE: c_uint = 0x1401;
const GL_UNPACK_ALIGNMENT: c_uint = 0x0CF5;
const GL_TEXTURE_MIN_FILTER: c_uint = 0x2801;
const GL_TEXTURE_MAG_FILTER: c_uint = 0x2800;
const GL_TEXTURE_WRAP_S: c_uint = 0x2802;
const GL_TEXTURE_WRAP_T: c_uint = 0x2803;
const GL_LINEAR: c_int = 0x2601;
const GL_CLAMP_TO_EDGE: c_int = 0x812F;
const GL_TRIANGLE_STRIP: c_uint = 0x0005;

extern "C" {
    fn glGenTextures(n: c_int, textures: *mut c_uint);
    fn glBindTexture(target: c_uint, texture: c_uint);
    fn glActiveTexture(texture: c_uint);
    fn glPixelStorei(pname: c_uint, param: c_int);
    fn glTexImage2D(target: c_uint, level: c_int, ifmt: c_int, w: c_int, h: c_int, border: c_int, format: c_uint, ty: c_uint, pixels: *const c_void);
    fn glTexSubImage2D(target: c_uint, level: c_int, x: c_int, y: c_int, w: c_int, h: c_int, format: c_uint, ty: c_uint, pixels: *const c_void);
    fn glTexParameteri(target: c_uint, pname: c_uint, param: c_int);
    fn glGetUniformLocation(program: c_uint, name: *const c_char) -> c_int;
    fn glUniform1i(loc: c_int, x: c_int);
    fn glUniform2f(loc: c_int, x: f32, y: f32);
    fn glUniform4f(loc: c_int, x: f32, y: f32, z: f32, w: f32);
    fn glUniform4fv(loc: c_int, count: c_int, value: *const f32);
    fn glDrawArrays(mode: c_uint, first: c_int, count: c_int);
}

const VS_VIDEO: &CStr = crate::gfx::glsl!("shaders/vs_src.vert");
const FS_VIDEO_NV12: &CStr = crate::gfx::glsl!("shaders/fs_video.frag", "#define PLX_NV12\n");
const FS_VIDEO_I420: &CStr = crate::gfx::glsl!("shaders/fs_video.frag");

/// How the chroma is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chroma {
    /// One plane of interleaved Cb/Cr pairs.
    Nv12,
    /// Separate Cb and Cr planes.
    I420,
}

/// The Y'CbCr → R'G'B' coefficients a stream declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// One plane of a picture: `w`×`h` samples, rows `stride` bytes apart.
#[derive(Clone, Copy, Debug)]
pub struct Plane {
    pub data: *const u8,
    pub stride: i32,
    pub w: i32,
    pub h: i32,
}

/// The rows `fs_video.frag` dots `(Y', Cb, Cr, 1)` with — samples normalised to 0..1 — to get R', G'
/// and B': the range expansion (limited range is Y' 16-235, chroma 16-240, in 8-bit terms) and the
/// matrix in one affine map.
pub fn yuv_rows(matrix: Matrix, full_range: bool) -> [[f32; 4]; 3] {
    let (kr, kb) = match matrix {
        Matrix::Bt601 => (0.299, 0.114),
        Matrix::Bt709 => (0.2126, 0.0722),
        Matrix::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;
    let (ys, yo, cs) = if full_range { (1.0, 0.0, 1.0) } else { (255.0 / 219.0, -16.0 / 219.0, 255.0 / 224.0) };
    let mid = 128.0 / 255.0;
    // R' = Y' + 2(1-kr) Cr,  B' = Y' + 2(1-kb) Cb,  G' = Y' - gb Cb - gr Cr; with Cb, Cr centred.
    let (rr, bb) = (2.0 * (1.0 - kr) * cs, 2.0 * (1.0 - kb) * cs);
    let (gb, gr) = (2.0 * kb * (1.0 - kb) / kg * cs, 2.0 * kr * (1.0 - kr) / kg * cs);
    [
        [ys, 0.0, rr, yo - rr * mid],
        [ys, -gb, -gr, yo + (gb + gr) * mid],
        [ys, bb, 0.0, yo - bb * mid],
    ]
}

struct Program {
    id: c_uint,
    rect: c_int,
    ycrop: c_int,
    ccrop: c_int,
    rows: [c_int; 3],
}

fn link(chroma: Chroma) -> Option<Program> {
    let fs = match chroma {
        Chroma::Nv12 => FS_VIDEO_NV12,
        Chroma::I420 => FS_VIDEO_I420,
    };
    let id = crate::gfx::link_program(VS_VIDEO.as_ptr(), fs.as_ptr())?;
    let loc = |name: &CStr| unsafe { glGetUniformLocation(id, name.as_ptr()) };
    crate::gfx::use_prog(id);
    unsafe {
        glUniform2f(loc(c"u_screen"), SCR_W, SCR_H);
        glUniform1i(loc(c"u_y"), 0);
        glUniform1i(loc(c"u_cb"), 1);
        glUniform1i(loc(c"u_cr"), 3);
    }
    Some(Program {
        id,
        rect: loc(c"u_rect"),
        ycrop: loc(c"u_ycrop"),
        ccrop: loc(c"u_ccrop"),
        rows: [loc(c"u_r"), loc(c"u_g"), loc(c"u_b")],
    })
}

thread_local! {
    /// The two programs, linked on first use: `None` = not yet tried, `Some(None)` = failed.
    static PROGRAMS: std::cell::RefCell<[Option<Option<Program>>; 2]> = const { std::cell::RefCell::new([None, None]) };
}

/// One plane's texture: its name, the texel size it was specified at, and the picture's own
/// columns and rows within it.
#[derive(Clone, Copy, Default)]
struct PlaneTex {
    tex: c_uint,
    tex_w: i32,
    tex_h: i32,
    w: i32,
    h: i32,
}

impl PlaneTex {
    /// `(scale.xy, max.xy)` for the shader: the quad's coordinate mapped into the picture's own
    /// columns, clamped half a texel inside the last.
    fn crop(&self) -> [f32; 4] {
        let (tw, th) = (self.tex_w.max(1) as f32, self.tex_h.max(1) as f32);
        [self.w as f32 / tw, self.h as f32 / th, (self.w as f32 - 0.5) / tw, (self.h as f32 - 0.5) / th]
    }
}

/// The textures one picture is uploaded into, reused from picture to picture.
pub struct VideoTex {
    chroma: Chroma,
    planes: [PlaneTex; 3],
}

impl Default for VideoTex {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoTex {
    pub const fn new() -> Self {
        const NONE: PlaneTex = PlaneTex { tex: 0, tex_w: 0, tex_h: 0, w: 0, h: 0 };
        Self { chroma: Chroma::I420, planes: [NONE; 3] }
    }

    /// Upload a picture: `planes` is Y then Cb/Cr (NV12, two planes) or Y, Cb, Cr (I420). A plane
    /// of the size it had last time is overwritten in place; otherwise its texture is re-specified.
    pub fn upload(&mut self, chroma: Chroma, planes: &[Plane]) {
        self.chroma = chroma;
        let count = match chroma {
            Chroma::Nv12 => 2,
            Chroma::I420 => 3,
        };
        for (i, p) in planes.iter().take(count).enumerate() {
            // NV12's chroma is (Cb, Cr) byte pairs: one two-channel texel each.
            let (internal, format, bpp) = if chroma == Chroma::Nv12 && i == 1 { (GL_RG8, GL_RG, 2) } else { (GL_R8, GL_RED, 1) };
            let t = &mut self.planes[i];
            let tex_w = p.stride / bpp;
            unsafe {
                if t.tex == 0 {
                    glGenTextures(1, &mut t.tex);
                }
                glBindTexture(GL_TEXTURE_2D, t.tex);
                glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
                if (t.tex_w, t.tex_h) == (tex_w, p.h) {
                    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, tex_w, p.h, format, GL_UNSIGNED_BYTE, p.data as *const c_void);
                } else {
                    glTexImage2D(GL_TEXTURE_2D, 0, internal, tex_w, p.h, 0, format, GL_UNSIGNED_BYTE, p.data as *const c_void);
                    for (pname, v) in [
                        (GL_TEXTURE_MIN_FILTER, GL_LINEAR),
                        (GL_TEXTURE_MAG_FILTER, GL_LINEAR),
                        (GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE),
                        (GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE),
                    ] {
                        glTexParameteri(GL_TEXTURE_2D, pname, v);
                    }
                    crate::gfx::tex_ledger::specified_bytes(t.tex, (p.stride.max(0) as u64) * (p.h.max(0) as u64));
                }
                glPixelStorei(GL_UNPACK_ALIGNMENT, 4);
            }
            *t = PlaneTex { tex: t.tex, tex_w, tex_h: p.h, w: p.w, h: p.h };
        }
    }

    /// Whether a picture has been uploaded.
    pub fn ready(&self) -> bool {
        self.planes[0].tex != 0
    }

    /// Draw the uploaded picture into `(x, y, w, h)`, converted with `rows` ([`yuv_rows`]), UNDER
    /// everything drawn so far this frame ([`crate::gfx::draw_under`]).
    pub fn draw_under(&self, rows: &[[f32; 4]; 3], x: f32, y: f32, w: f32, h: f32) {
        if !self.ready() {
            return;
        }
        let slot = match self.chroma {
            Chroma::Nv12 => 0,
            Chroma::I420 => 1,
        };
        PROGRAMS.with(|programs| {
            let mut programs = programs.borrow_mut();
            let program = programs[slot].get_or_insert_with(|| {
                let p = link(self.chroma);
                if p.is_none() {
                    plx_base::eventlog::log(&format!("video prog ({:?}) link failed — no picture", self.chroma));
                }
                p
            });
            let Some(p) = program.as_ref() else { return };
            crate::gfx::draw_under(|| unsafe {
                crate::gfx::use_prog(p.id);
                glUniform4f(p.rect, x, y, w, h);
                let [yc, cc] = [self.planes[0].crop(), self.planes[1].crop()];
                glUniform4fv(p.ycrop, 1, yc.as_ptr());
                glUniform4fv(p.ccrop, 1, cc.as_ptr());
                for (loc, row) in p.rows.iter().zip(rows) {
                    glUniform4fv(*loc, 1, row.as_ptr());
                }
                glActiveTexture(GL_TEXTURE1);
                glBindTexture(GL_TEXTURE_2D, self.planes[1].tex);
                if self.chroma == Chroma::I420 {
                    glActiveTexture(GL_TEXTURE3);
                    glBindTexture(GL_TEXTURE_2D, self.planes[2].tex);
                }
                glActiveTexture(GL_TEXTURE0);
                glBindTexture(GL_TEXTURE_2D, self.planes[0].tex);
                glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(rows: &[[f32; 4]; 3], y: f32, cb: f32, cr: f32) -> [f32; 3] {
        rows.map(|r| r[0] * y + r[1] * cb + r[2] * cr + r[3])
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 2e-3)
    }

    #[test]
    fn limited_range_black_and_white_land_on_zero_and_one() {
        for m in [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020] {
            let rows = yuv_rows(m, false);
            let mid = 128.0 / 255.0;
            assert!(close(apply(&rows, 16.0 / 255.0, mid, mid), [0.0; 3]), "{m:?} black");
            assert!(close(apply(&rows, 235.0 / 255.0, mid, mid), [1.0; 3]), "{m:?} white");
        }
    }

    #[test]
    fn full_range_grey_is_grey() {
        let rows = yuv_rows(Matrix::Bt709, true);
        let mid = 128.0 / 255.0;
        assert!(close(apply(&rows, 0.5, mid, mid), [0.5; 3]));
    }

    /// BT.709 limited-range 75% red bars (ITU-R BT.2111's 8-bit values Y'=51, Cb=109, Cr=212)
    /// convert to (0.75, 0, 0).
    #[test]
    fn bt709_red_converts_to_red() {
        let rows = yuv_rows(Matrix::Bt709, false);
        let rgb = apply(&rows, 51.0 / 255.0, 109.0 / 255.0, 212.0 / 255.0);
        assert!(rgb.iter().zip([0.75, 0.0, 0.0]).all(|(a, b)| (a - b).abs() < 0.01), "{rgb:?}");
    }

    #[test]
    fn a_padded_plane_samples_only_its_own_columns() {
        let p = PlaneTex { tex: 1, tex_w: 1984, tex_h: 1080, w: 1920, h: 1080 };
        let [sx, sy, mx, my] = p.crop();
        assert!((sx - 1920.0 / 1984.0).abs() < 1e-6 && sy == 1.0);
        assert!((mx - 1919.5 / 1984.0).abs() < 1e-6 && (my - 1079.5 / 1080.0).abs() < 1e-6);
    }
}
