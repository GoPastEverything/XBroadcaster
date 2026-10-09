use crate::{Scene, SourceKind};

/// Tightly packed BGRA canvas at the stream resolution.
#[derive(Clone, Debug)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        let pixels = (width as usize).saturating_mul(height as usize);
        Self {
            width,
            height,
            bgra: vec![0; pixels.saturating_mul(4)],
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if self.width == width && self.height == height {
            return;
        }
        *self = Self::new(width, height);
    }

    pub fn clear(&mut self, color: [u8; 4]) {
        for pixel in self.bgra.chunks_exact_mut(4) {
            pixel.copy_from_slice(&color);
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: [u8; 4], opacity: f32) {
        if w == 0 || h == 0 || opacity <= 0.0 {
            return;
        }
        let (x0, y0, x1, y1) = clip_rect(x, y, w, h, self.width, self.height);
        if color[3] == 255 && opacity >= 0.999 {
            for row in y0..y1 {
                let start = (row * self.width as usize + x0) * 4;
                let end = (row * self.width as usize + x1) * 4;
                for pixel in self.bgra[start..end].chunks_exact_mut(4) {
                    pixel.copy_from_slice(&color);
                }
            }
            return;
        }
        let alpha = alpha_u8(color[3], opacity);
        for row in y0..y1 {
            for col in x0..x1 {
                let i = (row * self.width as usize + col) * 4;
                blend_pixel(&mut self.bgra[i..i + 4], color, alpha);
            }
        }
    }

    pub fn blit(
        &mut self,
        src: &[u8],
        src_w: u32,
        src_h: u32,
        src_stride: usize,
        dx: i32,
        dy: i32,
        dw: u32,
        dh: u32,
        opacity: f32,
    ) {
        if src_w == 0 || src_h == 0 || dw == 0 || dh == 0 || opacity <= 0.0 {
            return;
        }
        if opacity >= 0.999
            && src_w == dw
            && src_h == dh
            && src_stride == src_w as usize * 4
            && dx == 0
            && dy == 0
            && dw == self.width
            && dh == self.height
            && src.len() >= self.bgra.len()
        {
            let len = self.bgra.len();
            self.bgra.copy_from_slice(&src[..len]);
            return;
        }

        let (x0, y0, x1, y1) = clip_rect(dx, dy, dw, dh, self.width, self.height);
        for row in y0..y1 {
            let src_y = ((row as i64 - dy as i64) * src_h as i64 / dh as i64)
                .clamp(0, src_h as i64 - 1) as usize;
            let src_row = src_y * src_stride;
            for col in x0..x1 {
                let src_x = ((col as i64 - dx as i64) * src_w as i64 / dw as i64)
                    .clamp(0, src_w as i64 - 1) as usize;
                let s = src_row + src_x * 4;
                if s + 4 > src.len() {
                    continue;
                }
                let mut pixel = [src[s], src[s + 1], src[s + 2], src[s + 3]];
                if pixel[3] == 0 && (pixel[0] | pixel[1] | pixel[2]) != 0 {
                    pixel[3] = 255;
                }
                let i = (row * self.width as usize + col) * 4;
                if pixel[3] == 255 && opacity >= 0.999 {
                    self.bgra[i..i + 4].copy_from_slice(&pixel);
                } else {
                    blend_pixel(&mut self.bgra[i..i + 4], pixel, alpha_u8(pixel[3], opacity));
                }
            }
        }
    }
}

/// One input image the compositor can sample. Display frames are already at
/// stream resolution; text bitmaps are their natural size.
pub struct FrameView<'a> {
    pub source_id: u64,
    pub bgra: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

