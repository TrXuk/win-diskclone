//! Volume Shadow Copy Service integration for creating consistent disk snapshots.
//!
//! Uses correct Windows API usage per MSDN:
//! - CreateFile for volumes: FILE_FLAG_BACKUP_SEMANTICS, no trailing backslash
//! - Volume enumeration: FindFirstVolumeW / FindNextVolumeW only

use std::collections::HashMap;
use std::ptr;

use volume_shadow_copy::vsbackup::BackupComponents;
use volume_shadow_copy::vss::{BackupType, SnapshotContext, VssAsync};
use volume_shadow_copy::VSS_ID;
use widestring::U16CString;

use winapi::ctypes::c_void;
use winapi::shared::minwindef::DWORD;
use winapi::um::fileapi::{
    CreateFileW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    GetVolumePathNamesForVolumeNameW, OPEN_EXISTING,
};
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;
use winapi::um::winioctl::IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS;
use winapi::um::winnt::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ};

use crate::disk::PartitionInfo;
use crate::error::{DiskCloneError, Result};

const MAX_PATH: usize = 260;

/// Result of analyzing a partition for VSS snapshot support.
#[derive(Debug, Clone)]
pub struct SnapshotAnalysis {
    pub partition_number: u32,
    pub starting_offset: u64,
    pub size_mb: f64,
    pub has_volume: bool,
    pub volume_display: String,
    /// Drive letters (e.g. ["C:"]) for mounted volumes, empty if not mounted.
    pub drive_letters: Vec<String>,
    pub vss_supported: bool,
    pub reason: String,
}

/// Analyzes which partitions on a disk will get VSS snapshots.
/// Call when user selects a disk, before Create snapshot.
/// Returns analysis for each partition (used or not).
pub fn analyze_snapshot_support(disk_number: u32) -> Result<Vec<SnapshotAnalysis>> {
    let layout = crate::disk::get_disk_layout_from_disk(disk_number)?;
    let volumes_on_disk = get_volumes_on_disk(disk_number)?;

    // Build volume offset -> (name, display, drive_letters) map
    let mut volume_info: HashMap<u64, (String, String, Vec<String>)> = HashMap::new();
    for vol in &volumes_on_disk {
        if let Some((_, offset)) = get_volume_disk_extents(vol)? {
            let display = volume_display_name(vol);
            let drive_letters = get_volume_drive_letters(vol).unwrap_or_default();
            volume_info.insert(offset, (vol.clone(), display, drive_letters));
        }
    }

    // Check VSS support for each volume (requires COM + BackupComponents)
    let mut vss_supported: HashMap<u64, (bool, String)> = HashMap::new();
    if !volumes_on_disk.is_empty() {
        volume_shadow_copy::initialize_com()
            .map_err(|e| DiskCloneError::Vss(format!("COM init: {:?}", e)))?;

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

        for vol in &volumes_on_disk {
            if let Some((_, offset)) = get_volume_disk_extents(vol)? {
                let volume_owned = U16CString::from_str(vol)
                    .map_err(|e| DiskCloneError::Other(e.to_string()))?;
                let volume_wide = volume_owned.as_ucstr();
                match backup_comp.is_volume_supported(None, volume_wide) {
                    Ok(true) => {
                        vss_supported.insert(offset, (true, "VSS supported".to_string()));
                    }
                    Ok(false) => {
                        vss_supported.insert(
                            offset,
                            (false, "Volume not supported by VSS writers".to_string()),
                        );
                    }
                    Err(e) => {
                        vss_supported.insert(
                            offset,
                            (false, format!("VSS check failed: {:?}", e)),
                        );
                    }
                }
            }
        }

        let _ = backup_comp.backup_complete();
    }

    // Build analysis for each partition
    let mut results = Vec::new();
    for part in &layout.partitions {
        if !part.is_used || part.partition_length == 0 {
            continue;
        }

        let size_mb = part.partition_length as f64 / 1024.0 / 1024.0;

        // Find matching volume (exact offset or within partition)
        let vol_match = volume_info
            .get(&part.starting_offset)
            .or_else(|| {
                volume_info
                    .keys()
                    .find(|&&off| off >= part.starting_offset && off < part.starting_offset + part.partition_length)
                    .and_then(|off| volume_info.get(off))
            });

        if let Some((_name, display, drive_letters)) = vol_match {
            let offset = part.starting_offset;
            let (supported, reason) = vss_supported
                .get(&offset)
                .or_else(|| {
                    vss_supported
                        .keys()
                        .find(|&&off| off >= part.starting_offset && off < part.starting_offset + part.partition_length)
                        .and_then(|off| vss_supported.get(off))
                })
                .cloned()
                .unwrap_or((false, "Could not determine VSS support".to_string()));

            results.push(SnapshotAnalysis {
                partition_number: part.partition_number,
                starting_offset: part.starting_offset,
                size_mb,
                has_volume: true,
                volume_display: display.clone(),
                drive_letters: drive_letters.to_vec(),
                vss_supported: supported,
                reason,
            });
        } else {
            results.push(SnapshotAnalysis {
                partition_number: part.partition_number,
                starting_offset: part.starting_offset,
                size_mb,
                has_volume: false,
                volume_display: String::new(),
                drive_letters: Vec::new(),
                vss_supported: false,
                reason: "No volume (e.g. MSR) - will use raw disk".to_string(),
            });
        }
    }

    Ok(results)
}

