//! diskclone-visual - GUI for Windows bootable disk clone via VSS

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use eframe::egui;
use diskclone::{
    create_ssh_session, DiskCloneError, FileSink, ImageBuilder, LocalDiskSink, ProgressSink,
    SshSink, VssSnapshot,
};

enum WorkerMsg {
    Total(u64),
    Status(String, bool),
    Done,
    Error(String),
}

fn main() -> eframe::Result<()> {
    let title = format!("DiskClone Visual {}", diskclone::VERSION);
    eframe::run_native(
        &title,
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([560.0, 480.0]),
            ..Default::default()
        },
        Box::new(|_| Ok(Box::new(DiskCloneApp::default()))),
    )
}

impl Default for DiskCloneApp {
    fn default() -> Self {
        let ssh_user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "root".to_string());
        Self {
            disks: Vec::new(),
            selected_source: None,
            dest_mode: DestMode::default(),
            ssh_user,
            ssh_host: String::new(),
            ssh_password: String::new(),
            remote_path: String::new(),
            local_file_path: String::new(),
            target_disk: None,
            status: String::new(),
            status_ok: true,
            progress: Arc::new(AtomicU64::new(0)),
            total_bytes: 0,
            phase: Phase::default(),
            error: None,
            worker_rx: None,
        }
    }
}

struct DiskCloneApp {
    disks: Vec<diskclone::PhysicalDiskInfo>,
    selected_source: Option<u32>,
    dest_mode: DestMode,
    ssh_user: String,
    ssh_host: String,
    ssh_password: String,
    remote_path: String,
    local_file_path: String,
    target_disk: Option<u32>,
    status: String,
    status_ok: bool,
    progress: Arc<AtomicU64>,
    total_bytes: u64,
    phase: Phase,
    error: Option<String>,
    worker_rx: Option<mpsc::Receiver<WorkerMsg>>,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum DestMode {
    #[default]
    Ssh,
    LocalFile,
    LocalDisk,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Phase {
    #[default]
    Idle,
    LoadingDisks,
    CreatingSnapshot,
    Streaming,
    Done,
    Error,
}

impl DiskCloneApp {
    fn load_disks(&mut self) {
        self.phase = Phase::LoadingDisks;
        self.status = "Loading disks...".to_string();
        match diskclone::list_physical_disks() {
            Ok(d) => {
                self.disks = d;
                self.status = format!("Found {} disk(s)", self.disks.len());
                self.status_ok = true;
                self.phase = Phase::Idle;
            }
            Err(e) => {
                self.status = format!("Error: {}", e);
                self.status_ok = false;
                self.phase = Phase::Error;
            }
        }
    }

    fn start_clone(&mut self) {
        let source = match self.selected_source {
            Some(d) => d,
            None => {
                self.error = Some("Select a source disk".to_string());
                return;
            }
        };

        if self.dest_mode == DestMode::LocalDisk && self.target_disk.is_none() {
            self.error = Some("Select a target disk".to_string());
            return;
        }

        self.error = None;
        self.phase = Phase::CreatingSnapshot;
        self.status = "Creating VSS snapshot...".to_string();

        let progress = Arc::new(AtomicU64::new(0));
        self.progress = progress.clone();

        let dest_mode = self.dest_mode;
        let ssh_user = self.ssh_user.clone();
        let ssh_host = self.ssh_host.clone();
        let ssh_password = self.ssh_password.clone();
        let remote_path = self.remote_path.clone();
        let local_file_path = self.local_file_path.clone();
        let target_disk = self.target_disk;

        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let ssh_password = if ssh_password.trim().is_empty() {
                None
            } else {
                Some(ssh_password)
            };
            let result = run_clone(
                source,
                dest_mode,
                &ssh_user,
                &ssh_host,
                ssh_password.as_deref(),
                &remote_path,
                &local_file_path,
                target_disk,
                progress,
                |msg| {
                    let _ = tx.send(msg);
                },
            );
            match result {
                Ok(_bytes) => {
                    let _ = tx.send(WorkerMsg::Done);
                }
                Err(e) => {
                    let _ = tx.send(WorkerMsg::Error(e.to_string()));
                }
            }
        });

        self.worker_rx = Some(rx);
        self.phase = Phase::Streaming;
    }
}

