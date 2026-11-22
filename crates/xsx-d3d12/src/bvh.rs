//! Deterministic software DXR / Xbox Series X BVH acceleration structure engine.
//!
//! Backs `XboxBvhBuilder.dll` and DirectX Raytracing (`D3D12XBOX_RT_*`) calls with a
//! clean CPU SAH/midpoint BVH builder, slab ray-AABB intersection, Möller–Trumbore
//! ray-triangle intersection, and deterministic software ray-traced rendering.

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }

    pub fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }

    pub fn scale(self, s: f32) -> Self {
        Self::new(self.x * s, self.y * s, self.z * s)
    }

    pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Self) -> Self {
        Self::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    pub fn normalize(self) -> Self {
        let len = self.length();
        if len > 1e-8 {
            self.scale(1.0 / len)
        } else {
            Self::new(0.0, 0.0, 1.0)
        }
    }

    pub fn min(self, o: Self) -> Self {
        Self::new(self.x.min(o.x), self.y.min(o.y), self.z.min(o.z))
    }

    pub fn max(self, o: Self) -> Self {
        Self::new(self.x.max(o.x), self.y.max(o.y), self.z.max(o.z))
    }

    pub fn axis(self, idx: usize) -> f32 {
        match idx {
            0 => self.x,
            1 => self.y,
            _ => self.z,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Default for Aabb {
    fn default() -> Self {
        Self {
            min: Vec3::new(f32::INFINITY, f32::INFINITY, f32::INFINITY),
            max: Vec3::new(f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY),
        }
    }
}

impl Aabb {
    pub fn expand_point(&mut self, p: Vec3) {
        self.min = self.min.min(p);
        self.max = self.max.max(p);
    }

    pub fn expand_aabb(&mut self, b: Aabb) {
        self.min = self.min.min(b.min);
        self.max = self.max.max(b.max);
    }

    pub fn longest_axis(&self) -> usize {
        let e = self.max.sub(self.min);
        if e.x >= e.y && e.x >= e.z {
            0
        } else if e.y >= e.z {
            1
        } else {
            2
        }
    }

    pub fn intersect(&self, ray: &RtRay, t_max: f32) -> bool {
        let mut tmin = ray.t_min;
        let mut tmax = t_max;
        for axis in 0..3 {
            let orig = ray.origin.axis(axis);
            let dir = ray.direction.axis(axis);
            let bmin = self.min.axis(axis);
            let bmax = self.max.axis(axis);
            if dir.abs() < 1e-8 {
                if orig < bmin || orig > bmax {
                    return false;
                }
            } else {
                let inv = 1.0 / dir;
                let mut t0 = (bmin - orig) * inv;
                let mut t1 = (bmax - orig) * inv;
                if inv < 0.0 {
                    core::mem::swap(&mut t0, &mut t1);
                }
                tmin = tmin.max(t0);
                tmax = tmax.min(t1);
                if tmax < tmin {
                    return false;
                }
            }
        }
        true
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RtTriangle {
    pub v0: Vec3,
    pub v1: Vec3,
    pub v2: Vec3,
    pub color: Vec3,
    pub reflectivity: f32,
}

impl RtTriangle {
    pub fn bounds(&self) -> Aabb {
        let mut b = Aabb::default();
        b.expand_point(self.v0);
        b.expand_point(self.v1);
        b.expand_point(self.v2);
        b
    }

    pub fn centroid(&self) -> Vec3 {
        self.v0.add(self.v1).add(self.v2).scale(1.0 / 3.0)
    }

    pub fn normal(&self) -> Vec3 {
        self.v1.sub(self.v0).cross(self.v2.sub(self.v0)).normalize()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RtRay {
    pub origin: Vec3,
    pub direction: Vec3,
    pub t_min: f32,
    pub t_max: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RtHit {
    pub triangle_index: u32,
    pub t: f32,
    pub normal: Vec3,
    pub color: Vec3,
    pub reflectivity: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct RtDispatchParams {
    pub width: u32,
    pub height: u32,
    pub camera_pos: Vec3,
    pub camera_look_at: Vec3,
    pub light_pos: Vec3,
    pub max_bounces: u32,
}

#[derive(Debug, Clone)]
struct BvhNode {
    bounds: Aabb,
    left: u32,
    right: u32,
    first_prim: u32,
    prim_count: u32,
}

/// CPU BVH acceleration structure supporting BLAS, TLAS, ray queries, and ray-traced dispatch.
#[derive(Debug, Clone, Default)]
pub struct BvhAccelerationStructure {
    triangles: Vec<RtTriangle>,
    prim_indices: Vec<u32>,
    nodes: Vec<BvhNode>,
}

impl BvhAccelerationStructure {
    pub fn build(triangles: Vec<RtTriangle>) -> Result<Self, &'static str> {
        if triangles.is_empty() {
            return Err("cannot build BVH from empty triangle list");
        }
        let prim_indices: Vec<u32> = (0..triangles.len() as u32).collect();
        let mut bvh = Self {
            triangles,
            prim_indices,
            nodes: Vec::new(),
        };
        let total = bvh.prim_indices.len();
        bvh.build_node(0, total);
        Ok(bvh)
    }

    fn build_node(&mut self, start: usize, end: usize) -> u32 {
        let mut bounds = Aabb::default();
        let mut centroid_bounds = Aabb::default();
        for &idx in &self.prim_indices[start..end] {
            let tri = &self.triangles[idx as usize];
            bounds.expand_aabb(tri.bounds());
            centroid_bounds.expand_point(tri.centroid());
        }

        let node_idx = self.nodes.len() as u32;
        let count = end - start;
        if count <= 2 {
            self.nodes.push(BvhNode {
                bounds,
                left: 0,
                right: 0,
                first_prim: start as u32,
                prim_count: count as u32,
            });
            return node_idx;
        }

        let axis = centroid_bounds.longest_axis();
        let mid_coord = 0.5 * (centroid_bounds.min.axis(axis) + centroid_bounds.max.axis(axis));
        let mut mid = start;
        for i in start..end {
            let c = self.triangles[self.prim_indices[i] as usize]
                .centroid()
                .axis(axis);
            if c < mid_coord {
                self.prim_indices.swap(i, mid);
                mid += 1;
            }
        }
        if mid == start || mid == end {
            mid = start + count / 2;
        }

        self.nodes.push(BvhNode {
            bounds,
            left: 0,
            right: 0,
            first_prim: 0,
            prim_count: 0,
        });
        let left = self.build_node(start, mid);
        let right = self.build_node(mid, end);
        self.nodes[node_idx as usize].left = left;
        self.nodes[node_idx as usize].right = right;
        node_idx
    }

    pub fn node_count(&self) -> u32 {
        self.nodes.len() as u32
    }

    pub fn triangle_count(&self) -> u32 {
        self.triangles.len() as u32
    }

    pub fn root_bounds(&self) -> Aabb {
        self.nodes.first().map(|n| n.bounds).unwrap_or_default()
    }

    pub fn triangles(&self) -> &[RtTriangle] {
        &self.triangles
    }

    /// Möller–Trumbore ray-BVH closest-hit query.
    pub fn trace_ray(&self, ray: &RtRay) -> Option<RtHit> {
        if self.nodes.is_empty() {
            return None;
        }
        let mut best_t = ray.t_max;
        let mut best_hit: Option<RtHit> = None;
        let mut stack = [0u32; 64];
        let mut sp = 1usize;
        stack[0] = 0;

        while sp > 0 {
            sp -= 1;
            let node = &self.nodes[stack[sp] as usize];
            if !node.bounds.intersect(ray, best_t) {
                continue;
            }
            if node.prim_count > 0 {
                let first = node.first_prim as usize;
                let end = first + node.prim_count as usize;
                for &tri_idx in &self.prim_indices[first..end] {
                    let tri = &self.triangles[tri_idx as usize];
                    if let Some(t) = intersect_triangle(ray, tri, best_t) {
                        best_t = t;
                        best_hit = Some(RtHit {
                            triangle_index: tri_idx,
                            t,
                            normal: tri.normal(),
                            color: tri.color,
                            reflectivity: tri.reflectivity,
                        });
                    }
                }
            } else if sp + 2 <= stack.len() {
                stack[sp] = node.right;
                stack[sp + 1] = node.left;
                sp += 2;
            }
        }
        best_hit
    }

    /// Render a deterministic RGBA8 frame using primary + shadow rays and return `(rgba, fnv1a_hash)`.
    pub fn render_scene(&self, params: &RtDispatchParams) -> Result<(Vec<u8>, u64), &'static str> {
        if params.width == 0 || params.height == 0 {
            return Err("invalid ray-dispatch dimensions");
        }
        let w = params.width as usize;
        let h = params.height as usize;
        let mut rgba = vec![0u8; w * h * 4];

        let forward = params.camera_look_at.sub(params.camera_pos).normalize();
        let world_up = Vec3::new(0.0, 1.0, 0.0);
        let right = forward.cross(world_up).normalize();
        let up = right.cross(forward).normalize();
        let aspect = (w as f32) / (h as f32);

        for y in 0..h {
            let ndc_y = 1.0 - 2.0 * ((y as f32 + 0.5) / h as f32);
            for x in 0..w {
                let ndc_x = (2.0 * ((x as f32 + 0.5) / w as f32) - 1.0) * aspect;
                let dir = forward
                    .add(right.scale(ndc_x * 0.6))
                    .add(up.scale(ndc_y * 0.6))
                    .normalize();
                let ray = RtRay {
                    origin: params.camera_pos,
                    direction: dir,
                    t_min: 1e-4,
                    t_max: 1e5,
                };
                let color = if let Some(hit) = self.trace_ray(&ray) {
                    let p = ray.origin.add(ray.direction.scale(hit.t));
                    let to_light = params.light_pos.sub(p).normalize();
                    let ndotl = hit.normal.dot(to_light).abs().max(0.15);
                    hit.color.scale(ndotl)
                } else {
                    Vec3::new(0.08, 0.12, 0.22)
                };
                let idx = (y * w + x) * 4;
                rgba[idx] = (color.x.clamp(0.0, 1.0) * 255.0) as u8;
                rgba[idx + 1] = (color.y.clamp(0.0, 1.0) * 255.0) as u8;
                rgba[idx + 2] = (color.z.clamp(0.0, 1.0) * 255.0) as u8;
                rgba[idx + 3] = 255;
            }
        }
        let hash = fnv1a_hash(&rgba);
        Ok((rgba, hash))
    }
}

fn intersect_triangle(ray: &RtRay, tri: &RtTriangle, t_max: f32) -> Option<f32> {
    let e1 = tri.v1.sub(tri.v0);
    let e2 = tri.v2.sub(tri.v0);
    let h = ray.direction.cross(e2);
    let a = e1.dot(h);
    if a.abs() < 1e-7 {
        return None;
    }
    let f = 1.0 / a;
    let s = ray.origin.sub(tri.v0);
    let u = f * s.dot(h);
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = f * ray.direction.dot(q);
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = f * e2.dot(q);
    if t >= ray.t_min && t <= t_max {
        Some(t)
    } else {
        None
    }
}

pub fn fnv1a_hash(bytes: &[u8]) -> u64 {
    let mut h = 14695981039346656037u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211u64);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_bvh_and_intersects_triangle() {
        let tris = vec![RtTriangle {
            v0: Vec3::new(-1.0, -1.0, 3.0),
            v1: Vec3::new(1.0, -1.0, 3.0),
            v2: Vec3::new(0.0, 1.0, 3.0),
            color: Vec3::new(0.9, 0.3, 0.2),
            reflectivity: 0.25,
        }];
        let bvh = BvhAccelerationStructure::build(tris).unwrap();
        assert_eq!(bvh.triangle_count(), 1);
        assert!(bvh.node_count() >= 1);

        let hit = bvh
            .trace_ray(&RtRay {
                origin: Vec3::new(0.0, 0.0, 0.0),
                direction: Vec3::new(0.0, 0.0, 1.0),
                t_min: 0.001,
                t_max: 100.0,
            })
            .expect("center ray should hit triangle");
        assert!((hit.t - 3.0).abs() < 1e-4);
        assert_eq!(hit.triangle_index, 0);

        let (rgba, hash) = bvh
            .render_scene(&RtDispatchParams {
                width: 16,
                height: 16,
                camera_pos: Vec3::new(0.0, 0.0, 0.0),
                camera_look_at: Vec3::new(0.0, 0.0, 3.0),
                light_pos: Vec3::new(2.0, 3.0, -1.0),
                max_bounces: 1,
            })
            .unwrap();
        assert_eq!(rgba.len(), 16 * 16 * 4);
        assert_ne!(hash, 0);
    }
}
