//! Volume Shadow Copy Service integration for creating consistent disk snapshots.

use std::collections::HashMap;
use std::ptr;

use volume_shadow_copy::vsbackup::BackupComponents;
use volume_shadow_copy::vss::{BackupType, SnapshotContext, VssAsync};
use volume_shadow_copy::VSS_ID;
use widestring::U16CString;

use winapi::ctypes::c_void;
use winapi::shared::minwindef::DWORD;
use winapi::um::fileapi::{CreateFileW, OPEN_EXISTING};
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::winioctl::IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS;
use winapi::um::winnt::{FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, GENERIC_READ};

use crate::disk::PartitionInfo;
use crate::error::{DiskCloneError, Result};

/// VSS snapshot guard - ensures proper cleanup when dropped.
pub struct VssSnapshot {
    backup_components: Option<BackupComponents>,
    snapshot_ids: Vec<VSS_ID>,
}

impl VssSnapshot {
    /// Creates a VSS snapshot of all volumes on the given disk.
    /// Returns a mapping from partition starting offset to shadow copy device path.
    pub fn create_for_disk(disk_number: u32, _partition_infos: &[PartitionInfo]) -> Result<Self> {
        volume_shadow_copy::initialize_com()
            .map_err(|e| DiskCloneError::Vss(format!("COM init: {:?}", e)))?;

        let volumes_on_disk = get_volumes_on_disk(disk_number)?;

        if volumes_on_disk.is_empty() {
            return Err(DiskCloneError::Other(
                "No volumes found on disk - cannot create shadow copy".to_string(),
            ));
        }

        let backup_comp = BackupComponents::new()
            .map_err(|e| DiskCloneError::Vss(format!("BackupComponents::new: {:?}", e)))?;

        backup_comp
            .initialize_for_backup(None)
            .map_err(|e| DiskCloneError::Vss(format!("initialize_for_backup: {:?}", e)))?;

        backup_comp
            .set_context(SnapshotContext::Backup, Default::default())
            .map_err(|e| DiskCloneError::Vss(format!("set_context: {:?}", e)))?;

        backup_comp
            .set_backup_state(false, false, BackupType::Copy, false)
            .map_err(|e| DiskCloneError::Vss(format!("set_backup_state: {:?}", e)))?;

        let gather = backup_comp
            .gather_writer_metadata()
            .map_err(|e| DiskCloneError::Vss(format!("gather_writer_metadata: {:?}", e)))?;
        wait_vss_async(gather)?;

        let _snapshot_set_id = backup_comp
            .start_snapshot_set()
            .map_err(|e| DiskCloneError::Vss(format!("start_snapshot_set: {:?}", e)))?;

        let mut snapshot_ids = Vec::new();
        for volume_name in &volumes_on_disk {
            let volume_owned = U16CString::from_str(volume_name)
                .map_err(|e| DiskCloneError::Other(e.to_string()))?;
            let volume_wide = volume_owned.as_ucstr();
            let is_supported = backup_comp
                .is_volume_supported(None, volume_wide)
                .map_err(|e| DiskCloneError::Vss(format!("is_volume_supported: {:?}", e)))?;
            if is_supported {
                let snapshot_id = backup_comp
                    .add_to_snapshot_set(volume_wide, None)
                    .map_err(|e| DiskCloneError::Vss(format!("add_to_snapshot_set: {:?}", e)))?;
                snapshot_ids.push(snapshot_id);
            }
        }

        if snapshot_ids.is_empty() {
            return Err(DiskCloneError::Other(
                "No volumes could be added to snapshot set".to_string(),
            ));
        }

        let prepare = backup_comp
            .prepare_for_backup()
            .map_err(|e| DiskCloneError::Vss(format!("prepare_for_backup: {:?}", e)))?;
        wait_vss_async(prepare)?;

        let do_snap = backup_comp
            .do_snapshot_set()
            .map_err(|e| DiskCloneError::Vss(format!("do_snapshot_set: {:?}", e)))?;
        wait_vss_async(do_snap)?;

        Ok(VssSnapshot {
            backup_components: Some(backup_comp),
            snapshot_ids,
        })
    }

    /// Gets the shadow copy device path for a volume by its original volume name.
    /// Volume names can differ in prefix (\\.\ vs \\?\) between FindFirstVolume and VSS;
    /// we normalize by comparing the Volume{guid} part only.
    pub fn get_shadow_path(&self, volume_name: &str) -> Option<String> {
        let backup_comp = self.backup_components.as_ref()?;
        let volume_key = normalize_volume_name_for_match(volume_name);
        for &snapshot_id in &self.snapshot_ids {
            if let Ok(props) = backup_comp.get_snapshot_properties(snapshot_id) {
                let orig = props.original_volume_name().to_string_lossy();
                if normalize_volume_name_for_match(&orig) == volume_key {
                    let s = props.snapshot_device_object();
                    return Some(s.to_string_lossy().trim_end_matches('\\').to_string());
                }
            }
        }
        None
    }

