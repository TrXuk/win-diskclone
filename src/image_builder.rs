//! Assembles full disk image from GPT header and partition data.
//!
//! Preserves LBA positions: GPT, gaps, and backup GPT are written so the output
//! matches the source disk layout exactly. Partition tools (gdisk, etc.) see
//! valid GPT and partition table.

use std::collections::HashMap;

use winapi::um::winnt::HANDLE;

use crate::diagram::{build_diagram_regions, DiagramRegion};
use crate::disk::{
    get_disk_layout, open_physical_disk, open_volume_raw, read_sectors, DiskLayout, SECTOR_SIZE,
};
use crate::error::Result;
use crate::local_sink::ImageSink;
use crate::vss::VssSnapshot;

/// GPT header and partition table size in sectors (primary at LBA 0, backup at end).
const GPT_HEADER_SECTORS: u64 = 34;

/// Builds and streams a full bootable disk image.
pub struct ImageBuilder {
    disk_handle: HANDLE,
    layout: DiskLayout,
    shadow_map: HashMap<u64, String>,
    buffer_size: usize,
}

impl ImageBuilder {
    /// Creates a new ImageBuilder. The VSS snapshot must be kept alive during the build.
    pub fn new(
        disk_number: u32,
        vss_snapshot: &VssSnapshot,
        buffer_size_mb: usize,
    ) -> Result<Self> {
        let disk_handle = open_physical_disk(disk_number)?;
        let layout = get_disk_layout(disk_handle)?;
        let shadow_map = vss_snapshot.build_partition_shadow_map(&layout.partitions, disk_number)?;

        let buffer_size = (buffer_size_mb * 1024 * 1024).max(512);
        let buffer_size = (buffer_size / 512) * 512;

        Ok(Self {
            disk_handle,
            layout,
            shadow_map,
            buffer_size,
        })
    }

    /// Streams the full disk image to the sink, preserving LBA layout.
    /// Writes: primary GPT from disk, gaps (zeros), partition data from shadow/disk,
    /// and backup GPT at end of disk.
    pub fn stream_to<S: ImageSink>(&self, sink: &mut S) -> Result<u64> {
        let mut total_written: u64 = 0;
        let mut buffer = vec![0u8; self.buffer_size];
        let disk_length = self.layout.disk_length;
        let gpt_size = GPT_HEADER_SECTORS * SECTOR_SIZE;
        let backup_start = disk_length.saturating_sub(gpt_size);

        let mut pos = 0u64;

        while pos < disk_length {
            // Limit chunk to not cross region boundaries
            let mut chunk_end = pos + self.buffer_size as u64;
            chunk_end = chunk_end.min(disk_length);

            if pos < gpt_size {
                chunk_end = chunk_end.min(gpt_size);
            } else if pos >= backup_start {
                // already in backup GPT
            } else {
                // In middle: limit to start of next partition or backup
                for p in &self.layout.partitions {
                    if !p.is_used || p.partition_length == 0 {
                        continue;
                    }
                    let p_start = p.starting_offset;
                    let p_end = p.starting_offset + p.partition_length;
                    if pos < p_start && chunk_end > p_start {
                        chunk_end = p_start;
                    }
                    if pos >= p_start && pos < p_end && chunk_end > p_end {
                        chunk_end = p_end;
                    }
                }
                if chunk_end > backup_start && pos < backup_start {
                    chunk_end = backup_start;
                }
            }

            let to_write = (chunk_end - pos) as usize;
            if to_write == 0 {
                break;
            }

            // Primary GPT: [0, gpt_size)
            if pos < gpt_size {
                let n = read_sectors(self.disk_handle, pos, &mut buffer[..to_write])?;
                let written = sink.write(&buffer[..n])?;
                total_written += written as u64;
                pos += written as u64;
                continue;
            }

            // Backup GPT: [backup_start, disk_length)
            if pos >= backup_start {
                let n = read_sectors(self.disk_handle, pos, &mut buffer[..to_write])?;
                let written = sink.write(&buffer[..n])?;
                total_written += written as u64;
                pos += written as u64;
                continue;
            }

            // Check if this chunk is within a partition
            let mut in_partition = false;
            for partition in &self.layout.partitions {
                if !partition.is_used || partition.partition_length == 0 {
                    continue;
                }
                let part_start = partition.starting_offset;
                let part_end = partition.starting_offset + partition.partition_length;

                if pos < part_start || pos >= part_end {
                    continue;
                }

                let read_handle: HANDLE = if let Some(shadow_path) = self.shadow_map.get(&partition.starting_offset) {
                    let path = if shadow_path.starts_with(r"\\?\") {
                        format!(r"\\.\{}", &shadow_path[4..])
                    } else if shadow_path.starts_with(r"\\.\") {
                        shadow_path.clone()
                    } else {
                        format!(r"\\.\GLOBALROOT\Device\{}", shadow_path)
                    };
                    open_volume_raw(&path)?
                } else {
                    self.disk_handle
                };

                let part_offset = if read_handle == self.disk_handle {
                    partition.starting_offset
                } else {
                    0
                };

                let read_offset = part_offset + (pos - part_start);

                let n = read_sectors(read_handle, read_offset, &mut buffer[..to_write])?;
                let written = sink.write(&buffer[..n])?;
                total_written += written as u64;
                pos += written as u64;

                if read_handle != self.disk_handle {
                    unsafe {
                        winapi::um::handleapi::CloseHandle(read_handle);
                    }
                }
                in_partition = true;
                break;
            }

            if !in_partition {
                // Gap: write zeros
                buffer[..to_write].fill(0);
                let written = sink.write(&buffer[..to_write])?;
                total_written += written as u64;
                pos += written as u64;
            }
        }

        sink.flush()?;
        Ok(total_written)
    }

    /// Returns the total disk size.
    pub fn disk_length(&self) -> u64 {
        self.layout.disk_length
    }

    /// Returns regions for the layout diagram (source disk vs output image).
    pub fn diagram_regions(&self) -> Vec<DiagramRegion> {
        build_diagram_regions(&self.layout, &self.shadow_map)
    }
}

impl Drop for ImageBuilder {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.disk_handle);
        }
    }
}
