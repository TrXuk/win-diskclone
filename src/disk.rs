//! Raw disk access and partition layout discovery on Windows.

use std::io;
use std::ptr;

use winapi::ctypes::c_void;
use winapi::shared::minwindef::DWORD;
use winapi::um::fileapi::{CreateFileW, OPEN_EXISTING, SetFilePointerEx};
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::winbase::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING, FILE_BEGIN};
use winapi::um::winioctl::{
    GET_LENGTH_INFORMATION, DRIVE_LAYOUT_INFORMATION_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
    IOCTL_DISK_GET_LENGTH_INFO, PARTITION_INFORMATION_EX,
};
use winapi::um::winnt::{
    FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, GENERIC_READ, GENERIC_WRITE, HANDLE,
};

use widestring::U16CString;

use crate::error::Result;

/// Sector size for disk I/O alignment.
pub const SECTOR_SIZE: u64 = 512;

/// Maximum partitions we support (GPT allows 128).
const MAX_PARTITIONS: usize = 128;

/// Size of PARTITION_INFORMATION_EX structure.
const PARTITION_INFO_SIZE: usize = std::mem::size_of::<PARTITION_INFORMATION_EX>();

/// Buffer size for drive layout (header + max partitions).
const LAYOUT_BUFFER_SIZE: usize =
    std::mem::size_of::<DRIVE_LAYOUT_INFORMATION_EX>() + (MAX_PARTITIONS - 1) * PARTITION_INFO_SIZE;

/// Information about a single partition.
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub partition_number: u32,
    pub starting_offset: u64,
    pub partition_length: u64,
    pub is_used: bool,
    pub volume_guid: Option<String>,
}

/// Disk layout containing partition information.
#[derive(Debug, Clone)]
pub struct DiskLayout {
    pub disk_length: u64,
    pub partition_count: u32,
    pub partitions: Vec<PartitionInfo>,
}

/// Information about a physical disk for UI selection.
#[derive(Debug, Clone)]
pub struct PhysicalDiskInfo {
    pub disk_number: u32,
    pub size_bytes: u64,
}

/// Lists physical disks (0..15) that can be opened. Returns disk number and size.
pub fn list_physical_disks() -> Result<Vec<PhysicalDiskInfo>> {
    let mut disks = Vec::new();
    for n in 0..16u32 {
        if let Ok(handle) = open_physical_disk(n) {
            if let Ok(size) = get_disk_length(handle) {
                disks.push(PhysicalDiskInfo {
                    disk_number: n,
                    size_bytes: size,
                });
            }
            unsafe {
                winapi::um::handleapi::CloseHandle(handle);
            }
        }
    }
    Ok(disks)
}

/// Opens a physical disk for read access.
pub fn open_physical_disk(disk_number: u32) -> Result<HANDLE> {
    let path = U16CString::from_str(&format!(r"\\.\PhysicalDrive{}", disk_number))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error().into());
    }

    Ok(handle)
}

/// Opens a physical disk for write access (e.g., for cloning to local disk).
pub fn open_physical_disk_write(disk_number: u32) -> Result<HANDLE> {
    let path = U16CString::from_str(&format!(r"\\.\PhysicalDrive{}", disk_number))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ,
            ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error().into());
    }

    Ok(handle)
}

/// Gets the disk length (total size in bytes).
pub fn get_disk_length(handle: HANDLE) -> Result<u64> {
    let mut info = GET_LENGTH_INFORMATION {
        Length: unsafe { std::mem::zeroed() },
    };
    let mut bytes_returned: DWORD = 0;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_LENGTH_INFO,
            ptr::null_mut(),
            0,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<GET_LENGTH_INFORMATION>() as DWORD,
            &mut bytes_returned,
            ptr::null_mut(),
        )
    };

    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }

    Ok(unsafe { *info.Length.QuadPart() } as u64)
}

