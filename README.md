# diskclone

Windows bootable disk clone via VSS (Volume Shadow Copy Service).

**Author:** [Matt - Vim and Tonic](https://www.youtube.com/@VimAndTonic) Creates a consistent snapshot of an entire OS disk (including EFI, bootloader, recovery partitions) and streams it to a file, local disk, or remote system via SSH.

**Warning:**
This code comes with *absolutely no warranty or expectation of sane results* it needs admin access to your disks, please test first and have backups if you plan to use this.

**Requires Windows 10/11 and Administrator privileges.** Run Command Prompt or PowerShell as Administrator.

## Features

- **VSS snapshot**: Creates a consistent point-in-time copy while the system is running
- **Full disk image**: Includes GPT header, EFI system partition, MSR, main OS partition, recovery
- **Bootable output**: The cloned image is fully bootable at the destination
- **No external runtime deps**: Statically linked (libssh2 uses Windows CNG for crypto)
- **Multiple output modes**: Local file, local physical disk, or SSH stream

## Build

### Option 1: Docker (cross-compile from Linux/macOS)

No Windows or Rust installation needed. Builds `diskclone.exe` in a container:

```bash
./build-docker.sh
```

Or manually:

```bash
docker build -t diskclone-builder .
docker run --rm -v $(pwd)/target:/app/target -v $(pwd):/app:ro -w /app diskclone-builder \
  cargo build --release --target x86_64-pc-windows-gnu
```

Binary: `target/x86_64-pc-windows-gnu/release/diskclone.exe`

### Option 2: Native Windows build

Requires Rust toolchain and Windows target:

```bash
# Install Rust (if needed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Add Windows target
rustup target add x86_64-pc-windows-msvc

# Build (on Windows)
cargo build --release
```

### Option 3: Cross-compile with `cross` (Linux/macOS)

If you have [cross](https://github.com/cross-rs/cross) installed:

```bash
cross build --release --target x86_64-pc-windows-gnu
```

For a fully static binary on Windows, the `.cargo/config.toml` enables `crt-static` for the MSVC target and static linking for the GNU target.

## Usage

```
diskclone [OPTIONS] --disk N (--output FILE | --target-disk N | --ssh USER@HOST [--remote-path PATH])

Options:
  --disk N           Source physical drive (0, 1, 2, ...)
  --output FILE      Write to local file
  --target-disk N    Write to local physical drive
  --ssh USER@HOST   Stream via SSH
  --remote-path PATH Remote path (e.g. /dev/sdb or /backup/disk.img) [default: /tmp/diskclone.img]
  --buffer-size N   Read buffer size in MB [default: 16]
```

### Examples

```bash
# Clone disk 0 to a file
diskclone --disk 0 --output C:\backup\disk.img

# Clone disk 0 to physical drive 1 (local)
diskclone --disk 0 --target-disk 1

# Stream to remote Linux server via SSH
diskclone --disk 0 --ssh admin@backup-server --remote-path /dev/sdb

# Stream to remote file
diskclone --disk 0 --ssh admin@backup-server --remote-path /backup/disk.img
```

### SSH Authentication

Uses SSH agent (`ssh-add`) or default SSH key (`~/.ssh/id_rsa`). Ensure your key is loaded before running:

```bash
ssh-add ~/.ssh/id_rsa
```

For block device writes on the remote, the SSH user typically needs sudo access to write to `/dev/sdX`.

## How It Works

1. **Disk discovery**: Opens `\\.\PhysicalDriveN`, reads GPT partition layout via `IOCTL_DISK_GET_DRIVE_LAYOUT_EX`
2. **Volume mapping**: Enumerates volumes on the disk via `FindFirstVolume`/`FindNextVolume`, maps to partitions using `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`
3. **VSS snapshot**: Creates shadow copy of all volumes on the disk via Volume Shadow Copy Service
4. **Image assembly**: Streams GPT header (first 34 sectors) from raw disk, then each partition from shadow copy (for volumes) or raw disk (for MSR)
5. **Output**: Writes to file, physical disk, or SSH channel

## Author

**[Matt - Vim and Tonic](https://www.youtube.com/@VimAndTonic)**

## License
Apache-2.0
