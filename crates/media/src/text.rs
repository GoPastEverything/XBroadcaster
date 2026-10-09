//! Rasterize a text source with GDI once, then reuse the bitmap.
//!
//! Drawing text every frame is the expensive way. The pipeline caches the
//! bitmap until the string, size, or color changes.

use std::mem::size_of;

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DT_CALCRECT, DT_LEFT,
    DT_NOPREFIX, DT_WORDBREAK, DIB_RGB_COLORS, DeleteDC, DeleteObject, DrawTextW, HGDIOBJ,
    OUT_DEFAULT_PRECIS, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;
use windows::core::PCWSTR;

use crate::MediaError;

pub struct TextBitmap {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub key: TextKey,
}

#[derive(Clone, PartialEq, Eq)]
pub struct TextKey {
    pub text: String,
    pub px: u32,
    pub color: [u8; 4],
}

pub fn rasterize(text: &str, px: u32, color: [u8; 4]) -> Result<TextBitmap, MediaError> {
    if text.trim().is_empty() {
        return Ok(TextBitmap {
            width: 1,
            height: 1,
            bgra: vec![0, 0, 0, 0],
            key: TextKey {
                text: text.to_string(),
                px,
                color,
            },
        });
    }
    unsafe {
        let screen = GetDesktopWindow();
        let _ = screen;
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return Err(MediaError::message("CreateCompatibleDC failed"));
        }
        let face = wide("Segoe UI");
        let font = CreateFontW(
            -(px.max(8) as i32),
            0,
            0,
            0,
            600,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            PCWSTR(face.as_ptr()),
        );
        let old = SelectObject(dc, HGDIOBJ(font.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(
            dc,
            COLORREF((u32::from(color[2]) << 16) | (u32::from(color[1]) << 8) | u32::from(color[0])),
        );

        let mut wide_text = wide(text);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 960,
            bottom: 0,
        };
        DrawTextW(dc, &mut wide_text, &mut rect, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX | DT_LEFT);
        let width = (rect.right - rect.left).clamp(1, 1920) as u32 + 8;
        let height = (rect.bottom - rect.top).clamp(1, 1080) as u32 + 8;

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0 as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let old_bmp = SelectObject(dc, HGDIOBJ(dib.0));
        let mut draw_rect = RECT {
            left: 4,
            top: 4,
            right: width as i32 - 4,
            bottom: height as i32 - 4,
        };
        DrawTextW(
            dc,
            &mut wide_text,
            &mut draw_rect,
            DT_WORDBREAK | DT_NOPREFIX | DT_LEFT,
        );

        let byte_len = (width as usize) * (height as usize) * 4;
        let mut bgra = vec![0u8; byte_len];
        std::ptr::copy_nonoverlapping(bits as *const u8, bgra.as_mut_ptr(), byte_len);
        for pixel in bgra.chunks_exact_mut(4) {
            if pixel[0] | pixel[1] | pixel[2] != 0 {
                pixel[3] = 255;
            }
        }

        SelectObject(dc, old_bmp);
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = DeleteDC(dc);
        let _ = info;
        Ok(TextBitmap {
            width,
            height,
            bgra,
            key: TextKey {
                text: text.to_string(),
                px,
                color,
            },
        })
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
