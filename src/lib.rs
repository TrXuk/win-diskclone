//! Windows bootable disk clone via VSS shadow copy.
//!
//! Creates a consistent snapshot of an entire OS disk and streams it via SSH,
//! to a local file, or directly to another physical disk.

/// Version string (e.g. "0.1.0+abc1234") from Cargo and git hash at build time.
pub const VERSION: &str = env!("DISKCLONE_VERSION");

/// Short git commit hash at build time.
pub const GIT_HASH: &str = env!("DISKCLONE_GIT_HASH");

pub mod diagram;
pub mod disk;
pub mod error;
pub mod image_builder;
pub mod local_sink;
pub mod ssh_sink;
pub mod vss;

pub use disk::{
    get_disk_layout, get_disk_layout_from_disk, list_physical_disks, open_physical_disk,
    DiskLayout, PartitionInfo, PhysicalDiskInfo, SECTOR_SIZE,
};
pub use error::{DiskCloneError, Result};
pub use diagram::{build_diagram_regions, format_diagram_ascii, DiagramRegion, RegionSource};
pub use image_builder::ImageBuilder;
pub use local_sink::{FileSink, LocalDiskSink, ProgressSink};
pub use ssh_sink::{create_ssh_session, SshSink};
pub use vss::{analyze_snapshot_support, open_shadow_in_explorer, SnapshotAnalysis, VssSnapshot};
