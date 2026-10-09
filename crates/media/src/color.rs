//! BGRA to limited-range BT.601 NV12.
//!
//! This is the CPU cost that a later D3D11 video-processor path should remove.
//! It only ever runs at the stream size, never at the desktop size.

/// Convert tightly packed BGRA into NV12. `dst` must be `width * height * 3 / 2` bytes.
/// Width and height must be even.
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize, dst: &mut [u8]) {
    assert!(width % 2 == 0 && height % 2 == 0);
    let y_plane = width * height;
    assert!(bgra.len() >= y_plane * 4);
    assert!(dst.len() >= y_plane + y_plane / 2);

    // One pass over each 2x2 block. The old two-pass form converted every pixel twice.
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let mut u = 0i32;
            let mut v = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let px = ((y + dy) * width + (x + dx)) * 4;
                    let (yy, uu, vv) = yuv(bgra[px], bgra[px + 1], bgra[px + 2]);
                    dst[(y + dy) * width + (x + dx)] = yy;
                    u += i32::from(uu);
                    v += i32::from(vv);
                }
            }
            let uv_index = y_plane + (y / 2) * width + x;
            dst[uv_index] = (u / 4) as u8;
            dst[uv_index + 1] = (v / 4) as u8;
        }
    }
}

fn yuv(b: u8, g: u8, r: u8) -> (u8, u8, u8) {
    let b = i32::from(b);
    let g = i32::from(g);
    let r = i32::from(r);
    let y = clamp_u8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
    let u = clamp_u8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128);
    let v = clamp_u8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128);
    (y, u, v)
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limited_range_black_and_white() {
        let mut black = [0u8; 8];
        bgra_to_nv12(&[0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255], 2, 2, &mut black);
        assert_eq!(black[0], 16);
        assert_eq!(&black[4..6], &[128, 128]);

        let white = [255, 255, 255, 255];
        let bgra = white.repeat(4);
        let mut dst = [0u8; 8];
        bgra_to_nv12(&bgra, 2, 2, &mut dst);
        assert_eq!(dst[0], 235);
        assert_eq!(&dst[4..6], &[128, 128]);
    }
}
