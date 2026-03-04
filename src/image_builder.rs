//! Assembles full disk image from GPT header and partition data.
//!
//! Preserves LBA positions: GPT, gaps, and backup GPT are written so the output
//! matches the source disk layout exactly. Partition tools (gdisk, etc.) see
//! valid GPT and partition table.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

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

/// Current streaming region info for progress display.
#[derive(Debug, Clone, Default)]
pub struct StreamRegionInfo {
    pub partition_num: Option<u32>,
    pub source_type: StreamSourceType,
    pub path: Option<String>,
}

/// Type of data source being streamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamSourceType {
    #[default]
    PrimaryGpt,
    BackupGpt,
    Gap,
    PartitionVss,
    PartitionRaw,
    PartitionExcluded,
}

/// Builds and streams a full bootable disk image.
pub struct ImageBuilder {
    disk_number: u32,
    disk_handle: HANDLE,
    layout: DiskLayout,
    shadow_map: HashMap<u64, String>,
    buffer_size: usize,
}

impl ImageBuilder {
    /// Diagnostic: prints shadow map and verifies GPT can be read from disk.
    /// Used for debugging "zero partitions" output issues.
    pub fn debug_image_diag(&self) -> Result<String> {
        use std::fmt::Write;
        use crate::disk::read_sectors;

        let mut out = String::new();
        writeln!(out, "=== Image Builder Diagnostic ===").ok();
        writeln!(out, "Shadow map ({} entries):", self.shadow_map.len()).ok();
        for (off, path) in &self.shadow_map {
            writeln!(out, "  offset {} -> {}", off, path).ok();
        }
        writeln!(out, "Partitions:").ok();
        for p in &self.layout.partitions {
            if p.is_used && p.partition_length > 0 {
                let has_shadow = self.shadow_map.contains_key(&p.starting_offset);
                writeln!(
                    out,
                    "  Part {}: offset={} len={} shadow={}",
                    p.partition_number, p.starting_offset, p.partition_length, has_shadow
                )
                .ok();
            }
        }
        // Read first 512 bytes (MBR) and verify signature
        let mut mbr = [0u8; 512];
        let n = read_sectors(self.disk_handle, 0, &mut mbr)?;
        if n >= 512 {
            let sig = mbr[510] as u16 | ((mbr[511] as u16) << 8);
            writeln!(out, "MBR signature: 0x{:04x} (expected 0xAA55)", sig).ok();
            if sig != 0xAA55 {
                writeln!(out, "  WARNING: Invalid MBR signature!").ok();
            }
        }
        // Read GPT header (sector 1) - "EFI PART" at offset 0
        let mut gpt_sector = [0u8; 512];
        let n = read_sectors(self.disk_handle, 512, &mut gpt_sector)?;
        if n >= 8 {
            let sig = std::str::from_utf8(&gpt_sector[0..8]).unwrap_or("???");
            writeln!(out, "GPT signature: \"{}\" (expected \"EFI PART\")", sig).ok();
            if sig != "EFI PART" {
                writeln!(out, "  WARNING: Invalid GPT signature!").ok();
            }
        }
        Ok(out)
    }

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
            disk_number,
            disk_handle,
            layout,
            shadow_map,
            buffer_size,
        })
    }

    /// Streams the full disk image to the sink, preserving LBA layout.
    /// Writes: primary GPT from disk, gaps (zeros), partition data from shadow/disk,
    /// and backup GPT at end of disk.
    /// Excluded partitions (by partition number) are written as zeros.
    /// If `stream_status` is provided, it is updated with the current region being streamed.
    /// If `on_vss_fallback` is provided, it is called when VSS returns 0 bytes and we fall back to raw disk.
    pub fn stream_to<S: ImageSink>(
        &self,
        sink: &mut S,
        excluded_partitions: Option<&HashSet<u32>>,
        stream_status: Option<&Mutex<StreamRegionInfo>>,
        mut on_vss_fallback: Option<&mut dyn FnMut(u32)>,
    ) -> Result<u64> {
        let empty = HashSet::new();
        let excluded = excluded_partitions.unwrap_or(&empty);
        let report = |info: StreamRegionInfo| {
            if let Some(st) = stream_status {
                if let Ok(mut s) = st.lock() {
                    *s = info;
                }
            }
        };
        let mut total_written: u64 = 0;
        let mut buffer = vec![0u8; self.buffer_size];
        let disk_length = self.layout.disk_length;
        let gpt_size = GPT_HEADER_SECTORS * SECTOR_SIZE;
        let backup_start = disk_length.saturating_sub(gpt_size);

        let mut pos = 0u64;

        // Read and verify sector 0 (MBR) once - reuse this data for first chunk
        // to avoid any re-read issues (e.g. handle/offset quirks on second read)
        let n0 = crate::disk::read_sectors(self.disk_handle, 0, &mut buffer[..512])?;
        if n0 < 512 {
            return Err(crate::error::DiskCloneError::Other(
                "Failed to read sector 0".to_string(),
            ));
        }
        let mbr_sig = buffer[510] as u16 | ((buffer[511] as u16) << 8);
        if mbr_sig != 0xAA55 {
            return Err(crate::error::DiskCloneError::Other(format!(
                "Disk sector 0 invalid MBR (0x{:04x}), cannot proceed",
                mbr_sig
            )));
        }

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
                report(StreamRegionInfo {
                    partition_num: None,
                    source_type: StreamSourceType::PrimaryGpt,
                    path: None,
                });
                let n = if pos == 0 {
                    // Reuse sector 0 already read for MBR check; only read sectors 1..34
                    let rest = (gpt_size - 512) as usize;
                    let nr = read_sectors(self.disk_handle, 512, &mut buffer[512..512 + rest])?;
                    512 + nr
                } else {
                    read_sectors(self.disk_handle, pos, &mut buffer[..to_write])?
                };
                if n == 0 && to_write > 0 {
                    return Err(crate::error::DiskCloneError::Other(format!(
                        "Zero-byte read at offset {} (primary GPT)",
                        pos
                    )));
                }
                let written = sink.write(&buffer[..n])?;
                total_written += written as u64;
                pos += written as u64;
                continue;
            }

            // Backup GPT: [backup_start, disk_length)
            if pos >= backup_start {
                report(StreamRegionInfo {
                    partition_num: None,
                    source_type: StreamSourceType::BackupGpt,
                    path: None,
                });
                let n = read_sectors(self.disk_handle, pos, &mut buffer[..to_write])?;
                if n == 0 && to_write > 0 {
                    return Err(crate::error::DiskCloneError::Other(format!(
                        "Zero-byte read at offset {} (backup GPT)",
                        pos
                    )));
                }
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
                if excluded.contains(&partition.partition_number) {
                    // Excluded: treat as gap, write zeros
                    if pos >= partition.starting_offset
                        && pos < partition.starting_offset + partition.partition_length
                    {
                        report(StreamRegionInfo {
                            partition_num: Some(partition.partition_number),
                            source_type: StreamSourceType::PartitionExcluded,
                            path: None,
                        });
                        buffer[..to_write].fill(0);
                        let written = sink.write(&buffer[..to_write])?;
                        total_written += written as u64;
                        pos += written as u64;
                        in_partition = true;
                        break;
                    }
                    continue;
                }
                let part_start = partition.starting_offset;
                let part_end = partition.starting_offset + partition.partition_length;

                if pos < part_start || pos >= part_end {
                    continue;
                }

                let (read_handle, read_path, part_offset): (HANDLE, Option<String>, u64) =
                    if let Some(shadow_path) = self.shadow_map.get(&partition.starting_offset) {
                        let path = if shadow_path.starts_with(r"\\?\") {
                            format!(r"\\.\{}", &shadow_path[4..])
                        } else if shadow_path.starts_with(r"\\.\") {
                            shadow_path.clone()
                        } else {
                            format!(r"\\.\GLOBALROOT\Device\{}", shadow_path)
                        };
                        let handle = open_volume_raw(&path)?;
                        (handle, Some(path), 0)
                    } else {
                        let path = format!(r"\\.\PhysicalDrive{}", self.disk_number);
                        (self.disk_handle, Some(path), partition.starting_offset)
                    };

                report(StreamRegionInfo {
                    partition_num: Some(partition.partition_number),
                    source_type: if read_handle == self.disk_handle {
                        StreamSourceType::PartitionRaw
                    } else {
                        StreamSourceType::PartitionVss
                    },
                    path: read_path.clone(),
                });

                let read_offset = part_offset + (pos - part_start);

                let mut n = read_sectors(read_handle, read_offset, &mut buffer[..to_write])?;

                if n == 0 && to_write > 0 && read_handle != self.disk_handle {
                    n = read_sectors(read_handle, read_offset, &mut buffer[..to_write])?;
                }
                if n == 0 && to_write > 0 && read_handle != self.disk_handle {
                    unsafe { winapi::um::handleapi::CloseHandle(read_handle) };
                    n = read_sectors(
                        self.disk_handle,
                        partition.starting_offset + (pos - part_start),
                        &mut buffer[..to_write],
                    )?;
                    if n > 0 {
                        if let Some(ref mut f) = on_vss_fallback {
                            f(partition.partition_number);
                        }
                        report(StreamRegionInfo {
                            partition_num: Some(partition.partition_number),
                            source_type: StreamSourceType::PartitionRaw,
                            path: Some(format!(r"\\.\PhysicalDrive{}", self.disk_number)),
                        });
                    }
                }

                if n == 0 && to_write > 0 {
                    if read_handle != self.disk_handle {
                        unsafe { winapi::um::handleapi::CloseHandle(read_handle) };
                    }
                    return Err(crate::error::DiskCloneError::Other(format!(
                        "Zero-byte read at disk offset {} (partition {}, volume offset {}). VSS retry and raw fallback both failed.",
                        pos, partition.partition_number, read_offset
                    )));
                }
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
                report(StreamRegionInfo {
                    partition_num: None,
                    source_type: StreamSourceType::Gap,
                    path: None,
                });
                buffer[..to_write].fill(0);
                let written = sink.write(&buffer[..to_write])?;
                total_written += written as u64;
                pos += written as u64;
            }
        }

        sink.flush()?;

        if total_written != disk_length {
            return Err(crate::error::DiskCloneError::Other(format!(
                "Incomplete clone: wrote {} bytes ({} GB), expected {} bytes ({} GB)",
                total_written,
                total_written / (1024 * 1024 * 1024),
                disk_length,
                disk_length / (1024 * 1024 * 1024)
            )));
        }
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