/// Opens a disk, gets its layout, and closes the handle. Convenience for one-off layout queries.
pub fn get_disk_layout_from_disk(disk_number: u32) -> Result<DiskLayout> {
    let handle = open_physical_disk(disk_number)?;
    let layout = get_disk_layout(handle)?;
    unsafe {
        winapi::um::handleapi::CloseHandle(handle);
    }
    Ok(layout)
}

/// Gets the partition layout of a disk.
pub fn get_disk_layout(handle: HANDLE) -> Result<DiskLayout> {
    let disk_length = get_disk_length(handle)?;

    let mut buffer = vec![0u8; LAYOUT_BUFFER_SIZE];
    let mut bytes_returned: DWORD = 0;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
            ptr::null_mut(),
            0,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as DWORD,
            &mut bytes_returned,
            ptr::null_mut(),
        )
    };

    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }

    let layout = unsafe { &*(buffer.as_ptr() as *const DRIVE_LAYOUT_INFORMATION_EX) };
    let partition_count = layout.PartitionCount as usize;

    let mut partitions = Vec::with_capacity(partition_count);

    for i in 0..partition_count {
        let part = unsafe {
            &*((&layout.PartitionEntry as *const PARTITION_INFORMATION_EX as *const u8)
                .add(i * PARTITION_INFO_SIZE) as *const PARTITION_INFORMATION_EX)
        };

        let partition_length = unsafe { *part.PartitionLength.QuadPart() } as u64;
        let starting_offset = unsafe { *part.StartingOffset.QuadPart() } as u64;
        // Partition is "used" if it has non-zero size. RewritePartition is for write ops, not layout.
        // Empty GPT/MBR entries have PartitionLength = 0.
        let is_used = partition_length > 0;

        let volume_guid = if is_used && part.PartitionStyle == 1 {
            // GPT - we could extract GUID from part.u.Gpt, but for mapping we use
            // volume extent lookup elsewhere
            None
        } else {
            None
        };

        partitions.push(PartitionInfo {
            partition_number: part.PartitionNumber,
            starting_offset,
            partition_length,
            is_used,
            volume_guid,
        });
    }

    Ok(DiskLayout {
        disk_length,
        partition_count: partition_count as u32,
        partitions,
    })
}

/// Reads sectors from a handle at the given offset.
/// Offset and size must be sector-aligned (512 bytes).
pub fn read_sectors(handle: HANDLE, offset: u64, buffer: &mut [u8]) -> Result<usize> {
    use winapi::um::fileapi::ReadFile;
    use winapi::um::winnt::LARGE_INTEGER;

    let mut bytes_read: DWORD = 0;

    let mut li: LARGE_INTEGER = unsafe { std::mem::zeroed() };
    unsafe { *li.QuadPart_mut() = offset as i64 };

    let ok = unsafe {
        SetFilePointerEx(handle, li, ptr::null_mut(), FILE_BEGIN)
    };

    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }

    let ok = unsafe {
        ReadFile(
            handle,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as DWORD,
            &mut bytes_read,
            ptr::null_mut(),
        )
    };

    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }

    Ok(bytes_read as usize)
}

/// Opens a volume or shadow copy for raw read access.
/// Path should be like `\\.\GLOBALROOT\Device\HarddiskVolumeShadowCopy12` or `\\.\C:`.
/// Uses FILE_FLAG_BACKUP_SEMANTICS for shadow copies (required for VSS device access).
pub fn open_volume_raw(path: &str) -> Result<HANDLE> {
    let path_win = path.replace('/', "\\");
    let path_wide = U16CString::from_str(&path_win)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    // Shadow copy devices need FILE_FLAG_BACKUP_SEMANTICS for CreateFile to succeed
    let is_shadow = path_win.contains("ShadowCopy") || path_win.contains("shadowcopy");
    let flags = if is_shadow {
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_BACKUP_SEMANTICS
    } else {
        FILE_FLAG_NO_BUFFERING
    };

    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            ptr::null_mut(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error().into());
    }

    Ok(handle)
}
