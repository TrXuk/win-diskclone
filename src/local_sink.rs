//! Local output sinks for disk image.

use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use winapi::ctypes::c_void;
use winapi::shared::minwindef::DWORD;
use winapi::um::fileapi::{CreateFileW, FlushFileBuffers, WriteFile, OPEN_EXISTING};
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
// CREATE_ALWAYS = 2 (create new or overwrite existing)
const CREATE_ALWAYS: DWORD = 2;
use winapi::um::winnt::{FILE_ATTRIBUTE_NORMAL, FILE_SHARE_WRITE, GENERIC_WRITE};

use widestring::U16CString;

use crate::error::Result;

/// Wraps an ImageSink to report progress via shared atomic counters.
/// Use `bytes_written` (Arc<AtomicU64>) for progress display.
pub struct ProgressSink<S: ImageSink> {
    inner: S,
    bytes_written: std::sync::Arc<AtomicU64>,
    #[allow(dead_code)]
    total_bytes: u64,
}

impl<S: ImageSink> ProgressSink<S> {
    #[allow(dead_code)]
    pub fn new(inner: S, bytes_written: std::sync::Arc<AtomicU64>, total_bytes: u64) -> Self {
        Self {
            inner,
            bytes_written,
            total_bytes,
        }
    }
}

impl<S: ImageSink> ImageSink for ProgressSink<S> {
    fn write(&mut self, data: &[u8]) -> Result<usize> {
        let n = self.inner.write(data)?;
        self.bytes_written.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

/// Trait for writing disk image data.
pub trait ImageSink: Send {
    /// Writes a chunk of data. Returns number of bytes written.
    fn write(&mut self, data: &[u8]) -> Result<usize>;
    /// Flushes any buffered data.
    fn flush(&mut self) -> Result<()>;
}

/// Writes disk image to a local file.
/// Uses raw Windows CreateFile/WriteFile to avoid std::fs buffering issues.
pub struct FileSink {
    handle: winapi::um::winnt::HANDLE,
}

// SAFETY: FileSink is used from a single thread during streaming
unsafe impl Send for FileSink {}

impl FileSink {
    pub fn new(path: &str) -> Result<Self> {
        let path_win = path.replace('/', "\\");
        let path_wide = U16CString::from_str(&path_win)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let handle = unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                GENERIC_WRITE,
                0,
                ptr::null_mut(),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }

        Ok(Self { handle })
    }
}

impl ImageSink for FileSink {
    fn write(&mut self, data: &[u8]) -> Result<usize> {
        let mut bytes_written: DWORD = 0;
        let ok = unsafe {
            WriteFile(
                self.handle,
                data.as_ptr() as *const c_void,
                data.len() as DWORD,
                &mut bytes_written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if bytes_written as usize != data.len() {
            return Err(crate::error::DiskCloneError::Other(format!(
                "Short write: requested {} bytes, wrote {}",
                data.len(),
                bytes_written
            )));
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> Result<()> {
        if unsafe { FlushFileBuffers(self.handle) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.handle);
        }
    }
}

/// Writes disk image directly to a physical disk.
/// Requires sector-aligned writes (512 bytes).
#[repr(transparent)]
pub struct LocalDiskSink {
    handle: winapi::um::winnt::HANDLE,
}

// SAFETY: LocalDiskSink is used from a single thread only during streaming
unsafe impl Send for LocalDiskSink {}

impl LocalDiskSink {
    pub fn new(disk_number: u32) -> Result<Self> {
        let path = U16CString::from_str(&format!(r"\\.\PhysicalDrive{}", disk_number))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }

        Ok(Self { handle })
    }
}

impl ImageSink for LocalDiskSink {
    fn write(&mut self, data: &[u8]) -> Result<usize> {
        let mut total_written: usize = 0;
        while total_written < data.len() {
            let mut bytes_written: DWORD = 0;
            let ok = unsafe {
                WriteFile(
                    self.handle,
                    data[total_written..].as_ptr() as *const c_void,
                    (data.len() - total_written) as DWORD,
                    &mut bytes_written,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            total_written += bytes_written as usize;
            if bytes_written == 0 {
                break; // avoid infinite loop
            }
        }
        Ok(total_written)
    }

    fn flush(&mut self) -> Result<()> {
        unsafe {
            winapi::um::fileapi::FlushFileBuffers(self.handle);
        }
        Ok(())
    }
}

impl Drop for LocalDiskSink {
    fn drop(&mut self) {
        unsafe {
            winapi::um::handleapi::CloseHandle(self.handle);
        }
    }
}
