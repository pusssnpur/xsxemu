//! Cross-platform software Direct3D 12 command executor and triangle rasterizer.
//!
//! Enables full end-to-end execution and pixel-accurate verification of Xbox Series X
//! PE64 binaries on Linux CI and headless Windows environments without a GPU.

use std::collections::HashMap;

use xsx_abi::d3d12::{
    D3D12HeapProperties, D3D12Rect, D3D12ResourceDesc, D3D12ResourceDimension,
    D3D12VertexBufferView, D3D12Viewport, D3DPrimitiveTopology, SimpleVertex,
};

use crate::backend::{GpuBackend, GpuCommand, PresentedFrame};

struct SoftResource {
    is_texture: bool,
    width: u32,
    height: u32,
    /// Heap-allocated backing store with stable address for `Map` / `GetGPUVirtualAddress`.
    data: Box<[u8]>,
}

pub struct SoftwareD3D12Backend {
    default_width: u32,
    default_height: u32,
    resources: HashMap<u64, SoftResource>,
    rtv_map: HashMap<usize, u64>,
    last_rtv_resource: Option<u64>,

    // Pipeline state for command execution
    bound_rtv: Option<usize>,
    viewport: D3D12Viewport,
    scissor: Option<D3D12Rect>,
    topology: u32,
    vertex_buffers: HashMap<u32, D3D12VertexBufferView>,
}

impl SoftwareD3D12Backend {
    pub fn new(default_width: u32, default_height: u32) -> Self {
        Self {
            default_width,
            default_height,
            resources: HashMap::new(),
            rtv_map: HashMap::new(),
            last_rtv_resource: None,
            bound_rtv: None,
            viewport: D3D12Viewport {
                top_left_x: 0.0,
                top_left_y: 0.0,
                width: default_width as f32,
                height: default_height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            },
            scissor: None,
            topology: D3DPrimitiveTopology::TriangleList as u32,
            vertex_buffers: HashMap::new(),
        }
    }

    fn resolve_rtv_resource(&self, rtv_handle: usize) -> Option<u64> {
        self.rtv_map.get(&rtv_handle).copied()
    }

    fn clear_rtv(&mut self, rtv_handle: usize, color: [f32; 4], rects: &[D3D12Rect]) {
        let Some(res_id) = self.resolve_rtv_resource(rtv_handle) else {
            return;
        };
        let Some(res) = self.resources.get_mut(&res_id) else {
            return;
        };
        if !res.is_texture {
            return;
        }
        let rgba = [
            float_to_u8(color[0]),
            float_to_u8(color[1]),
            float_to_u8(color[2]),
            float_to_u8(color[3]),
        ];

        if rects.is_empty() {
            for px in res.data.chunks_exact_mut(4) {
                px.copy_from_slice(&rgba);
            }
        } else {
            let w = res.width as i32;
            let h = res.height as i32;
            for r in rects {
                let x0 = r.left.clamp(0, w);
                let x1 = r.right.clamp(0, w);
                let y0 = r.top.clamp(0, h);
                let y1 = r.bottom.clamp(0, h);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let idx = ((y as u32 * res.width + x as u32) * 4) as usize;
                        res.data[idx..idx + 4].copy_from_slice(&rgba);
                    }
                }
            }
        }
    }

    fn draw_instanced(&mut self, vertex_count: u32, start_vertex: u32) {
        if self.topology != D3DPrimitiveTopology::TriangleList as u32 {
            return;
        }
        let Some(rtv_handle) = self.bound_rtv else {
            return;
        };
        let Some(res_id) = self.resolve_rtv_resource(rtv_handle) else {
            return;
        };
        let Some(vb) = self.vertex_buffers.get(&0).copied() else {
            return;
        };
        if vb.buffer_location == 0 || vb.stride_in_bytes < core::mem::size_of::<SimpleVertex>() as u32
        {
            return;
        }

        // Read vertices from the guest/resource memory at vb.buffer_location
        let stride = vb.stride_in_bytes as usize;
        let max_verts = (vb.size_in_bytes as usize) / stride;
        let end_vert = ((start_vertex + vertex_count) as usize).min(max_verts);
        let start = start_vertex as usize;
        if end_vert <= start {
            return;
        }

        let mut vertices = Vec::with_capacity(end_vert - start);
        for i in start..end_vert {
            let ptr = (vb.buffer_location as usize + i * stride) as *const SimpleVertex;
            // Safety: caller bound a valid mapped buffer or guest .rdata vertex array.
            let v = unsafe { ptr.read_unaligned() };
            vertices.push(v);
        }

        let vp = self.viewport;
        let scissor = self.scissor;
        let Some(res) = self.resources.get_mut(&res_id) else {
            return;
        };
        if !res.is_texture {
            return;
        }

        for tri in vertices.chunks_exact(3) {
            rasterize_triangle(res, vp, scissor, tri[0], tri[1], tri[2]);
        }
    }
}