    /// Builds a mapping from partition starting offset to shadow path (if the partition has one).
    pub fn build_partition_shadow_map(
        &self,
        _partitions: &[PartitionInfo],
        disk_number: u32,
    ) -> Result<HashMap<u64, String>> {
        let volumes_on_disk = get_volumes_on_disk(disk_number)?;
        let mut offset_to_path = HashMap::new();

        for volume_name in &volumes_on_disk {
            if let Some(shadow_path) = self.get_shadow_path(volume_name) {
                if let Some(offset) = get_volume_disk_offset(volume_name)? {
                    offset_to_path.insert(offset, shadow_path);
                }
            }
        }

        Ok(offset_to_path)
    }

    /// Finishes the backup and releases VSS resources.
    pub fn finish(mut self) -> Result<()> {
        if let Some(backup_comp) = self.backup_components.take() {
            let complete = backup_comp
                .backup_complete()
                .map_err(|e| DiskCloneError::Vss(format!("backup_complete: {:?}", e)))?;
            let _ = wait_vss_async(complete);
        }
        Ok(())
    }
}

impl Drop for VssSnapshot {
    fn drop(&mut self) {
        if let Some(backup_comp) = self.backup_components.take() {
            let _ = backup_comp.backup_complete();
        }
    }
}

/// Normalizes a volume name for comparison. FindFirstVolume returns \\?\Volume{guid}\
/// while VSS GetSnapshotProperties may return \\.\Volume{guid}\ - we compare by
/// the Volume{guid} part only (case-insensitive).
fn normalize_volume_name_for_match(name: &str) -> String {
    let s = name.trim_matches(|c| c == '\\' || c == '/');
    if let Some(idx) = s.to_uppercase().find("VOLUME{") {
        s[idx..].to_uppercase()
    } else {
        s.to_uppercase()
    }
}

fn wait_vss_async<E: From<i32> + std::fmt::Debug>(async_result: VssAsync<E>) -> Result<()> {
    async_result
        .wait(Some(5 * 60 * 1000))
        .map_err(|e| DiskCloneError::Vss(format!("VSS async wait: {:?}", e)))?;
    Ok(())
}

/// Gets volume names (GUID paths) for all volumes on the specified physical disk.
fn get_volumes_on_disk(disk_number: u32) -> Result<Vec<String>> {
    use winapi::um::fileapi::{FindFirstVolumeW, FindNextVolumeW, FindVolumeClose};

    let mut volumes_on_disk = Vec::new();
    let mut volume_name = [0u16; 52];

    let find_handle = unsafe { FindFirstVolumeW(volume_name.as_mut_ptr(), volume_name.len() as DWORD) };

    if find_handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }

    loop {
        let len = volume_name.iter().position(|&c| c == 0).unwrap_or(volume_name.len());
        let name = String::from_utf16_lossy(&volume_name[..len]);
        let name_str = name.trim_end_matches('\\');

        if let Ok(Some((disk, _))) = get_volume_disk_extents(name_str) {
            if disk == disk_number {
                volumes_on_disk.push(format!("{}\\", name_str));
            }
        }

        if unsafe { FindNextVolumeW(find_handle, volume_name.as_mut_ptr(), volume_name.len() as DWORD) } == 0 {
            break;
        }
    }

    unsafe {
        FindVolumeClose(find_handle);
    }

    Ok(volumes_on_disk)
}

/// Gets the disk number and starting offset for a volume.
fn get_volume_disk_offset(volume_name: &str) -> Result<Option<u64>> {
    let (_disk_num, offset) = get_volume_disk_extents(volume_name)?.unwrap_or((0, 0));
    Ok(Some(offset))
}

fn get_volume_disk_extents(volume_name: &str) -> Result<Option<(u32, u64)>> {
    use winapi::um::winioctl::{DISK_EXTENT, VOLUME_DISK_EXTENTS};

    let path = if volume_name.starts_with(r"\\?\") || volume_name.starts_with(r"\\.\") {
        format!(r"\\.\{}", volume_name[4..].trim_matches('\\'))
    } else {
        format!(r"\\.\{}", volume_name.trim_matches('\\'))
    };
    let path_wide = widestring::U16CString::from_str(&path)
        .map_err(|e| DiskCloneError::Other(e.to_string()))?;

    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Ok(None);
    }

    let mut buffer = vec![0u8; std::mem::size_of::<VOLUME_DISK_EXTENTS>() + 64];
    let mut bytes_returned: DWORD = 0;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            ptr::null_mut(),
            0,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as DWORD,
            &mut bytes_returned,
            ptr::null_mut(),
        )
    };

    unsafe { winapi::um::handleapi::CloseHandle(handle) };

    if ok == 0 {
        return Ok(None);
    }

    let extents = unsafe { &*(buffer.as_ptr() as *const VOLUME_DISK_EXTENTS) };
    if extents.NumberOfDiskExtents > 0 {
        let extent = unsafe { &*(&extents.Extents as *const _ as *const DISK_EXTENT) };
        Ok(Some((extent.DiskNumber, unsafe { *extent.StartingOffset.QuadPart() } as u64)))
    } else {
        Ok(None)
    }
}
