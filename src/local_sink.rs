//! Local output sinks for disk image.

use std::io::Write;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use winapi::ctypes::c_void;
use winapi::shared::minwindef::DWORD;
use winapi::um::fileapi::{CreateFileW, WriteFile};
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::fileapi::OPEN_EXISTING;
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
pub struct FileSink {
    file: std::fs::File,
}

impl FileSink {
    pub fn new(path: &str) -> Result<Self> {
        let file = std::fs::File::create(path)?;
        Ok(Self { file })
    }
}

impl ImageSink for FileSink {
    fn write(&mut self, data: &[u8]) -> Result<usize> {
        Ok(self.file.write(data)?)
    }

    fn flush(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
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
        Ok(bytes_written as usize)
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