/// Gets drive letters (e.g. ["C:"]) for a volume. Volume name must be \\?\Volume{guid}\
fn get_volume_drive_letters(volume_name: &str) -> Result<Vec<String>> {
    let vol_path = if volume_name.ends_with('\\') {
        volume_name.to_string()
    } else {
        format!("{}\\", volume_name)
    };
    let vol_wide = U16CString::from_str(&vol_path)
        .map_err(|e| DiskCloneError::Other(e.to_string()))?;
    let mut buf = vec![0u16; 256];
    let mut len: DWORD = 0;

    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(
            vol_wide.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as DWORD,
            &mut len,
        )
    };

    if ok == 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(122) {
            // ERROR_INSUFFICIENT_BUFFER - retry with larger buffer
            buf.resize(len as usize, 0);
            let ok2 = unsafe {
                GetVolumePathNamesForVolumeNameW(
                    vol_wide.as_ptr(),
                    buf.as_mut_ptr(),
                    buf.len() as DWORD,
                    &mut len,
                )
            };
            if ok2 == 0 {
                return Err(err.into());
            }
        } else {
            return Err(err.into());
        }
    }

    // Parse null-separated strings: "C:\"<nul>"D:\"<nul><nul>
    let mut letters = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == 0 {
            break;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        let s = String::from_utf16_lossy(&buf[start..i]);
        let s = s.trim_matches(|c| c == '\\' || c == '/');
        if !s.is_empty() {
            if s.len() == 1 {
                letters.push(format!("{}:", s));
            } else {
                letters.push(s.to_string());
            }
        }
        i += 1; // skip null
    }
    Ok(letters)
}

