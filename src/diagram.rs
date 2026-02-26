//! Disk layout diagram for preview and confirmation.

use crate::disk::{DiskLayout, SECTOR_SIZE};
use std::collections::HashMap;

/// GPT header size in sectors.
const GPT_HEADER_SECTORS: u64 = 34;

/// A region in the disk image with its data source.
#[derive(Debug, Clone)]
pub struct DiagramRegion {
    pub start: u64,
    pub end: u64,
    pub label: String,
    pub source: RegionSource,
}

/// Where the data for a region comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum RegionSource {
    /// Primary GPT (protective MBR + header + partition table) from raw disk.
    GptPrimary,
    /// Backup GPT at end of disk, from raw disk.
    GptBackup,
    /// Unallocated gap, filled with zeros.
    Gap,
    /// Partition data from VSS shadow copy (consistent snapshot).
    PartitionShadow { partition_num: u32 },
    /// Partition data from raw disk (e.g. MSR, no volume).
    PartitionRaw { partition_num: u32 },
}

/// Builds diagram regions from layout and shadow map.
/// The shadow map keys are partition starting offsets.
pub fn build_diagram_regions(
    layout: &DiskLayout,
    shadow_map: &HashMap<u64, String>,
) -> Vec<DiagramRegion> {
    let mut regions = Vec::new();
    let disk_length = layout.disk_length;
    let gpt_size = GPT_HEADER_SECTORS * SECTOR_SIZE;
    let backup_start = disk_length.saturating_sub(gpt_size);

    // Primary GPT
    regions.push(DiagramRegion {
        start: 0,
        end: gpt_size,
        label: "Primary GPT (MBR + header + partition table)".to_string(),
        source: RegionSource::GptPrimary,
    });
    let mut pos = gpt_size;

    // Sort partitions by offset for ordered iteration
    let mut parts: Vec<_> = layout
        .partitions
        .iter()
        .filter(|p| p.is_used && p.partition_length > 0)
        .collect();
    parts.sort_by_key(|p| p.starting_offset);

    for part in parts {
        let part_start = part.starting_offset;
        let part_end = part.starting_offset + part.partition_length;

        // Gap before partition
        if pos < part_start {
            let gap_end = part_start.min(backup_start);
            if gap_end > pos {
                regions.push(DiagramRegion {
                    start: pos,
                    end: gap_end,
                    label: format!("Gap ({:.1} MB)", (gap_end - pos) as f64 / 1024.0 / 1024.0),
                    source: RegionSource::Gap,
                });
                pos = gap_end;
            }
        }

        // Partition
        if part_start < backup_start {
            let has_shadow = shadow_map.contains_key(&part.starting_offset)
                || shadow_map.keys().any(|&off| off >= part_start && off < part_end);
            let source = if has_shadow {
                RegionSource::PartitionShadow {
                    partition_num: part.partition_number,
                }
            } else {
                RegionSource::PartitionRaw {
                    partition_num: part.partition_number,
                }
            };
            let size_mb = part.partition_length as f64 / 1024.0 / 1024.0;
            let src_str = if has_shadow { "VSS shadow" } else { "Raw disk" };
            regions.push(DiagramRegion {
                start: part_start,
                end: part_end,
                label: format!(
                    "Partition {} ({:.1} MB) - {}",
                    part.partition_number, size_mb, src_str
                ),
                source,
            });
            pos = part_end;
        }
    }

    // Gap before backup GPT
    if pos < backup_start {
        regions.push(DiagramRegion {
            start: pos,
            end: backup_start,
            label: format!(
                "Gap ({:.1} MB)",
                (backup_start - pos) as f64 / 1024.0 / 1024.0
            ),
            source: RegionSource::Gap,
        });
    }

    // Backup GPT
    if backup_start < disk_length {
        regions.push(DiagramRegion {
            start: backup_start,
            end: disk_length,
            label: "Backup GPT (partition table copy)".to_string(),
            source: RegionSource::GptBackup,
        });
    }

    regions
}

/// Formats the diagram as ASCII art for terminal output.
pub fn format_diagram_ascii(regions: &[DiagramRegion], disk_length: u64) -> String {
    let width = 60u64;
    let mut lines = Vec::new();

    lines.push("SOURCE DISK (PhysicalDrive)                    OUTPUT IMAGE".to_string());
    lines.push("".to_string());

    for region in regions {
        let start_pct = (region.start as f64 / disk_length as f64) * 100.0;
        let end_pct = (region.end as f64 / disk_length as f64) * 100.0;
        let bar_start = ((start_pct / 100.0) * width as f64) as u64;
        let bar_end = ((end_pct / 100.0) * width as f64).ceil() as u64;
        let bar_len = (bar_end - bar_start).max(1);

        let (ch, desc) = match &region.source {
            RegionSource::GptPrimary => ('G', "GPT primary".to_string()),
            RegionSource::GptBackup => ('B', "GPT backup".to_string()),
            RegionSource::Gap => ('.', "zeros".to_string()),
            RegionSource::PartitionShadow { partition_num } => {
                ('S', format!("Part {} (VSS)", partition_num))
            }
            RegionSource::PartitionRaw { partition_num } => {
                ('R', format!("Part {} (raw)", partition_num))
            }
        };

        let bar: String = (0..width)
            .map(|i| {
                if i >= bar_start && i < bar_end {
                    ch
                } else {
                    ' '
                }
            })
            .collect();

        lines.push(format!("  [{}]  {}", bar, desc));
    }

    lines.push("".to_string());
    lines.push("Legend: G=Primary GPT  B=Backup GPT  .=Gap (zeros)  S=VSS shadow  R=Raw disk".to_string());
    lines.push(format!(
        "Total: {:.1} GB ({} bytes)",
        disk_length as f64 / 1024.0 / 1024.0 / 1024.0,
        disk_length
    ));

    lines.join("\n")
}