pub fn composite(canvas: &mut Canvas, scene: &Scene, frames: &[FrameView<'_>]) {
    canvas.clear([0, 0, 0, 255]);
    for source in &scene.sources {
        if !source.visible || source.opacity <= 0.0 {
            continue;
        }
        match &source.kind {
            SourceKind::Color { color } => {
                canvas.fill_rect(source.x, source.y, source.w, source.h, *color, source.opacity);
            }
            SourceKind::Display { .. } | SourceKind::Text { .. } => {
                if let Some(frame) = frames.iter().find(|frame| frame.source_id == source.id) {
                    canvas.blit(
                        frame.bgra,
                        frame.width,
                        frame.height,
                        frame.stride,
                        source.x,
                        source.y,
                        source.w,
                        source.h,
                        source.opacity,
                    );
                }
            }
        }
    }
}

fn clip_rect(x: i32, y: i32, w: u32, h: u32, bounds_w: u32, bounds_h: u32) -> (usize, usize, usize, usize) {
    let x1 = x.saturating_add(w as i32).max(0) as usize;
    let y1 = y.saturating_add(h as i32).max(0) as usize;
    let x0 = x.max(0) as usize;
    let y0 = y.max(0) as usize;
    (
        x0.min(bounds_w as usize),
        y0.min(bounds_h as usize),
        x1.min(bounds_w as usize),
        y1.min(bounds_h as usize),
    )
}

fn alpha_u8(color_alpha: u8, opacity: f32) -> u8 {
    let opacity = opacity.clamp(0.0, 1.0);
    ((u16::from(color_alpha) as f32) * opacity).round() as u8
}

fn blend_pixel(dst: &mut [u8], src: [u8; 4], alpha: u8) {
    if alpha == 0 {
        return;
    }
    if alpha == 255 {
        dst.copy_from_slice(&src);
        return;
    }
    let inv = 255 - u16::from(alpha);
    for channel in 0..3 {
        let mixed = u16::from(src[channel]) * u16::from(alpha) + u16::from(dst[channel]) * inv;
        dst[channel] = (mixed / 255) as u8;
    }
    dst[3] = 255;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Scene, Source, SourceKind};

    #[test]
    fn opaque_color_covers_a_rect() {
        let mut canvas = Canvas::new(4, 2);
        canvas.clear([0, 0, 0, 255]);
        canvas.fill_rect(1, 0, 2, 2, [10, 20, 30, 255], 1.0);
        assert_eq!(&canvas.bgra[0..4], &[0, 0, 0, 255]);
        assert_eq!(&canvas.bgra[4..8], &[10, 20, 30, 255]);
        assert_eq!(&canvas.bgra[8..12], &[10, 20, 30, 255]);
        assert_eq!(&canvas.bgra[12..16], &[0, 0, 0, 255]);
        assert_eq!(&canvas.bgra[20..24], &[10, 20, 30, 255]);
    }

    #[test]
    fn full_frame_blit_is_a_copy() {
        let mut canvas = Canvas::new(2, 1);
        let src = [1, 2, 3, 255, 4, 5, 6, 255];
        canvas.blit(&src, 2, 1, 8, 0, 0, 2, 1, 1.0);
        assert_eq!(canvas.bgra, src);
    }

    #[test]
    fn scene_stacks_color_under_a_frame() {
        let scene = Scene {
            id: 1,
            name: "test".into(),
            sources: vec![
                Source {
                    id: 1,
                    name: "bg".into(),
                    kind: SourceKind::Color {
                        color: [1, 2, 3, 255],
                    },
                    visible: true,
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 1,
                    opacity: 1.0,
                },
                Source {
                    id: 2,
                    name: "fg".into(),
                    kind: SourceKind::Display { monitor: 0 },
                    visible: true,
                    x: 1,
                    y: 0,
                    w: 1,
                    h: 1,
                    opacity: 1.0,
                },
            ],
        };
        let frame = [9, 9, 9, 255];
        let mut canvas = Canvas::new(2, 1);
        composite(
            &mut canvas,
            &scene,
            &[FrameView {
                source_id: 2,
                bgra: &frame,
                width: 1,
                height: 1,
                stride: 4,
            }],
        );
        assert_eq!(&canvas.bgra[0..4], &[1, 2, 3, 255]);
        assert_eq!(&canvas.bgra[4..8], &[9, 9, 9, 255]);
    }
}