fn volume_display_name(volume_name: &str) -> String {
    let s = volume_name.trim_matches(|c| c == '\\' || c == '/');
    if let Some(idx) = s.to_uppercase().find("VOLUME{") {
        let guid_part = &s[idx..];
        if guid_part.len() > 45 {
            format!("Volume{}...", &guid_part[..42])
        } else {
            format!("Volume{}", guid_part)
        }
    } else {
        s.to_string()
    }
}

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
    /// Uses VSS snapshot properties directly (original_volume_name) to get offsets, avoiding
    /// volume name format mismatches between FindFirstVolume and VSS GetSnapshotProperties.
    pub fn build_partition_shadow_map(
        &self,
        _partitions: &[PartitionInfo],
        disk_number: u32,
    ) -> Result<HashMap<u64, String>> {
        let backup_comp = match &self.backup_components {
            Some(b) => b,
            None => return Ok(HashMap::new()),
        };

        let mut offset_to_path = HashMap::new();

        for &snapshot_id in &self.snapshot_ids {
            if let Ok(props) = backup_comp.get_snapshot_properties(snapshot_id) {
                let orig = props.original_volume_name().to_string_lossy();
                let shadow_path = props
                    .snapshot_device_object()
                    .to_string_lossy()
                    .trim_end_matches('\\')
                    .to_string();

                if let Some((extent_disk, offset)) = get_volume_disk_extents(&orig)? {
                    if extent_disk == disk_number {
                        offset_to_path.insert(offset, shadow_path);
                    }
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

/// Opens a VSS shadow copy in Explorer (read-only browse).
/// Mounts the shadow to a temporary drive letter (Z: down to D:) and opens Explorer.
/// The mount persists until reboot or manual unmount via diskmgmt.msc.
/// Path should be the snapshot device object, e.g. \\.\GLOBALROOT\Device\HarddiskVolumeShadowCopy12
pub fn open_shadow_in_explorer(path: &str) -> Result<()> {
    use std::process::Command;
    use winapi::um::fileapi::{GetDriveTypeW, GetFinalPathNameByHandleW};
    use winapi::um::winbase::SetVolumeMountPointW;
    use winapi::um::winnt::FILE_ATTRIBUTE_NORMAL;

    const VOLUME_NAME_GUID: DWORD = 0x1;

    let path = path.trim();
    let device_path = if path.starts_with(r"\\.\") || path.starts_with(r"\\?\") {
        path.to_string()
    } else {
        format!(r"\\.\GLOBALROOT\Device\{}", path.trim_start_matches('\\'))
    };

    // Ensure path has trailing backslash for volume root access (GetFinalPathNameByHandle)
    let open_path = if device_path.ends_with('\\') {
        device_path.clone()
    } else {
        format!("{}\\", device_path)
    };

    let path_wide =
        U16CString::from_str(&open_path).map_err(|e| DiskCloneError::Other(e.to_string()))?;

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
        return Err(std::io::Error::last_os_error().into());
    }

    // Get volume GUID path (\\?\Volume{guid}\) - use larger buffer for full GUID
    let mut vol_path = [0u16; MAX_PATH + 1];
    let len = unsafe {
        GetFinalPathNameByHandleW(
            handle,
            vol_path.as_mut_ptr(),
            vol_path.len() as DWORD,
            VOLUME_NAME_GUID,
        )
    };

    unsafe {
        winapi::um::handleapi::CloseHandle(handle);
    }

    if len == 0 || len as usize >= vol_path.len() {
        return Err(DiskCloneError::Other(
            "Could not get volume GUID for shadow copy".to_string(),
        ));
    }

    let vol_guid_str = String::from_utf16_lossy(&vol_path[..len as usize]);
    let vol_guid = vol_guid_str.trim_end_matches('\0').trim_end_matches('\\');
    let vol_guid_wide = U16CString::from_str(&format!("{}\\", vol_guid))
        .map_err(|e| DiskCloneError::Other(e.to_string()))?;

    // Find unused drive letter (Z: down to D:)
    let drive = (b'D'..=b'Z')
        .rev()
        .map(|c| format!("{}:\\", c as char))
        .find(|d| {
            let wide = U16CString::from_str(d).unwrap();
            let dt = unsafe { GetDriveTypeW(wide.as_ptr()) };
            dt == winapi::um::winbase::DRIVE_NO_ROOT_DIR
        })
        .ok_or_else(|| {
            DiskCloneError::Other("No unused drive letter available (try Z: through D:)".to_string())
        })?;

    let mount_wide = U16CString::from_str(&drive).map_err(|e| DiskCloneError::Other(e.to_string()))?;

    let ok = unsafe {
        SetVolumeMountPointW(mount_wide.as_ptr(), vol_guid_wide.as_ptr())
    };

    if ok == 0 {
        return Err(DiskCloneError::Other(format!(
            "Failed to mount shadow copy to {}: {}",
            drive,
            std::io::Error::last_os_error()
        )));
    }

    // Open Explorer; user can browse. Mount persists until they close Explorer or reboot.
    Command::new("explorer.exe")
        .arg(&drive)
        .spawn()
        .map_err(|e| DiskCloneError::Other(format!("Failed to open Explorer: {}", e)))?;

    Ok(())
}

fn wait_vss_async<E: From<i32> + std::fmt::Debug>(async_result: VssAsync<E>) -> Result<()> {
    async_result
        .wait(Some(5 * 60 * 1000))
        .map_err(|e| DiskCloneError::Vss(format!("VSS async wait: {:?}", e)))?;
    Ok(())
}

/// Enumerates all volumes via FindFirstVolumeW/FindNextVolumeW.
/// Returns \\?\Volume{GUID}\ paths (with trailing backslash for VSS APIs).
fn enumerate_all_volumes() -> Result<Vec<String>> {
    let mut volumes = Vec::new();
    let mut volume_name = [0u16; MAX_PATH + 1];

    let find_handle = unsafe { FindFirstVolumeW(volume_name.as_mut_ptr(), volume_name.len() as DWORD) };

    if find_handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }

    loop {
        let len = volume_name.iter().position(|&c| c == 0).unwrap_or(volume_name.len());
        let name = String::from_utf16_lossy(&volume_name[..len]);
        // Ensure trailing backslash for VSS API consistency
        let normalized = if name.ends_with('\\') {
            name
        } else {
            format!("{}\\", name)
        };
        volumes.push(normalized);

        if unsafe {
            FindNextVolumeW(find_handle, volume_name.as_mut_ptr(), volume_name.len() as DWORD)
        } == 0
        {
            break;
        }
    }

    unsafe {
        FindVolumeClose(find_handle);
    }

    Ok(volumes)
}

/// Gets volume names (GUID paths) for all volumes on the specified physical disk.
/// Uses FindFirstVolumeW/FindNextVolumeW only; filters by disk via get_volume_disk_extents.
fn get_volumes_on_disk(disk_number: u32) -> Result<Vec<String>> {
    let all = enumerate_all_volumes()?;
    let mut volumes_on_disk = Vec::new();

    for vol in all {
        if let Ok(Some((disk, _))) = get_volume_disk_extents(&vol) {
            if disk == disk_number {
                volumes_on_disk.push(vol);
            }
        }
    }

    Ok(volumes_on_disk)
}

/// Diagnostic: prints VSS/volume enumeration details to stderr. Run on Windows to debug
/// "no volumes" or "raw disk" issues. Usage: diskclone --debug-vss --disk N
pub fn debug_vss_diag(disk_number: u32) -> Result<String> {
    use std::fmt::Write;
    use winapi::um::errhandlingapi::GetLastError;

    let mut out = String::new();

    writeln!(out, "=== VSS Diagnostic for Disk {} ===", disk_number).ok();

    // 1. Disk layout
    let layout = match crate::disk::get_disk_layout_from_disk(disk_number) {
        Ok(l) => l,
        Err(e) => {
            writeln!(out, "get_disk_layout_from_disk FAILED: {:?}", e).ok();
            return Ok(out);
        }
    };
    writeln!(out, "Disk length: {} bytes", layout.disk_length).ok();
    for p in &layout.partitions {
        if p.is_used && p.partition_length > 0 {
            writeln!(
                out,
                "  Partition {}: offset={} length={}",
                p.partition_number, p.starting_offset, p.partition_length
            )
            .ok();
        }
    }

    // 2. Enumerate volumes
    let volumes = match enumerate_all_volumes() {
        Ok(v) => v,
        Err(e) => {
            writeln!(out, "enumerate_all_volumes FAILED: {:?}", e).ok();
            return Ok(out);
        }
    };
    writeln!(out, "Enumerated {} volumes", volumes.len()).ok();

    // 3. For each volume, try get_volume_disk_extents with detailed error
    for (i, vol) in volumes.iter().enumerate() {
        writeln!(out, "\nVolume {}: {}", i + 1, vol).ok();

        let vol_trim = vol.trim();
        let base = if vol_trim.starts_with(r"\\?\") || vol_trim.starts_with(r"\\.\") {
            vol_trim[4..].trim_end_matches('\\').to_string()
        } else {
            vol_trim.trim_start_matches('\\').trim_end_matches('\\').to_string()
        };
        let path = if base.starts_with("Volume{") {
            format!(r"\\?\{}", base)
        } else {
            format!(r"\\.\{}", base)
        };
        writeln!(out, "  CreateFile path: {}", path).ok();

        let path_wide = match widestring::U16CString::from_str(&path) {
            Ok(w) => w,
            Err(e) => {
                writeln!(out, "  U16CString FAILED: {}", e).ok();
                continue;
            }
        };

        let handle = unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            let err = unsafe { GetLastError() };
            writeln!(out, "  CreateFile FAILED, GetLastError={} (0x{:x})", err, err).ok();
            continue;
        }

        let mut buffer = vec![0u8; 256];
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
            let err = unsafe { GetLastError() };
            writeln!(out, "  DeviceIoControl FAILED, GetLastError={} (0x{:x})", err, err).ok();
            continue;
        }

        let extents = unsafe { &*(buffer.as_ptr() as *const winapi::um::winioctl::VOLUME_DISK_EXTENTS) };
        if extents.NumberOfDiskExtents > 0 {
            let extent = unsafe {
                &*(&extents.Extents as *const _ as *const winapi::um::winioctl::DISK_EXTENT)
            };
            let disk = extent.DiskNumber;
            let offset = unsafe { *extent.StartingOffset.QuadPart() } as u64;
            writeln!(out, "  OK: disk={} offset={}", disk, offset).ok();
            if disk == disk_number {
                writeln!(out, "  -> ON TARGET DISK").ok();
            }
        } else {
            writeln!(out, "  No extents").ok();
        }
    }

    // 4. Summary: volumes on our disk
    let on_disk = get_volumes_on_disk(disk_number)?;
    writeln!(out, "\nVolumes on disk {}: {}", disk_number, on_disk.len()).ok();
    for v in &on_disk {
        writeln!(out, "  {}", v).ok();
    }

    Ok(out)
}