impl GpuBackend for SoftwareD3D12Backend {
    fn name(&self) -> &'static str {
        "SoftwareD3D12"
    }

    fn create_resource(
        &mut self,
        id: u64,
        _heap_props: &D3D12HeapProperties,
        _heap_flags: u32,
        desc: &D3D12ResourceDesc,
        _initial_state: u32,
    ) -> Result<u64, String> {
        let is_texture = desc.dimension == D3D12ResourceDimension::Texture2D as u32;
        let (width, height, byte_len) = if is_texture {
            let w = (desc.width as u32).max(1);
            let h = desc.height.max(1);
            (w, h, (w as usize) * (h as usize) * 4)
        } else {
            let sz = (desc.width as usize).max(16);
            (sz as u32, 1, sz)
        };

        let data = vec![0u8; byte_len].into_boxed_slice();
        let gpu_va = data.as_ptr() as u64;
        self.resources.insert(
            id,
            SoftResource {
                is_texture,
                width,
                height,
                data,
            },
        );
        Ok(gpu_va)
    }

    fn map_resource(&mut self, id: u64) -> Result<*mut u8, String> {
        self.resources
            .get_mut(&id)
            .map(|r| r.data.as_mut_ptr())
            .ok_or_else(|| format!("unknown resource id {id}"))
    }

    fn unmap_resource(&mut self, _id: u64) {}

    fn create_rtv(&mut self, rtv_handle: usize, resource_id: u64) {
        self.rtv_map.insert(rtv_handle, resource_id);
        self.last_rtv_resource = Some(resource_id);
    }

    fn execute_commands(&mut self, commands: &[GpuCommand]) -> Result<(), String> {
        for cmd in commands {
            match cmd {
                GpuCommand::ResourceBarrier { .. } => {}
                GpuCommand::SetViewport(vp) => self.viewport = *vp,
                GpuCommand::SetScissor(rect) => self.scissor = Some(*rect),
                GpuCommand::SetRenderTarget { rtv_handle } => {
                    self.bound_rtv = Some(*rtv_handle);
                    if let Some(res_id) = self.resolve_rtv_resource(*rtv_handle) {
                        self.last_rtv_resource = Some(res_id);
                    }
                }
                GpuCommand::ClearRenderTarget {
                    rtv_handle,
                    color,
                    rects,
                } => self.clear_rtv(*rtv_handle, *color, rects),
                GpuCommand::SetPrimitiveTopology(topo) => self.topology = *topo,
                GpuCommand::SetVertexBuffer { slot, view } => {
                    self.vertex_buffers.insert(*slot, *view);
                }
                GpuCommand::DrawInstanced {
                    vertex_count,
                    start_vertex,
                    ..
                } => self.draw_instanced(*vertex_count, *start_vertex),
            }
        }
        Ok(())
    }

    fn present_resource(
        &mut self,
        resource_id: Option<u64>,
        frame_index: u64,
        _sync_interval: u32,
    ) -> Result<Option<PresentedFrame>, String> {
        let target_id = resource_id.or(self.last_rtv_resource);
        if let Some(id) = target_id {
            if let Some(res) = self.resources.get(&id) {
                return Ok(Some(PresentedFrame {
                    frame_index,
                    width: res.width,
                    height: res.height,
                    rgba: res.data.to_vec(),
                }));
            }
        }
        Ok(Some(PresentedFrame {
            frame_index,
            width: self.default_width,
            height: self.default_height,
            rgba: vec![0u8; (self.default_width * self.default_height * 4) as usize],
        }))
    }
}

fn float_to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn ndc_to_screen(pos: [f32; 3], vp: D3D12Viewport) -> (f32, f32) {
    let sx = vp.top_left_x + (pos[0] * 0.5 + 0.5) * vp.width;
    let sy = vp.top_left_y + (0.5 - pos[1] * 0.5) * vp.height;
    (sx, sy)
}

fn edge_fn(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (px - ax) * (by - ay) - (py - ay) * (bx - ax)
}

fn rasterize_triangle(
    res: &mut SoftResource,
    vp: D3D12Viewport,
    scissor: Option<D3D12Rect>,
    v0: SimpleVertex,
    v1: SimpleVertex,
    v2: SimpleVertex,
) {
    let (x0, y0) = ndc_to_screen(v0.pos, vp);
    let (x1, y1) = ndc_to_screen(v1.pos, vp);
    let (x2, y2) = ndc_to_screen(v2.pos, vp);

    let area = edge_fn(x0, y0, x1, y1, x2, y2);
    if area.abs() < 1e-6 {
        return;
    }

    let min_x = x0.min(x1).min(x2).floor().max(0.0) as i32;
    let max_x = x0.max(x1).max(x2).ceil().min(res.width as f32 - 1.0) as i32;
    let min_y = y0.min(y1).min(y2).floor().max(0.0) as i32;
    let max_y = y0.max(y1).max(y2).ceil().min(res.height as f32 - 1.0) as i32;

    let (sx0, sy0, sx1, sy1) = match scissor {
        Some(s) => (
            min_x.max(s.left),
            min_y.max(s.top),
            max_x.min(s.right - 1),
            max_y.min(s.bottom - 1),
        ),
        None => (min_x, min_y, max_x, max_y),
    };

    for y in sy0..=sy1 {
        for x in sx0..=sx1 {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let w0 = edge_fn(x1, y1, x2, y2, px, py);
            let w1 = edge_fn(x2, y2, x0, y0, px, py);
            let w2 = edge_fn(x0, y0, x1, y1, px, py);

            let inside = if area > 0.0 {
                w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0
            } else {
                w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0
            };

            if inside {
                let b0 = w0 / area;
                let b1 = w1 / area;
                let b2 = w2 / area;
                let r = b0 * v0.color[0] + b1 * v1.color[0] + b2 * v2.color[0];
                let g = b0 * v0.color[1] + b1 * v1.color[1] + b2 * v2.color[1];
                let b = b0 * v0.color[2] + b1 * v1.color[2] + b2 * v2.color[2];
                let a = b0 * v0.color[3] + b1 * v1.color[3] + b2 * v2.color[3];
                let idx = ((y as u32 * res.width + x as u32) * 4) as usize;
                res.data[idx] = float_to_u8(r);
                res.data[idx + 1] = float_to_u8(g);
                res.data[idx + 2] = float_to_u8(b);
                res.data[idx + 3] = float_to_u8(a);
            }
        }
    }
}
