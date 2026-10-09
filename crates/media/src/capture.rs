//! DXGI Desktop Duplication, scaled on the GPU to the stream size.
//!
//! The desktop texture stays on the GPU. One bilinear draw writes the fitted
//! frame into a stream-sized target, and only that small texture is read back.
//! A 4K desktop is never copied to system memory.

use std::mem::size_of;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, s};

use crate::MediaError;

const SCALE_HLSL: &str = r#"
Texture2D srcTex : register(t0);
SamplerState srcSamp : register(s0);
struct VsOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};
VsOut main_vs(uint id : SV_VertexID) {
    float2 xy = float2(id == 2 ? 3.0 : -1.0, id == 1 ? 3.0 : -1.0);
    VsOut o;
    o.pos = float4(xy, 0.0, 1.0);
    o.uv = float2((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return o;
}
float4 main_ps(VsOut i) : SV_Target {
    return srcTex.Sample(srcSamp, i.uv);
}
"#;

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

pub struct DisplayCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    desktop_w: u32,
    desktop_h: u32,
    out_w: u32,
    out_h: u32,
    target: ID3D11Texture2D,
    target_view: ID3D11RenderTargetView,
    staging: ID3D11Texture2D,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    packed: Vec<u8>,
    adapter_index: u32,
    output_index: u32,
}

pub struct CapturedFrame<'a> {
    pub bgra: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

impl DisplayCapture {
    pub fn list_monitors() -> Result<Vec<MonitorInfo>, MediaError> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut monitors = Vec::new();
            let mut adapter_index = 0u32;
            loop {
                let adapter = match factory.EnumAdapters1(adapter_index) {
                    Ok(adapter) => adapter,
                    Err(_) => break,
                };
                let mut output_index = 0u32;
                loop {
                    let output = match adapter.EnumOutputs(output_index) {
                        Ok(output) => output,
                        Err(_) => break,
                    };
                    let desc = output.GetDesc()?;
                    let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).max(0) as u32;
                    let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).max(0) as u32;
                    monitors.push(MonitorInfo {
                        name: wide_name(&desc.DeviceName),
                        width,
                        height,
                    });
                    output_index += 1;
                }
                adapter_index += 1;
            }
            Ok(monitors)
        }
    }

    pub fn start(monitor: usize, out_w: u32, out_h: u32) -> Result<Self, MediaError> {
        let out_w = out_w.max(2) & !1;
        let out_h = out_h.max(2) & !1;
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let (adapter, output, adapter_index, output_index, desktop_w, desktop_h) =
                pick_output(&factory, monitor)?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0]),
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                Some(&raw mut context),
            )?;
            let device = device.ok_or_else(|| MediaError::message("D3D11 device missing"))?;
            let context = context.ok_or_else(|| MediaError::message("D3D11 context missing"))?;
            let output1: IDXGIOutput1 = output.cast()?;
            let duplication = output1.DuplicateOutput(&device)?;
            let (vs, ps) = compile_shaders(&device)?;
            let mut sampler = None;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    MaxAnisotropy: 1,
                    MinLOD: 0.0,
                    MaxLOD: 0.0,
                    ..Default::default()
                },
                Some(&raw mut sampler),
            )?;
            let sampler = sampler.ok_or_else(|| MediaError::message("sampler missing"))?;
            let (target, target_view, staging) = stream_textures(&device, out_w, out_h)?;
            Ok(Self {
                device,
                context,
                duplication,
                desktop_w,
                desktop_h,
                out_w,
                out_h,
                target,
                target_view,
                staging,
                vs,
                ps,
                sampler,
                packed: vec![0u8; (out_w as usize) * (out_h as usize) * 4],
                adapter_index,
                output_index,
            })
        }
    }

    pub fn frame(&mut self, timeout_ms: u32) -> Result<Option<CapturedFrame<'_>>, MediaError> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            match self.duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {}
                Err(err) if is_timeout(&err) => return Ok(None),
                Err(err) if is_access_lost(&err) => {
                    self.recreate_duplication()?;
                    return Ok(None);
                }
                Err(err) => return Err(err.into()),
            }
            let release = ReleaseGuard(&self.duplication);
            if info.LastPresentTime == 0 {
                drop(release);
                return Ok(None);
            }
            let resource = resource.ok_or_else(|| MediaError::message("duplication frame had no texture"))?;
            let texture: ID3D11Texture2D = resource.cast()?;
            let mut view = None;
            self.device.CreateShaderResourceView(&texture, None, Some(&raw mut view))?;
            let view = view.ok_or_else(|| MediaError::message("shader resource view missing"))?;
            self.draw_fitted(&view);
            self.context.PSSetShaderResources(0, Some(&[None]));
            drop(view);
            drop(release);
            self.context.CopyResource(&self.staging, &self.target);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let src = mapped.pData as *const u8;
            let row_bytes = (self.out_w as usize) * 4;
            for y in 0..self.out_h as usize {
                let from = src.add(y * pitch);
                let to = y * row_bytes;
                std::ptr::copy_nonoverlapping(from, self.packed.as_mut_ptr().add(to), row_bytes);
            }
            self.context.Unmap(&self.staging, 0);
            Ok(Some(CapturedFrame {
                bgra: &self.packed,
                width: self.out_w,
                height: self.out_h,
                stride: row_bytes,
            }))
        }
    }

    unsafe fn draw_fitted(&self, view: &ID3D11ShaderResourceView) {
        unsafe {
            let (vx, vy, vw, vh) = fit(self.desktop_w, self.desktop_h, self.out_w, self.out_h);
            self.context.OMSetRenderTargets(Some(&[Some(self.target_view.clone())]), None);
            self.context.ClearRenderTargetView(&self.target_view, &[0.0, 0.0, 0.0, 1.0]);
            self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: vx,
                TopLeftY: vy,
                Width: vw,
                Height: vh,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            self.context.PSSetShaderResources(0, Some(&[Some(view.clone())]));
            self.context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.IASetInputLayout(None);
            self.context.Draw(3, 0);
        }
    }

    unsafe fn recreate_duplication(&mut self) -> Result<(), MediaError> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let adapter = factory.EnumAdapters1(self.adapter_index)?;
            let output = adapter.EnumOutputs(self.output_index)?;
            let output1: IDXGIOutput1 = output.cast()?;
            self.duplication = output1.DuplicateOutput(&self.device)?;
            Ok(())
        }
    }
}