fn run_clone<F: Fn(WorkerMsg)>(
    source_disk: u32,
    dest_mode: DestMode,
    ssh_user: &str,
    ssh_host: &str,
    ssh_password: Option<&str>,
    remote_path: &str,
    local_file_path: &str,
    target_disk: Option<u32>,
    progress: Arc<AtomicU64>,
    send: F,
) -> Result<u64, DiskCloneError> {
    send(WorkerMsg::Status("Creating VSS snapshot...".to_string(), true));
    let layout = diskclone::get_disk_layout_from_disk(source_disk)?;

    let vss = VssSnapshot::create_for_disk(source_disk, &layout.partitions)?;
    send(WorkerMsg::Status("VSS snapshot created".to_string(), true));

    let builder = ImageBuilder::new(source_disk, &vss, 16)?;
    let total_size = builder.disk_length();
    send(WorkerMsg::Total(total_size));
    progress.store(0, Ordering::Relaxed);

    let stream_start = Instant::now();
    let bytes_written = match dest_mode {
        DestMode::Ssh => {
            send(WorkerMsg::Status(format!("Connecting to {}...", ssh_host), true));
            let sess = create_ssh_session(ssh_user, ssh_host, ssh_password)?;
            let path = if remote_path.is_empty() {
                "/tmp/diskclone.img"
            } else {
                remote_path
            };
            send(WorkerMsg::Status(format!("Streaming to {}...", path), true));
            let sink = if path.starts_with("/dev/") {
                SshSink::new(&sess, path)?
            } else {
                SshSink::new_cat(&sess, path)?
            };
            let mut progress_sink = ProgressSink::new(sink, progress.clone(), total_size);
            builder.stream_to(&mut progress_sink)?
        }
        DestMode::LocalFile => {
            let path = if local_file_path.is_empty() {
                "diskclone.img"
            } else {
                local_file_path
            };
            send(WorkerMsg::Status(format!("Writing to {}...", path), true));
            let sink = FileSink::new(path)?;
            let mut progress_sink = ProgressSink::new(sink, progress.clone(), total_size);
            builder.stream_to(&mut progress_sink)?
        }
        DestMode::LocalDisk => {
            let disk = target_disk.unwrap_or(1);
            send(WorkerMsg::Status(format!("Writing to PhysicalDrive{}...", disk), true));
            let sink = LocalDiskSink::new(disk)?;
            let mut progress_sink = ProgressSink::new(sink, progress.clone(), total_size);
            builder.stream_to(&mut progress_sink)?
        }
    };

    vss.finish()?;

    let elapsed = stream_start.elapsed().as_secs_f64();
    let mb_s = (bytes_written as f64 / 1024.0 / 1024.0) / elapsed;
    send(WorkerMsg::Status(
        format!(
            "Done. {:.0} MB in {:.1}s ({:.1} MB/s)",
            bytes_written / (1024 * 1024),
            elapsed,
            mb_s
        ),
        true,
    ));

    Ok(bytes_written)
}