/// Gets the disk number and starting offset for a volume.
/// Uses correct CreateFile flags per MSDN: FILE_FLAG_BACKUP_SEMANTICS (required for volumes),
/// FILE_SHARE_READ|WRITE|DELETE, and path WITHOUT trailing backslash (opens volume object).
fn get_volume_disk_extents(volume_name: &str) -> Result<Option<(u32, u64)>> {
    use winapi::um::winioctl::{DISK_EXTENT, VOLUME_DISK_EXTENTS};

    // Extract Volume{guid} from \\?\Volume{guid}\ or \\.\Volume{guid}\
    // Do NOT use trim_matches('\\') - it strips leading \\ and corrupts the string.
    let vol = volume_name.trim();
    let base = if vol.starts_with(r"\\?\") || vol.starts_with(r"\\.\") {
        vol[4..].trim_end_matches('\\').to_string()
    } else {
        vol.trim_start_matches('\\').trim_end_matches('\\').to_string()
    };

    // For CreateFile + IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: path WITHOUT trailing backslash
    let path = if base.starts_with("Volume{") {
        format!(r"\\?\{}", base)
    } else {
        format!(r"\\.\{}", base)
    };

    let path_wide = widestring::U16CString::from_str(&path)
        .map_err(|e| DiskCloneError::Other(e.to_string()))?;

    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
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