struct ReleaseGuard<'a>(&'a IDXGIOutputDuplication);

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.ReleaseFrame();
        }
    }
}

unsafe fn pick_output(
    factory: &IDXGIFactory1,
    monitor: usize,
) -> Result<(IDXGIAdapter1, IDXGIOutput, u32, u32, u32, u32), MediaError> {
    unsafe {
        let mut seen = 0usize;
        let mut adapter_index = 0u32;
        loop {
            let adapter = factory
                .EnumAdapters1(adapter_index)
                .map_err(|_| MediaError::message("no display outputs"))?;
            let mut output_index = 0u32;
            loop {
                let output = match adapter.EnumOutputs(output_index) {
                    Ok(output) => output,
                    Err(_) => break,
                };
                if seen == monitor {
                    let desc = output.GetDesc()?;
                    let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).max(0) as u32;
                    let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).max(0) as u32;
                    return Ok((adapter, output, adapter_index, output_index, width.max(1), height.max(1)));
                }
                seen += 1;
                output_index += 1;
            }
            adapter_index += 1;
        }
    }
}

unsafe fn stream_textures(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView, ID3D11Texture2D), MediaError> {
    unsafe {
        let mut target = None;
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            },
            None,
            Some(&raw mut target),
        )?;
        let target = target.ok_or_else(|| MediaError::message("render target missing"))?;
        let mut view = None;
        device.CreateRenderTargetView(&target, None, Some(&raw mut view))?;
        let view = view.ok_or_else(|| MediaError::message("render target view missing"))?;
        let mut staging = None;
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            },
            None,
            Some(&raw mut staging),
        )?;
        let staging = staging.ok_or_else(|| MediaError::message("staging texture missing"))?;
        let _ = size_of::<D3D11_TEXTURE2D_DESC>();
        Ok((target, view, staging))
    }
}

unsafe fn compile_shaders(device: &ID3D11Device) -> Result<(ID3D11VertexShader, ID3D11PixelShader), MediaError> {
    unsafe {
        let vs_blob = compile_stage(SCALE_HLSL, s!("main_vs"), s!("vs_4_0"))?;
        let ps_blob = compile_stage(SCALE_HLSL, s!("main_ps"), s!("ps_4_0"))?;
        let vs_bytes = std::slice::from_raw_parts(vs_blob.GetBufferPointer() as *const u8, vs_blob.GetBufferSize());
        let ps_bytes = std::slice::from_raw_parts(ps_blob.GetBufferPointer() as *const u8, ps_blob.GetBufferSize());
        let mut vs = None;
        let mut ps = None;
        device.CreateVertexShader(vs_bytes, None::<&ID3D11ClassLinkage>, Some(&raw mut vs))?;
        device.CreatePixelShader(ps_bytes, None::<&ID3D11ClassLinkage>, Some(&raw mut ps))?;
        let vs = vs.ok_or_else(|| MediaError::message("vertex shader missing"))?;
        let ps = ps.ok_or_else(|| MediaError::message("pixel shader missing"))?;
        Ok((vs, ps))
    }
}

unsafe fn compile_stage(
    source: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> Result<windows::Win32::Graphics::Direct3D::ID3DBlob, MediaError> {
    unsafe {
        let mut blob = None;
        let mut errors = None;
        let result = D3DCompile(
            source.as_ptr() as *const _,
            source.len(),
            s!("xb_scale.hlsl"),
            None,
            None::<&windows::Win32::Graphics::Direct3D::ID3DInclude>,
            entry,
            target,
            0,
            0,
            &mut blob,
            Some(&mut errors),
        );
        if let Err(err) = result {
            let detail = errors
                .as_ref()
                .map(|errors| {
                    let text = std::slice::from_raw_parts(errors.GetBufferPointer() as *const u8, errors.GetBufferSize());
                    String::from_utf8_lossy(text).into_owned()
                })
                .unwrap_or_default();
            return Err(MediaError::message(format!("shader compile: {err} {detail}")));
        }
        blob.ok_or_else(|| MediaError::message("shader compiler returned no blob"))
    }
}

fn fit(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (f32, f32, f32, f32) {
    let scale = (dst_w as f32 / src_w.max(1) as f32).min(dst_h as f32 / src_h.max(1) as f32);
    let width = (src_w as f32 * scale).max(1.0);
    let height = (src_h as f32 * scale).max(1.0);
    let x = (dst_w as f32 - width) * 0.5;
    let y = (dst_h as f32 - height) * 0.5;
    (x, y, width, height)
}

fn wide_name(name: &[u16; 32]) -> String {
    let end = name.iter().position(|unit| *unit == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

fn is_timeout(err: &windows::core::Error) -> bool {
    err.code().0 as u32 == 0x887A_0027
}

fn is_access_lost(err: &windows::core::Error) -> bool {
    err.code().0 as u32 == 0x887A_0026
}