impl eframe::App for DiskCloneApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.phase == Phase::Streaming || self.phase == Phase::CreatingSnapshot {
            ctx.request_repaint();
        }

        if let Some(rx) = self.worker_rx.take() {
            loop {
                match rx.try_recv() {
                    Ok(WorkerMsg::Total(t)) => self.total_bytes = t,
                    Ok(WorkerMsg::Status(s, ok)) => {
                        self.status = s;
                        self.status_ok = ok;
                    }
                    Ok(WorkerMsg::Done) => {
                        self.phase = Phase::Done;
                        break;
                    }
                    Ok(WorkerMsg::Error(e)) => {
                        self.status = e.clone();
                        self.error = Some(e);
                        self.phase = Phase::Error;
                        break;
                    }
                    Err(_) => {
                        self.worker_rx = Some(rx);
                        break;
                    }
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("DiskClone Visual");
                ui.label(format!("v{}", diskclone::VERSION));
            });
            ui.add_space(8.0);

            if self.disks.is_empty() && self.phase == Phase::Idle {
                if ui.button("Load disks").clicked() {
                    self.load_disks();
                }
            } else if self.phase == Phase::LoadingDisks {
                ui.spinner();
                ui.label(&self.status);
            } else {
                ui.horizontal(|ui| {
                    ui.label("Source disk:");
                    for d in &self.disks {
                        let label = format!(
                            "Drive {} ({:.1} GB)",
                            d.disk_number,
                            d.size_bytes as f64 / 1024.0 / 1024.0 / 1024.0
                        );
                        let sel = self.selected_source == Some(d.disk_number);
                        if ui.selectable_label(sel, &label).clicked() {
                            self.selected_source = Some(d.disk_number);
                        }
                    }
                });
                ui.add_space(8.0);

                ui.label("Destination:");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.dest_mode, DestMode::Ssh, "SSH");
                    ui.radio_value(&mut self.dest_mode, DestMode::LocalFile, "Local file");
                    ui.radio_value(&mut self.dest_mode, DestMode::LocalDisk, "Local disk");
                });

                match self.dest_mode {
                    DestMode::Ssh => {
                        ui.horizontal(|ui| {
                            ui.label("User:");
                            ui.text_edit_singleline(&mut self.ssh_user);
                            ui.label("Host:");
                            ui.text_edit_singleline(&mut self.ssh_host);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Password:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.ssh_password)
                                    .password(true)
                                    .desired_width(120.0),
                            );
                            ui.label("(optional, tries agent/pubkey first)");
                        });
                        ui.horizontal(|ui| {
                            ui.label("Remote path:");
                            ui.text_edit_singleline(&mut self.remote_path);
                        });
                        if self.remote_path.is_empty() {
                            ui.label("(default: /tmp/diskclone.img)");
                        }
                    }
                    DestMode::LocalFile => {
                        ui.horizontal(|ui| {
                            ui.label("File path:");
                            ui.text_edit_singleline(&mut self.local_file_path);
                        });
                        if self.local_file_path.is_empty() {
                            ui.label("(default: diskclone.img)");
                        }
                    }
                    DestMode::LocalDisk => {
                        ui.horizontal(|ui| {
                            ui.label("Target drive:");
                            for d in &self.disks {
                                if d.disk_number != self.selected_source.unwrap_or(u32::MAX) {
                                    let sel = self.target_disk == Some(d.disk_number);
                                    if ui.selectable_label(sel, &format!("Drive {}", d.disk_number)).clicked() {
                                        self.target_disk = Some(d.disk_number);
                                    }
                                }
                            }
                        });
                    }
                }

                ui.add_space(12.0);

                if let Some(ref e) = self.error {
                    ui.colored_label(egui::Color32::RED, e);
                }

                match self.phase {
                    Phase::Idle => {
                        if ui.button("Start clone").clicked() {
                            self.start_clone();
                        }
                    }
                    Phase::CreatingSnapshot | Phase::Streaming => {
                        ui.spinner();
                        ui.label(&self.status);
                        let written = self.progress.load(Ordering::Relaxed);
                        if self.total_bytes > 0 {
                            let pct = (written as f32 / self.total_bytes as f32) * 100.0;
                            ui.add(
                                egui::ProgressBar::new(pct / 100.0)
                                    .text(format!("{:.1}% ({:.1} GB / {:.1} GB)", pct, written as f64 / 1e9, self.total_bytes as f64 / 1e9)),
                            );
                        }
                    }
                    Phase::Done => {
                        ui.colored_label(egui::Color32::GREEN, &self.status);
                        if ui.button("Clone another").clicked() {
                            self.phase = Phase::Idle;
                            self.status.clear();
                        }
                    }
                    Phase::Error => {
                        ui.colored_label(egui::Color32::RED, &self.status);
                        if ui.button("Retry").clicked() {
                            self.phase = Phase::Idle;
                        }
                    }
                    Phase::LoadingDisks => {}
                }
            }
        });
    }
}
