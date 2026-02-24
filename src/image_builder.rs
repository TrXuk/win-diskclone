//! Assembles full disk image from GPT header and partition data.

use std::collections::HashMap;

use winapi::um::winnt::HANDLE;

use crate::disk::{
    get_disk_layout, open_physical_disk, open_volume_raw, read_sectors, DiskLayout, SECTOR_SIZE,
};
use crate::error::Result;
use crate::local_sink::ImageSink;
use crate::vss::VssSnapshot;

/// GPT header and partition table size in sectors.
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

    /// Streams the full disk image to the sink.
    pub fn stream_to<S: ImageSink>(&self, sink: &mut S) -> Result<u64> {
        let mut total_written: u64 = 0;

        let mut buffer = vec![0u8; self.buffer_size];

        let gpt_size = GPT_HEADER_SECTORS * SECTOR_SIZE;
        let mut offset = 0u64;

        while offset < gpt_size {
            let to_read = (gpt_size - offset).min(self.buffer_size as u64) as usize;
            let n = read_sectors(self.disk_handle, offset, &mut buffer[..to_read])?;
            let written = sink.write(&buffer[..n])?;
            total_written += written as u64;
            offset += n as u64;
        }

        for partition in &self.layout.partitions {
            if !partition.is_used || partition.partition_length == 0 {
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

            let mut part_remaining = partition.partition_length;

            while part_remaining > 0 {
                let to_read = part_remaining.min(self.buffer_size as u64) as usize;
                let to_read = (to_read / 512) * 512;
                if to_read == 0 {
                    break;
                }

                let n = read_sectors(read_handle, part_offset + (partition.partition_length - part_remaining), &mut buffer[..to_read])?;
                let written = sink.write(&buffer[..n])?;
                total_written += written as u64;
                part_remaining -= n as u64;
            }

            if read_handle != self.disk_handle {
                unsafe {
                    winapi::um::handleapi::CloseHandle(read_handle);
                }
            }
        }

        sink.flush()?;
        Ok(total_written)
    }

    /// Returns the total disk size.
    pub fn disk_length(&self) -> u64 {
        self.layout.disk_length
    }
}

impl Drop for ImageBuilder {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.disk_handle);
        }
    }
}
