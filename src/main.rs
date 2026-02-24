//! diskclone - Windows bootable disk clone via VSS
//!
//! Clones an entire OS disk (including EFI, bootloader) to a file, local disk, or remote system via SSH.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

use diskclone::{
    DiskCloneError, FileSink, ImageBuilder, LocalDiskSink, SshSink, VssSnapshot,
};

#[derive(Parser, Debug)]
#[command(name = "diskclone")]
#[command(about = "Clone Windows OS disk via VSS shadow copy")]
#[command(version = diskclone::VERSION)]
struct Args {
    /// Source physical drive number (0, 1, 2, ...)
    #[arg(long, value_name = "N")]
    disk: u32,

    /// Write to local file
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Write to local physical drive
    #[arg(long, value_name = "N")]
    target_disk: Option<u32>,

    /// Stream via SSH (user@host)
    #[arg(long, value_name = "USER@HOST")]
    ssh: Option<String>,

    /// Remote path for SSH (e.g. /dev/sdb or /backup/disk.img)
    #[arg(long, value_name = "PATH", requires = "ssh")]
    remote_path: Option<String>,

    /// SSH password (or set DISKCLONE_SSH_PASSWORD env var). Falls back to agent/pubkey if not provided.
    #[arg(long, value_name = "PASSWORD", requires = "ssh")]
    ssh_password: Option<String>,

    /// Read buffer size in MB (default: 16)
    #[arg(long, default_value = "16")]
    buffer_size: usize,
}

fn main() -> Result<(), DiskCloneError> {
    let args = Args::parse();

    let output_count = [args.output.is_some(), args.target_disk.is_some(), args.ssh.is_some()]
        .iter()
        .filter(|&&x| x)
        .count();

    if output_count != 1 {
        eprintln!("Error: Exactly one of --output, --target-disk, or --ssh must be specified");
        std::process::exit(1);
    }

    eprintln!("Creating VSS snapshot of disk {}...", args.disk);
    let start = Instant::now();

    let layout = diskclone::get_disk_layout_from_disk(args.disk)?;

    let vss = VssSnapshot::create_for_disk(args.disk, &layout.partitions)?;
    eprintln!("VSS snapshot created in {:?}", start.elapsed());

    let builder = ImageBuilder::new(args.disk, &vss, args.buffer_size)?;
    let total_size = builder.disk_length();
    eprintln!("Disk size: {} GB", total_size / (1024 * 1024 * 1024));

    let stream_start = Instant::now();
    let mut bytes_written = 0u64;

    if let Some(ref path) = args.output {
        eprintln!("Writing to file: {:?}", path);
        let mut sink = FileSink::new(path.to_str().unwrap())?;
        bytes_written = builder.stream_to(&mut sink)?;
    } else if let Some(target_disk) = args.target_disk {
        eprintln!("Writing to physical drive {}...", target_disk);
        let mut sink = LocalDiskSink::new(target_disk)?;
        bytes_written = builder.stream_to(&mut sink)?;
    } else if let Some(ref ssh_target) = args.ssh {
        let remote_path = args
            .remote_path
            .as_deref()
            .unwrap_or("/tmp/diskclone.img");

        let (user, host) = if let Some((u, h)) = ssh_target.split_once('@') {
            (u, h)
        } else {
            eprintln!("Error: SSH target must be user@host");
            std::process::exit(1);
        };

        eprintln!("Connecting via SSH to {}...", host);
        let password_env = std::env::var("DISKCLONE_SSH_PASSWORD").ok();
        let password = args.ssh_password.as_deref().or(password_env.as_deref());
        let sess = diskclone::create_ssh_session(user, host, password)?;

        eprintln!("Streaming to {} on remote...", remote_path);
        let mut sink = if remote_path.starts_with("/dev/") {
            SshSink::new(&sess, remote_path)?
        } else {
            SshSink::new_cat(&sess, remote_path)?
        };
        bytes_written = builder.stream_to(&mut sink)?;
    }

    vss.finish()?;

    eprintln!(
        "Done. Wrote {} MB in {:?} ({:.1} MB/s)",
        bytes_written / (1024 * 1024),
        stream_start.elapsed(),
        (bytes_written as f64 / 1024.0 / 1024.0) / stream_start.elapsed().as_secs_f64()
    );

    Ok(())
}
