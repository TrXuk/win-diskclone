//! diskclone - Windows bootable disk clone via VSS
//!
//! Clones an entire OS disk (including EFI, bootloader) to a file, local disk, or remote system via SSH.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

use diskclone::{
    format_diagram_ascii, DiskCloneError, FileSink, ImageBuilder, LocalDiskSink, SshSink,
    VssSnapshot,
};

#[derive(Parser, Debug)]
#[command(name = "diskclone")]
#[command(about = "Clone Windows OS disk via VSS shadow copy")]
#[command(version = diskclone::VERSION)]
struct Args {
    /// Source physical drive number (0, 1, 2, ...)
    #[arg(long, value_name = "N")]
    disk: Option<u32>,

    /// Run VSS diagnostic and exit (use with --disk N). Prints volume enumeration and extent lookup details.
    #[arg(long)]
    debug_vss: bool,

    /// Run image builder diagnostic and exit (use with --disk N). Creates snapshot, prints shadow map and GPT verification.
    #[arg(long)]
    debug_image: bool,

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

    if args.debug_vss {
        let disk = args.disk.unwrap_or_else(|| {
            eprintln!("Error: --debug-vss requires --disk N");
            std::process::exit(1);
        });
        let diag = diskclone::debug_vss_diag(disk)?;
        eprintln!("{}", diag);
        return Ok(());
    }

    if args.debug_image {
        let disk = args.disk.unwrap_or_else(|| {
            eprintln!("Error: --debug-image requires --disk N");
            std::process::exit(1);
        });
        let layout = diskclone::get_disk_layout_from_disk(disk)?;
        eprintln!("Creating VSS snapshot for diagnostic...");
        let vss = VssSnapshot::create_for_disk(disk, &layout.partitions)?;
        let builder = ImageBuilder::new(disk, &vss, 16)?;
        let diag = builder.debug_image_diag()?;
        eprintln!("{}", diag);
        vss.finish()?;
        return Ok(());
    }

    let disk = args.disk.unwrap_or_else(|| {
        eprintln!("Error: --disk N is required");
        std::process::exit(1);
    });

    let output_count = [args.output.is_some(), args.target_disk.is_some(), args.ssh.is_some()]
        .iter()
        .filter(|&&x| x)
        .count();

    if output_count != 1 {
        eprintln!("Error: Exactly one of --output, --target-disk, or --ssh must be specified");
        std::process::exit(1);
    }

    eprintln!("Creating VSS snapshot of disk {}...", disk);
    let start = Instant::now();

    let layout = diskclone::get_disk_layout_from_disk(disk)?;

    let vss = VssSnapshot::create_for_disk(disk, &layout.partitions)?;
    eprintln!("VSS snapshot created in {:?}", start.elapsed());

    let builder = ImageBuilder::new(disk, &vss, args.buffer_size)?;
    let total_size = builder.disk_length();
    eprintln!("Disk size: {} GB", total_size / (1024 * 1024 * 1024));

    let regions = builder.diagram_regions();
    eprintln!("\n{}", format_diagram_ascii(&regions, total_size));
    eprintln!("\nPress Enter to start clone, or Ctrl+C to cancel...");
    {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
    }

    let stream_start = Instant::now();
    let mut bytes_written = 0u64;

    if let Some(ref path) = args.output {
        let path_str = path.to_str().unwrap();
        eprintln!("Writing to file: {:?}", path_str);
        let mut sink = FileSink::new(path_str)?;
        bytes_written = builder.stream_to(&mut sink, None, None, None)?;
        // Verify: read back first 512 bytes and check MBR/GPT
        if let Ok(mut f) = std::fs::File::open(path_str) {
            use std::io::Read;
            let mut head = [0u8; 512];
            if f.read_exact(&mut head).is_ok() {
                let mbr_sig = head[510] as u16 | ((head[511] as u16) << 8);
                if mbr_sig != 0xAA55 {
                    eprintln!("WARNING: Output file MBR signature invalid (0x{:04x}), image may be corrupted", mbr_sig);
                }
            }
        }
    } else if let Some(target_disk) = args.target_disk {
        eprintln!("Writing to physical drive {}...", target_disk);
        let mut sink = LocalDiskSink::new(target_disk)?;
        bytes_written = builder.stream_to(&mut sink, None, None, None)?;
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
        bytes_written = builder.stream_to(&mut sink, None, None, None)?;
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
