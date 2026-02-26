//! diskclone-visual - GUI for Windows bootable disk clone via VSS

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use eframe::egui;
use diskclone::{
    analyze_snapshot_support, create_ssh_session, open_shadow_in_explorer, DiagramRegion,
    DiskCloneError, FileSink, ImageBuilder, LocalDiskSink, ProgressSink, RegionSource,
    SnapshotAnalysis, SshSink, VssSnapshot,
};

enum WorkerMsg {
    Total(u64),
    Status(String, bool),
    DiagramReady(Vec<DiagramRegion>, u64),
    Done,
    Error(String),
    Cancelled,
}

fn main() -> eframe::Result<()> {
    let title = format!("DiskClone Visual {}", diskclone::VERSION);
    eframe::run_native(
        &title,
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([560.0, 640.0]),
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
            diagram_regions: Vec::new(),
            confirm_tx: None,
            snapshot_analysis: None,
            analysis_rx: None,
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
    diagram_regions: Vec<DiagramRegion>,
    confirm_tx: Option<mpsc::Sender<Option<DestConfig>>>,
    snapshot_analysis: Option<Vec<SnapshotAnalysis>>,
    analysis_rx: Option<mpsc::Receiver<Result<Vec<SnapshotAnalysis>, DiskCloneError>>>,
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
    Confirm,
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

        self.error = None;
        self.phase = Phase::CreatingSnapshot;
        self.status = "Creating VSS snapshot...".to_string();

        let progress = Arc::new(AtomicU64::new(0));
        self.progress = progress.clone();

        let (tx, rx) = mpsc::channel();
        let (confirm_tx, confirm_rx) = mpsc::channel();

        thread::spawn(move || {
            let result = run_clone(
                source,
                progress,
                confirm_rx,
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
        self.confirm_tx = Some(confirm_tx);
        self.phase = Phase::CreatingSnapshot;
    }

    fn send_confirm_and_start(&mut self) {
        let dest_config = DestConfig {
            dest_mode: self.dest_mode,
            ssh_user: self.ssh_user.clone(),
            ssh_host: self.ssh_host.clone(),
            ssh_password: if self.ssh_password.trim().is_empty() {
                None
            } else {
                Some(self.ssh_password.clone())
            },
            remote_path: self.remote_path.clone(),
            local_file_path: self.local_file_path.clone(),
            target_disk: self.target_disk,
        };
        if let Some(tx) = self.confirm_tx.take() {
            let _ = tx.send(Some(dest_config));
            self.phase = Phase::Streaming;
            self.status = "Streaming...".to_string();
        }
    }
}

struct DestConfig {
    dest_mode: DestMode,
    ssh_user: String,
    ssh_host: String,
    ssh_password: Option<String>,
    remote_path: String,
    local_file_path: String,
    target_disk: Option<u32>,
}

fn run_clone<F: Fn(WorkerMsg)>(
    source_disk: u32,
    progress: Arc<AtomicU64>,
    confirm_rx: mpsc::Receiver<Option<DestConfig>>,
    send: F,
) -> Result<u64, DiskCloneError> {
    send(WorkerMsg::Status("Creating VSS snapshot...".to_string(), true));
    let layout = diskclone::get_disk_layout_from_disk(source_disk)?;

    let vss = VssSnapshot::create_for_disk(source_disk, &layout.partitions)?;
    send(WorkerMsg::Status("VSS snapshot created".to_string(), true));

    let builder = ImageBuilder::new(source_disk, &vss, 16)?;
    let total_size = builder.disk_length();
    let regions = builder.diagram_regions();
    send(WorkerMsg::DiagramReady(regions, total_size));

    // Wait for user to select destination and confirm
    let dest_config = match confirm_rx.recv() {
        Ok(Some(cfg)) => cfg,
        Ok(None) | Err(_) => {
            send(WorkerMsg::Cancelled);
            return Ok(0);
        }
    };

    if dest_config.dest_mode == DestMode::LocalDisk && dest_config.target_disk.is_none() {
        send(WorkerMsg::Error("Select a target disk for local disk clone".to_string()));
        return Ok(0);
    }

    progress.store(0, Ordering::Relaxed);

    let stream_start = Instant::now();
    let bytes_written = match dest_config.dest_mode {
        DestMode::Ssh => {
            send(WorkerMsg::Status(
                format!("Connecting to {}...", dest_config.ssh_host),
                true,
            ));
            let sess = create_ssh_session(
                &dest_config.ssh_user,
                &dest_config.ssh_host,
                dest_config.ssh_password.as_deref(),
            )?;
            let path = if dest_config.remote_path.is_empty() {
                "/tmp/diskclone.img"
            } else {
                &dest_config.remote_path
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
            let path = if dest_config.local_file_path.is_empty() {
                "diskclone.img"
            } else {
                &dest_config.local_file_path
            };
            send(WorkerMsg::Status(format!("Writing to {}...", path), true));
            let sink = FileSink::new(path)?;
            let mut progress_sink = ProgressSink::new(sink, progress.clone(), total_size);
            builder.stream_to(&mut progress_sink)?
        }
        DestMode::LocalDisk => {
            let disk = dest_config.target_disk.unwrap_or(1);
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
        if self.phase == Phase::Streaming
            || self.phase == Phase::CreatingSnapshot
            || self.phase == Phase::Confirm
            || self.analysis_rx.is_some()
        {
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
                    Ok(WorkerMsg::DiagramReady(regions, total)) => {
                        self.diagram_regions = regions;
                        self.total_bytes = total;
                        self.phase = Phase::Confirm;
                        self.status = "Review layout and confirm to start".to_string();
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
                    Ok(WorkerMsg::Cancelled) => {
                        self.phase = Phase::Idle;
                        self.status = "Cancelled".to_string();
                        self.diagram_regions.clear();
                        self.confirm_tx = None;
                        self.error = None;
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
                            let new_disk = d.disk_number;
                            if self.selected_source != Some(new_disk) {
                                self.snapshot_analysis = None;
                                let (tx, rx) = mpsc::channel();
                                self.analysis_rx = Some(rx);
                                thread::spawn(move || {
                                    let _ = tx.send(analyze_snapshot_support(new_disk));
                                });
                            }
                            self.selected_source = Some(d.disk_number);
                        }
                    }
                });
                ui.add_space(8.0);

                // Snapshot analysis — shown when disk selected, before Create snapshot
                if self.selected_source.is_some() {
                    if let Some(rx) = self.analysis_rx.take() {
                        match rx.try_recv() {
                            Ok(Ok(analysis)) => self.snapshot_analysis = Some(analysis),
                            Ok(Err(e)) => self.error = Some(e.to_string()),
                            Err(mpsc::TryRecvError::Empty) => self.analysis_rx = Some(rx),
                            Err(mpsc::TryRecvError::Disconnected) => {}
                        }
                    }

                    if let Some(ref analysis) = self.snapshot_analysis {
                        ui.collapsing("VSS snapshot analysis (before Create snapshot)", |ui| {
                            ui.label("Which partitions will get a VSS shadow:");
                            egui::Grid::new("snapshot_analysis")
                                .num_columns(4)
                                .striped(true)
                                .show(ui, |ui| {
                                    ui.strong("Partition");
                                    ui.strong("Size");
                                    ui.strong("Source");
                                    ui.strong("Reason");
                                    ui.end_row();
                                    for a in analysis {
                                        ui.label(format!("{}", a.partition_number));
                                        ui.label(format!("{:.1} MB", a.size_mb));
                                        let (src, color) = if a.vss_supported {
                                            ("VSS shadow", egui::Color32::from_rgb(60, 160, 80))
                                        } else if a.has_volume {
                                            ("Raw disk", egui::Color32::from_rgb(200, 100, 60))
                                        } else {
                                            ("Raw disk", egui::Color32::from_rgb(180, 120, 60))
                                        };
                                        ui.colored_label(color, src);
                                        ui.label(&a.reason);
                                        ui.end_row();
                                    }
                                });
                        });
                        ui.add_space(8.0);
                    } else if self.analysis_rx.is_some() {
                        ui.label("Analyzing snapshot support...");
                        ui.add_space(8.0);
                    }
                }

                // Preview (diagram) — shown before destination when snapshot is ready
                if self.phase == Phase::Confirm {
                    ui.label("Disk layout — data sources:");
                    ui.add_space(4.0);

                    let disk_len = self.total_bytes as f64;
                    let bar_height = 24.0;

                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), bar_height),
                        egui::Sense::hover(),
                    );
                    let mut x = rect.left();
                    for r in &self.diagram_regions {
                        let pct = (r.end - r.start) as f64 / disk_len;
                        let w = (rect.width() * pct as f32).max(2.0);
                        let color = match &r.source {
                            RegionSource::GptPrimary => egui::Color32::from_rgb(80, 120, 180),
                            RegionSource::GptBackup => egui::Color32::from_rgb(60, 100, 160),
                            RegionSource::Gap => egui::Color32::from_rgb(100, 100, 100),
                            RegionSource::PartitionShadow { .. } => {
                                egui::Color32::from_rgb(60, 160, 80)
                            }
                            RegionSource::PartitionRaw { .. } => {
                                egui::Color32::from_rgb(180, 120, 60)
                            }
                        };
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(
                                egui::pos2(x, rect.top()),
                                egui::vec2(w, bar_height),
                            ),
                            2.0,
                            color,
                        );
                        x += w;
                    }

                    ui.add_space(8.0);

                    let swatch_size = egui::vec2(16.0, 16.0);
                    let legend_items: [(egui::Color32, &str); 5] = [
                        (egui::Color32::from_rgb(80, 120, 180), "Primary GPT (raw disk)"),
                        (egui::Color32::from_rgb(60, 100, 160), "Backup GPT (raw disk)"),
                        (egui::Color32::from_rgb(100, 100, 100), "Gap (zeros)"),
                        (egui::Color32::from_rgb(60, 160, 80), "Partition from VSS shadow"),
                        (egui::Color32::from_rgb(180, 120, 60), "Partition from raw disk (MSR, etc.)"),
                    ];
                    for (color, label) in legend_items {
                        ui.horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(swatch_size, egui::Sense::hover());
                            ui.painter().rect_filled(rect, 2.0, color);
                            ui.add_space(6.0);
                            ui.label(label);
                        });
                    }

                    ui.add_space(8.0);
                    ui.separator();

                    for r in &self.diagram_regions {
                        let size_mb = (r.end - r.start) as f64 / 1024.0 / 1024.0;
                        ui.horizontal(|ui| {
                            ui.label(format!("  {} ({:.1} MB)", r.label, size_mb));
                            if let Some(ref path) = r.shadow_path {
                                if ui.button("Browse shadow copy").clicked() {
                                    if let Err(e) = open_shadow_in_explorer(path) {
                                        self.error = Some(e.to_string());
                                    }
                                }
                            }
                        });
                    }

                    ui.add_space(8.0);
                    ui.label(format!(
                        "Total: {:.1} GB",
                        self.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0
                    ));
                    ui.add_space(12.0);
                }

                // Destination — only shown after preview (when snapshot is ready)
                if self.phase == Phase::Confirm {
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
                    ui.add_space(8.0);
                }

                ui.add_space(12.0);

                if let Some(ref e) = self.error {
                    ui.colored_label(egui::Color32::RED, e);
                }

                match self.phase {
                    Phase::Idle => {
                        if ui.button("Create snapshot").clicked() {
                            self.start_clone();
                        }
                    }
                    Phase::Confirm => {
                        ui.horizontal(|ui| {
                            if ui.button("Start clone").clicked() {
                                if self.dest_mode == DestMode::LocalDisk
                                    && self.target_disk.is_none()
                                {
                                    self.error =
                                        Some("Select a target disk for local disk clone".to_string());
                                } else {
                                    self.error = None;
                                    self.send_confirm_and_start();
                                }
                            }
                            if ui.button("Cancel").clicked() {
                                drop(self.confirm_tx.take());
                                self.phase = Phase::Idle;
                                self.diagram_regions.clear();
                            }
                        });
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
