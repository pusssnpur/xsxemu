//! Xbox Series X UMA -> Discrete PC GPU upload staging ring buffer.
//!
//! Xbox Series X exposes unified GDDR6 memory where titles frequently write directly
//! into `DEFAULT` / `CUSTOM` CPU-cached resources. On discrete PC GPUs, those writes
//! must be staged through an `UPLOAD` ring buffer and copied via `CopyBufferRegion`
//! / `CopyTextureRegion`, then retired once the GPU fence completes.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagingAllocation {
    pub offset: usize,
    pub size: usize,
    pub fence_value: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingStagingCopy {
    pub src_offset: usize,
    pub dst_resource_id: u64,
    pub dst_offset: u64,
    pub size_bytes: usize,
}

/// Lock-free-style ring buffer allocator for staging CPU writes to discrete GPU resources.
pub struct UmaStagingRing {
    buffer: Vec<u8>,
    head: usize,
    tail: usize,
    used: usize,
    in_flight: VecDeque<StagingAllocation>,
    pending_copies: Vec<PendingStagingCopy>,
    total_bytes_staged: u64,
}

impl UmaStagingRing {
    pub fn new(capacity_bytes: usize) -> Self {
        let cap = capacity_bytes.max(64 * 1024);
        Self {
            buffer: vec![0u8; cap],
            head: 0,
            tail: 0,
            used: 0,
            in_flight: VecDeque::new(),
            pending_copies: Vec::new(),
            total_bytes_staged: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }

    pub fn total_bytes_staged(&self) -> u64 {
        self.total_bytes_staged
    }

    /// Stage `data` into the upload ring with `alignment` (typically 256 or 512 bytes)
    /// and record a pending GPU copy targeting `(dst_resource_id, dst_offset)`.
    pub fn stage_copy(
        &mut self,
        data: &[u8],
        alignment: usize,
        dst_resource_id: u64,
        dst_offset: u64,
        fence_value: u64,
    ) -> Result<StagingAllocation, String> {
        let align = alignment.max(16);
        let cap = self.buffer.len();
        let aligned_len = align_up(data.len().max(1), align);
        if aligned_len > cap {
            return Err(format!(
                "staging request ({aligned_len} bytes) exceeds ring capacity ({cap} bytes)"
            ));
        }

        let mut offset = align_up(self.head, align);
        let mut waste = offset - self.head;
        if offset + aligned_len > cap {
            // Wrap around to offset 0
            waste = cap - self.head;
            offset = 0;
        }

        let total_needed = waste + aligned_len;
        if self.used + total_needed > cap {
            return Err("UMA staging ring full (fence retirement required)".into());
        }

        self.buffer[offset..offset + data.len()].copy_from_slice(data);
        self.head = (offset + aligned_len) % cap;
        self.used += total_needed;
        self.total_bytes_staged += data.len() as u64;

        let alloc = StagingAllocation {
            offset,
            size: total_needed,
            fence_value,
        };
        self.in_flight.push_back(alloc);
        self.pending_copies.push(PendingStagingCopy {
            src_offset: offset,
            dst_resource_id,
            dst_offset,
            size_bytes: data.len(),
        });
        Ok(alloc)
    }

    /// Drain pending GPU copies to be encoded onto the next host D3D12 copy/direct command list.
    pub fn drain_pending_copies(&mut self) -> Vec<PendingStagingCopy> {
        std::mem::take(&mut self.pending_copies)
    }

    /// Reclaim ring space for all allocations whose fence value `<= completed_fence`.
    pub fn retire_up_to(&mut self, completed_fence: u64) -> usize {
        let mut retired = 0usize;
        while let Some(front) = self.in_flight.front() {
            if front.fence_value <= completed_fence {
                let alloc = self.in_flight.pop_front().unwrap();
                self.tail = (self.tail + alloc.size) % self.buffer.len();
                self.used = self.used.saturating_sub(alloc.size);
                retired += 1;
            } else {
                break;
            }
        }
        retired
    }

    pub fn read_staged(&self, offset: usize, len: usize) -> &[u8] {
        &self.buffer[offset..offset + len]
    }
}

fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_wraps_and_retires_by_fence() {
        let mut ring = UmaStagingRing::new(64 * 1024);
        let a1 = ring.stage_copy(&[1u8; 1024], 256, 10, 0, 1).unwrap();
        let a2 = ring.stage_copy(&[2u8; 2048], 512, 11, 512, 2).unwrap();
        assert_eq!(ring.read_staged(a1.offset, 4), &[1, 1, 1, 1]);
        assert_eq!(ring.read_staged(a2.offset, 4), &[2, 2, 2, 2]);
        assert_eq!(ring.drain_pending_copies().len(), 2);

        assert_eq!(ring.retire_up_to(1), 1);
        assert!(ring.used_bytes() > 0);
        assert_eq!(ring.retire_up_to(2), 1);
        assert_eq!(ring.used_bytes(), 0);
    }
}
