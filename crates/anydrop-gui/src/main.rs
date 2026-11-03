//! AnyDrop desktop GUI (egui / eframe).
//!
//! - Always receiving in the background (TCP + mDNS advertise)
//! - Device list from LAN (mDNS) + Tailscale, refreshed periodically
//! - Drag & drop files/folders onto the window, pick a device, press Send
//! - Incoming offers pop up a dialog (Accept / Decline) unless auto-accept is on

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(test)]
mod icns;
mod icon;
mod settings;
mod single;
mod tray;

use anydrop_core::discovery::{Advertiser, Browser};
use anydrop_core::transfer::{
    self, collect_files, AcceptFn, ConnOutcome, OfferRequest, Progress, ProgressFn, ReceiverConfig,
    SendOutcome,
};
use anydrop_core::{tailscale, DeviceInfo, Peer, PeerSource};
use eframe::egui;
use settings::Settings;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tray::{Tray, TrayAction};

/// LAN peers come from a long-lived mDNS listener (no polling). Tailscale has no
/// multicast, so its peers are probed quietly at this interval.
const TS_RESCAN_EVERY: Duration = Duration::from_secs(60);
/// Keep a Tailscale peer listed until it has been missing this long.
const TS_STALE_AFTER: Duration = Duration::from_secs(180);
/// Shown wherever auto-accept can be switched on.
const AUTO_ACCEPT_WARNING: &str = "⚠ 같은 네트워크의 누구나 확인 없이 파일을 보낼 수 있습니다.";

fn main() -> eframe::Result {
    // Paths passed on the command line (e.g. "Send with AnyDrop" menu) are pre-selected.
    let files: Vec<PathBuf> = std::env::args_os()
        .skip(1)
        .filter(|a| !a.to_string_lossy().starts_with("--"))
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();

    // Only one AnyDrop per user: a second launch wakes the running one and exits.
    let instance = match single::acquire(&files) {
        single::Acquire::Primary(p) => p,
        single::Acquire::AlreadyRunning { notified } => {
            single::tell_already_running(notified);
            return Ok(());
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AnyDrop")
            .with_inner_size([860.0, 600.0])
            .with_min_inner_size([640.0, 420.0])
            .with_drag_and_drop(true)
            .with_icon(Arc::new({
                // eframe also uses this as the Dock icon on macOS: give it enough pixels to stay sharp.
                let (rgba, width, height) =
                    icon::app_icon_rgba(if cfg!(target_os = "macos") { 512 } else { 64 });
                egui::IconData {
                    rgba,
                    width,
                    height,
                }
            })),
        ..Default::default()
    };
    eframe::run_native(
        "AnyDrop",
        options,
        Box::new(move |cc| Ok(Box::new(AnyDropApp::new(cc, instance, files)))),
    )
}

// ------------------------------------------------------------------ shared state

struct PendingOffer {
    req: OfferRequest,
    reply: Option<oneshot::Sender<bool>>,
}

struct LogLine {
    time: String,
    ok: bool,
    text: String,
    folder: Option<PathBuf>,
}

#[derive(Default)]
struct Shared {
    ts_peers: HashMap<String, (Peer, Instant)>,
    ts_scanning: bool,
    offers: VecDeque<PendingOffer>,
    sending: Option<(String, Option<Progress>)>,
    clear_files: bool,
    /// Another launch asked us to come to the front (with optional files to add).
    show_requests: Vec<Vec<PathBuf>>,
    receiving: Option<Progress>,
    log: Vec<LogLine>,
    receiver_status: String,
}

impl Shared {
    fn log(&mut self, ok: bool, text: impl Into<String>, folder: Option<PathBuf>) {
        let time = chrono::Local::now().format("%H:%M:%S").to_string();
        self.log.push(LogLine {
            time,
            ok,
            text: text.into(),
            folder,
        });
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }
}

type SharedRef = Arc<Mutex<Shared>>;

struct ReceiverHandle {
    task: tokio::task::JoinHandle<()>,
    _adv: Option<Advertiser>,
}

// ------------------------------------------------------------------------- app

struct AnyDropApp {
    rt: tokio::runtime::Runtime,
    ctx: egui::Context,
    shared: SharedRef,
    settings: Settings,
    draft: Settings,
    me: DeviceInfo,
    auto_accept: Arc<AtomicBool>,
    receiver: Option<ReceiverHandle>,
    files: Vec<PathBuf>,
    selected: Option<String>, // peer device id
    manual_addr: String,
    manual_peers: Vec<Peer>,
    browser: Option<Browser>,
    last_ts_scan: Option<Instant>,
    show_settings: bool,
    /// Tray asked to enable auto-accept: show the warning and ask first.
    confirm_auto_accept: bool,
    tray: Option<Tray>,
    /// Window is hidden in the tray.
    hidden: bool,
    /// Hide on the first frame (`--hidden` or "start in tray").
    hide_on_start: bool,
    /// Tray "Quit" was chosen: let the close request through.
    quitting: bool,
    tooltip: String,
    /// Holds the single-instance lock until the app exits.
    _instance: single::PrimaryInstance,
}

impl AnyDropApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        mut instance: single::PrimaryInstance,
        files: Vec<PathBuf>,
    ) -> Self {
        install_fonts(&cc.egui_ctx);
        let settings = Settings::load();
        let me = DeviceInfo::local(Some(settings.name.clone()));
        let mut app = Self {
            rt: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime"),
            ctx: cc.egui_ctx.clone(),
            shared: Arc::default(),
            draft: settings.clone(),
            auto_accept: Arc::new(AtomicBool::new(settings.auto_accept)),
            settings,
            me,
            receiver: None,
            // Paths passed on the command line (e.g. "Send with AnyDrop" menu) are pre-selected.
            files,
            selected: None,
            manual_addr: String::new(),
            manual_peers: Vec::new(),
            browser: None,
            last_ts_scan: None,
            show_settings: false,
            confirm_auto_accept: false,
            tray: None,
            hidden: false,
            hide_on_start: false,
            quitting: false,
            tooltip: String::new(),
            _instance: single::PrimaryInstance::placeholder(),
        };
        {
            let shared = app.shared.clone();
            let ctx = cc.egui_ctx.clone();
            instance.listen(move |paths| {
                shared.lock().unwrap().show_requests.push(paths);
                ctx.request_repaint();
            });
        }
        app._instance = instance;
        app.tray = Tray::new(&cc.egui_ctx, app.settings.auto_accept);
        app.hide_on_start = app.tray.is_some()
            && (app.settings.start_hidden || std::env::args().any(|a| a == "--hidden"));
        app.start_receiver();
        app.start_browser();
        app
    }

    // ---------------------------------------------------------------- receiver

    fn start_receiver(&mut self) {
        if let Some(old) = self.receiver.take() {
            old.task.abort();
        }
        let s = &self.settings;
        if let Err(e) = std::fs::create_dir_all(&s.download_dir) {
            self.shared
                .lock()
                .unwrap()
                .log(false, format!("저장 폴더를 만들 수 없음: {e}"), None);
        }
        let listener = match self
            .rt
            .block_on(tokio::net::TcpListener::bind(("0.0.0.0", s.port)))
        {
            Ok(l) => l,
            Err(e) => {
                let mut sh = self.shared.lock().unwrap();
                let msg = format!(
                    "수신 불가: 포트 {} 사용 중 — AnyDrop이 이미 실행 중인지(트레이) 확인하세요 ({e})",
                    s.port
                );
                sh.receiver_status = msg.clone();
                sh.log(false, msg, None);
                return;
            }
        };
        let adv = {
            let _g = self.rt.enter();
            match Advertiser::start(&self.me, s.port) {
                Ok(a) => Some(a),
                Err(e) => {
                    self.shared.lock().unwrap().log(
                        false,
                        format!("mDNS 광고 실패 (IP 직접 입력으로는 수신 가능): {e}"),
                        None,
                    );
                    None
                }
            }
        };
        self.shared.lock().unwrap().receiver_status = format!("수신 대기 중 · 포트 {}", s.port);

        let cfg = Arc::new(ReceiverConfig {
            dest_dir: s.download_dir.clone(),
            device: self.me.clone(),
            accept: self.accept_fn(),
            progress: Some(self.progress_fn(false)),
            max_total_size: s.max_total_size(),
        });
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        let dir = s.download_dir.clone();
        let on_result = Arc::new(move |remote: SocketAddr, r: anyhow::Result<ConnOutcome>| {
            let mut sh = shared.lock().unwrap();
            sh.receiving = None;
            match r {
                Ok(ConnOutcome::Received(rep)) => {
                    let first = rep
                        .saved
                        .first()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    sh.log(
                        true,
                        format!(
                            "받음: {}개 파일, {} ← {}  ({first}{})",
                            rep.saved.len(),
                            human(rep.bytes),
                            rep.peer.name,
                            if rep.saved.len() > 1 { " 외" } else { "" }
                        ),
                        Some(dir.clone()),
                    );
                }
                Ok(ConnOutcome::Rejected(p)) => {
                    sh.log(false, format!("{}의 요청을 거절함", p.name), None)
                }
                Ok(ConnOutcome::Probe(_)) => {}
                Err(e) => sh.log(false, format!("{remote} 에서 받기 실패: {e:#}"), None),
            }
            ctx.request_repaint();
        });
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        let task = self.rt.spawn(async move {
            if let Err(e) = transfer::serve(listener, cfg, on_result).await {
                let mut sh = shared.lock().unwrap();
                sh.receiver_status = format!("수신 중지됨: {e}");
                ctx.request_repaint();
            }
        });
        self.receiver = Some(ReceiverHandle { task, _adv: adv });
    }

    fn accept_fn(&self) -> AcceptFn {
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        let auto = self.auto_accept.clone();
        Arc::new(move |req: OfferRequest| {
            let shared = shared.clone();
            let ctx = ctx.clone();
            let auto = auto.load(Ordering::Relaxed);
            Box::pin(async move {
                if auto {
                    return true;
                }
                let (tx, rx) = oneshot::channel();
                shared.lock().unwrap().offers.push_back(PendingOffer {
                    req,
                    reply: Some(tx),
                });
                ctx.request_repaint();
                rx.await.unwrap_or(false)
            })
        })
    }

    fn progress_fn(&self, sending: bool) -> ProgressFn {
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        Arc::new(move |p: &Progress| {
            let mut sh = shared.lock().unwrap();
            if sending {
                if let Some((_, slot)) = sh.sending.as_mut() {
                    *slot = Some(p.clone());
                }
            } else {
                sh.receiving = Some(p.clone());
            }
            ctx.request_repaint_after(Duration::from_millis(50));
        })
    }

    // ---------------------------------------------------------------- discovery

    fn start_browser(&mut self) {
        let ctx = self.ctx.clone();
        let _g = self.rt.enter();
        match Browser::start(Arc::new(move || ctx.request_repaint())) {
            Ok(b) => self.browser = Some(b),
            Err(e) => {
                self.shared
                    .lock()
                    .unwrap()
                    .log(false, format!("mDNS 검색 시작 실패: {e}"), None)
            }
        }
    }

    /// Quiet background Tailscale probe; never clears the list while running.
    fn ts_scan(&mut self) {
        {
            let mut sh = self.shared.lock().unwrap();
            if sh.ts_scanning {
                return;
            }
            sh.ts_scanning = true;
        }
        self.last_ts_scan = Some(Instant::now());
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        let me = self.me.clone();
        let port = self.settings.port;
        self.rt.spawn(async move {
            let found = tailscale::discover(&me, port, Duration::from_millis(1500))
                .await
                .unwrap_or_default();
            let mut sh = shared.lock().unwrap();
            sh.ts_scanning = false;
            let before: Vec<String> = sh.ts_peers.keys().cloned().collect();
            let now = Instant::now();
            for p in found {
                sh.ts_peers.insert(p.device.id.clone(), (p, now));
            }
            sh.ts_peers
                .retain(|_, (_, seen)| now.duration_since(*seen) < TS_STALE_AFTER);
            let mut after: Vec<String> = sh.ts_peers.keys().cloned().collect();
            let mut before = before;
            before.sort();
            after.sort();
            if before != after {
                ctx.request_repaint();
            }
        });
    }

    fn refresh(&mut self) {
        if let Some(b) = &self.browser {
            let _g = self.rt.enter();
            let _ = b.refresh();
        }
        self.ts_scan();
    }

    fn add_manual(&mut self) {
        let input = self.manual_addr.trim().to_string();
        if input.is_empty() {
            return;
        }
        let port = self.settings.port;
        let me = self.me.clone();
        let res = self.rt.block_on(async move {
            let host_port = if input.parse::<SocketAddr>().is_ok()
                || input
                    .rsplit_once(':')
                    .is_some_and(|(_, p)| p.parse::<u16>().is_ok() && !input.contains("::"))
            {
                input.clone()
            } else if input.contains(':') {
                format!("[{input}]:{port}")
            } else {
                format!("{input}:{port}")
            };
            let addr =
                tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host(host_port))
                    .await
                    .map_err(|_| anyhow::anyhow!("주소 확인 시간 초과"))??
                    .min_by_key(|a| a.is_ipv6())
                    .ok_or_else(|| anyhow::anyhow!("주소를 찾을 수 없음"))?;
            let dev = transfer::probe(addr, &me, Duration::from_secs(3)).await?;
            anyhow::Ok(Peer {
                device: dev,
                addrs: vec![addr],
                source: PeerSource::Manual,
            })
        });
        match res {
            Ok(p) => {
                self.selected = Some(p.device.id.clone());
                self.manual_peers.retain(|m| m.device.id != p.device.id);
                self.manual_peers.push(p);
                self.manual_addr.clear();
            }
            Err(e) => self.shared.lock().unwrap().log(
                false,
                format!("{} 연결 실패: {e:#}", self.manual_addr),
                None,
            ),
        }
    }

    /// LAN (mDNS) + Tailscale + manually added peers, merged by device id, sorted by name.
    fn all_peers(&self) -> Vec<Peer> {
        let mut v = self.browser.as_ref().map(|b| b.peers()).unwrap_or_default();
        let ts: Vec<Peer> = self
            .shared
            .lock()
            .unwrap()
            .ts_peers
            .values()
            .map(|(p, _)| p.clone())
            .collect();
        for p in ts.into_iter().chain(self.manual_peers.iter().cloned()) {
            if let Some(e) = v.iter_mut().find(|e| e.device.id == p.device.id) {
                for a in p.addrs {
                    if !e.addrs.contains(&a) {
                        e.addrs.push(a);
                    }
                }
            } else {
                v.push(p);
            }
        }
        v.retain(|p| p.device.id != self.me.id);
        v.sort_by(|a, b| {
            a.device
                .name
                .to_lowercase()
                .cmp(&b.device.name.to_lowercase())
                .then(a.device.id.cmp(&b.device.id))
        });
        v
    }

    // ---------------------------------------------------------------- sending

    fn send(&mut self, peer: Peer) {
        let files = self.files.clone();
        let me = self.me.clone();
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        let progress = self.progress_fn(true);
        shared.lock().unwrap().sending = Some((peer.device.name.clone(), None));
        self.rt.spawn(async move {
            let res: anyhow::Result<SendOutcome> = async {
                let list = tokio::task::spawn_blocking(move || collect_files(&files)).await??;
                let (addr, _) = transfer::probe_any(&peer.addrs, &me, Duration::from_secs(3))
                    .await
                    .map_err(|e| anyhow::anyhow!("응답하는 주소가 없음 — {e:#}"))?;
                transfer::send_files(addr, &me, &list, Some(progress)).await
            }
            .await;
            let mut sh = shared.lock().unwrap();
            sh.sending = None;
            match res {
                Ok(SendOutcome::Completed { files, bytes }) => {
                    sh.clear_files = true;
                    sh.log(
                        true,
                        format!(
                            "보냄: {files}개 파일, {} → {}",
                            human(bytes),
                            peer.device.name
                        ),
                        None,
                    )
                }
                Ok(SendOutcome::Rejected(r)) => sh.log(
                    false,
                    format!("{}이(가) 거절함: {r}", peer.device.name),
                    None,
                ),
                Err(e) => sh.log(
                    false,
                    format!("{}에게 보내기 실패: {e:#}", peer.device.name),
                    None,
                ),
            }
            ctx.request_repaint();
        });
    }

    fn apply_settings(&mut self) {
        let name_changed = self.draft.name != self.settings.name;
        self.settings = self.draft.clone();
        self.auto_accept
            .store(self.settings.auto_accept, Ordering::Relaxed);
        if let Some(t) = &self.tray {
            t.set_auto_accept(self.settings.auto_accept);
        }
        if let Err(e) = self.settings.save() {
            self.shared
                .lock()
                .unwrap()
                .log(false, format!("설정 저장 실패: {e}"), None);
        }
        if name_changed {
            self.me = DeviceInfo::local(Some(self.settings.name.clone()));
        }
        self.start_receiver();
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        // Only re-map a hidden window: sending Visible(true) to a visible one makes
        // some window systems re-create it without a fresh frame (blank window).
        if std::mem::take(&mut self.hidden) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
        ctx.request_repaint_after(Duration::from_millis(150));
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.hidden = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn update_tooltip(&mut self) {
        let Some(t) = &self.tray else { return };
        let text = {
            let sh = self.shared.lock().unwrap();
            let mut text = format!("AnyDrop — {}", self.settings.name);
            if let Some((to, _)) = &sh.sending {
                text.push_str(&format!("\n보내는 중 → {to}"));
            } else if sh.receiving.is_some() {
                text.push_str("\n받는 중...");
            } else if let Some(l) = sh.log.last() {
                text.push_str(&format!(
                    "\n{} {}",
                    if l.ok { "✔" } else { "✘" },
                    l.text.chars().take(60).collect::<String>()
                ));
            }
            text
        };
        if text != self.tooltip {
            t.set_tooltip(&text);
            self.tooltip = text;
        }
    }
}

// ----------------------------------------------------------------------- UI

impl eframe::App for AnyDropApp {
    /// Runs every frame *and* while the window is hidden in the tray.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.hide_on_start) {
            self.hide_window(ctx);
        }

        let requests = std::mem::take(&mut self.shared.lock().unwrap().show_requests);
        for paths in requests {
            for p in paths {
                if !self.files.contains(&p) {
                    self.files.push(p);
                }
            }
            self.show_window(ctx);
        }

        let actions = self
            .tray
            .as_ref()
            .map(|t| t.take_actions())
            .unwrap_or_default();
        for a in actions {
            match a {
                TrayAction::Show => self.show_window(ctx),
                TrayAction::OpenFolder => open_folder(&self.settings.download_dir),
                TrayAction::ToggleAutoAccept => {
                    if self.settings.auto_accept {
                        self.set_auto_accept(false);
                    } else {
                        // Turning it on needs the same warning as the settings dialog.
                        // The menu check mark already flipped itself: undo until confirmed.
                        if let Some(t) = &self.tray {
                            t.set_auto_accept(false);
                        }
                        self.confirm_auto_accept = true;
                        self.show_window(ctx);
                    }
                }
                TrayAction::Quit => {
                    self.quitting = true;
                    // A hidden window must be visible for the close to be processed on some platforms.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        // Close button -> hide to tray (keep receiving in the background).
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quitting
            && self.tray.is_some()
            && self.settings.minimize_to_tray
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }

        // An incoming offer needs the user: bring the window back.
        if self.hidden && !self.shared.lock().unwrap().offers.is_empty() {
            self.show_window(ctx);
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }

        if self
            .last_ts_scan
            .is_none_or(|t| t.elapsed() > TS_RESCAN_EVERY)
        {
            self.ts_scan();
        }
        // Wake up for the next Tailscale scan even when hidden.
        ctx.request_repaint_after(TS_RESCAN_EVERY);

        self.update_tooltip();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        ctx.request_repaint_after(Duration::from_secs(1));

        // Drag & drop
        let (hovering, dropped): (bool, Vec<PathBuf>) = ctx.input(|i| {
            (
                !i.raw.hovered_files.is_empty(),
                i.raw
                    .dropped_files
                    .iter()
                    .map(|f| f.path().to_path_buf())
                    .collect(),
            )
        });
        for p in dropped {
            if !p.as_os_str().is_empty() && !self.files.contains(&p) {
                self.files.push(p);
            }
        }

        if std::mem::take(&mut self.shared.lock().unwrap().clear_files) {
            self.files.clear();
        }
        let peers = self.all_peers();
        let (status, sending, receiving) = {
            let sh = self.shared.lock().unwrap();
            (
                sh.receiver_status.clone(),
                sh.sending.clone(),
                sh.receiving.clone(),
            )
        };

        // ---- top bar
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("AnyDrop");
                ui.separator();
                ui.label(format!("💻 {}", self.settings.name));
                ui.weak(format!("({})  {}", self.me.os, status));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ 설정").clicked() {
                        self.draft = self.settings.clone();
                        self.show_settings = true;
                    }
                    if ui.button("📂 받은 폴더").clicked() {
                        open_folder(&self.settings.download_dir);
                    }
                    if self.settings.auto_accept {
                        ui.colored_label(egui::Color32::from_rgb(220, 140, 0), "자동 수락 켜짐");
                    }
                });
            });
        });

        // ---- log
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(140.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("기록");
                    if ui.small_button("지우기").clicked() {
                        self.shared.lock().unwrap().log.clear();
                    }
                });
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let sh = self.shared.lock().unwrap();
                        for l in &sh.log {
                            ui.horizontal_wrapped(|ui| {
                                ui.weak(&l.time);
                                let color = if l.ok {
                                    egui::Color32::from_rgb(60, 170, 90)
                                } else {
                                    egui::Color32::from_rgb(210, 80, 70)
                                };
                                ui.colored_label(color, if l.ok { "✔" } else { "✘" });
                                ui.label(&l.text);
                                if let Some(f) = &l.folder {
                                    if ui.small_button("열기").clicked() {
                                        open_folder(f);
                                    }
                                }
                            });
                        }
                    });
            });

        // ---- device list
        egui::Panel::left("peers")
            .resizable(true)
            .default_size(270.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("주변 기기");
                    if ui
                        .small_button("🔄")
                        .on_hover_text("지금 다시 찾기 (평소에는 자동으로 갱신됩니다)")
                        .clicked()
                    {
                        self.refresh();
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(ui.available_height() - 70.0)
                    .show(ui, |ui| {
                        if peers.is_empty() {
                            ui.weak("기기를 찾는 중...\n상대 기기에서 AnyDrop을 실행하세요.");
                        }
                        for p in &peers {
                            let sel = self.selected.as_deref() == Some(p.device.id.as_str());
                            let icon = os_icon(&p.device.os);
                            let src = match p.source {
                                PeerSource::Lan => "LAN",
                                PeerSource::Tailscale => "Tailscale",
                                PeerSource::Manual => "직접 입력",
                            };
                            let text =
                                format!("{icon} {}\n    {} · {src}", p.device.name, p.device.os);
                            // Full-width row, text left-aligned.
                            let r = ui
                                .with_layout(
                                    egui::Layout::top_down_justified(egui::Align::LEFT),
                                    |ui| {
                                        ui.add(
                                            egui::Button::selectable(sel, text)
                                                .min_size(egui::vec2(0.0, 40.0)),
                                        )
                                    },
                                )
                                .inner;
                            let r = r.on_hover_text(
                                p.addrs
                                    .iter()
                                    .map(|a| a.to_string())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            );
                            if r.clicked() {
                                self.selected = Some(p.device.id.clone());
                            }
                        }
                    });
                ui.separator();
                ui.label("IP / 호스트 이름으로 추가");
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.manual_addr)
                            .hint_text("192.168.0.10")
                            .desired_width(170.0),
                    );
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("추가").clicked() || enter {
                        self.add_manual();
                    }
                });
            });

        // ---- main area
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("보낼 파일");
            let stroke = if hovering {
                egui::Stroke::new(2.0, ui.visuals().selection.bg_fill)
            } else {
                egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
            };
            let drop_h = (ui.available_height() - 140.0).max(120.0);
            egui::Frame::group(ui.style())
                .stroke(stroke)
                .inner_margin(10.0)
                .show(ui, |ui| {
                    ui.set_min_size(egui::vec2(ui.available_width(), drop_h));
                    if self.files.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(drop_h / 2.0 - 30.0);
                            ui.label(
                                egui::RichText::new("⬇ 파일이나 폴더를 여기에 끌어다 놓으세요")
                                    .size(18.0),
                            );
                            ui.weak("또는 아래 버튼으로 선택");
                        });
                    } else {
                        egui::ScrollArea::vertical()
                            .max_height(drop_h)
                            .show(ui, |ui| {
                                let mut remove = None;
                                for (i, f) in self.files.iter().enumerate() {
                                    ui.horizontal(|ui| {
                                        if ui.small_button("✕").clicked() {
                                            remove = Some(i);
                                        }
                                        let icon = if f.is_dir() { "📁" } else { "📄" };
                                        let name = f
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| f.display().to_string());
                                        ui.strong(format!("{icon} {name}"));
                                        if let Some(parent) = f.parent() {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(
                                                        parent.display().to_string(),
                                                    )
                                                    .weak(),
                                                )
                                                .truncate(),
                                            )
                                            .on_hover_text(f.display().to_string());
                                        }
                                    });
                                }
                                if let Some(i) = remove {
                                    self.files.remove(i);
                                }
                            });
                    }
                });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("📄 파일 선택...").clicked() {
                    if let Some(v) = rfd::FileDialog::new().pick_files() {
                        for p in v {
                            if !self.files.contains(&p) {
                                self.files.push(p);
                            }
                        }
                    }
                }
                if ui.button("📁 폴더 선택...").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        if !self.files.contains(&p) {
                            self.files.push(p);
                        }
                    }
                }
                if !self.files.is_empty() && ui.button("모두 지우기").clicked() {
                    self.files.clear();
                }
            });

            ui.add_space(6.0);
            let target = self
                .selected
                .as_ref()
                .and_then(|id| peers.iter().find(|p| &p.device.id == id))
                .cloned();
            ui.horizontal(|ui| {
                let label = match &target {
                    Some(p) => format!("🚀 {}에게 보내기", p.device.name),
                    None => "왼쪽에서 받을 기기를 선택하세요".to_string(),
                };
                let enabled = target.is_some() && !self.files.is_empty() && sending.is_none();
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::new(egui::RichText::new(label).size(16.0))
                            .min_size(egui::vec2(260.0, 34.0)),
                    )
                    .clicked()
                {
                    if let Some(p) = target.clone() {
                        self.send(p);
                    }
                }
            });

            if let Some((to, p)) = &sending {
                match p {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("{to}의 수락을 기다리는 중..."));
                        });
                    }
                    Some(p) => progress_bar(ui, &format!("보내는 중 → {to}"), p),
                }
            }
            if let Some(p) = &receiving {
                progress_bar(ui, "받는 중", p);
            }
        });

        self.offer_dialog(&ctx);
        self.settings_dialog(&ctx);
        self.auto_accept_dialog(&ctx);
    }

    fn on_exit(&mut self) {
        if let Some(r) = self.receiver.take() {
            r.task.abort();
        }
    }
}

impl AnyDropApp {
    fn offer_dialog(&mut self, ctx: &egui::Context) {
        let mut sh = self.shared.lock().unwrap();
        let Some(offer) = sh.offers.front_mut() else {
            return;
        };
        let mut decision = None;
        egui::Modal::new(egui::Id::unique("anydrop-offer")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("📥 받기 요청");
            ui.label(format!(
                "{} {} ({}) 이(가) {}개 파일, {}을(를) 보내려고 합니다.",
                os_icon(&offer.req.peer.os),
                offer.req.peer.name,
                offer.req.remote.ip(),
                offer.req.files.len(),
                human(offer.req.total_size)
            ));
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .show(ui, |ui| {
                    for f in &offer.req.files {
                        ui.horizontal(|ui| {
                            ui.label(&f.path);
                            ui.weak(human(f.size));
                        });
                    }
                });
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .button(egui::RichText::new("✔ 수락").size(16.0))
                    .clicked()
                {
                    decision = Some(true);
                }
                if ui
                    .button(egui::RichText::new("✘ 거절").size(16.0))
                    .clicked()
                {
                    decision = Some(false);
                }
            });
        });
        if let Some(d) = decision {
            if let Some(tx) = offer.reply.take() {
                let _ = tx.send(d);
            }
            sh.offers.pop_front();
        } else if offer.reply.as_ref().is_some_and(|t| t.is_closed()) {
            // Sender gave up or the receiver timed out.
            sh.offers.pop_front();
        }
    }

    fn set_auto_accept(&mut self, on: bool) {
        self.settings.auto_accept = on;
        self.draft.auto_accept = on;
        self.auto_accept.store(on, Ordering::Relaxed);
        if let Some(t) = &self.tray {
            t.set_auto_accept(on);
        }
        if let Err(e) = self.settings.save() {
            self.shared
                .lock()
                .unwrap()
                .log(false, format!("설정 저장 실패: {e}"), None);
        }
    }

    fn auto_accept_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_auto_accept {
            return;
        }
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("anydrop-auto-accept")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading("자동 수락을 켤까요?");
            ui.colored_label(ui.visuals().warn_fg_color, AUTO_ACCEPT_WARNING);
            ui.label("신뢰하는 네트워크(집, 사무실, Tailscale)에서만 사용하세요.");
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("켜기").clicked() {
                    answer = Some(true);
                }
                if ui.button("취소").clicked() {
                    answer = Some(false);
                }
            });
        });
        if let Some(on) = answer {
            self.confirm_auto_accept = false;
            if on {
                self.set_auto_accept(true);
            }
        }
    }

    fn settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut close = None;
        egui::Modal::new(egui::Id::unique("anydrop-settings")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("설정");
            egui::Grid::new("settings_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("기기 이름");
                    ui.text_edit_singleline(&mut self.draft.name);
                    ui.end_row();

                    ui.label("저장 폴더");
                    ui.horizontal(|ui| {
                        let mut s = self.draft.download_dir.display().to_string();
                        if ui
                            .add(egui::TextEdit::singleline(&mut s).desired_width(230.0))
                            .changed()
                        {
                            self.draft.download_dir = PathBuf::from(s);
                        }
                        if ui.button("선택...").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_directory(&self.draft.download_dir)
                                .pick_folder()
                            {
                                self.draft.download_dir = p;
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("포트 (TCP)");
                    ui.add(egui::DragValue::new(&mut self.draft.port).range(1024..=65535));
                    ui.end_row();

                    ui.label("");
                    ui.checkbox(
                        &mut self.draft.auto_accept,
                        "묻지 않고 자동 수락 (신뢰하는 네트워크에서만)",
                    );
                    ui.end_row();
                    if self.draft.auto_accept {
                        ui.label("");
                        ui.colored_label(ui.visuals().warn_fg_color, AUTO_ACCEPT_WARNING);
                        ui.end_row();
                    }

                    ui.label("최대 수신 크기");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.draft.max_receive_gb)
                                .range(0..=1_000_000)
                                .suffix(" GB"),
                        );
                        ui.weak("0 = 제한 없음");
                    });
                    ui.end_row();

                    if self.tray.is_some() {
                        ui.label("트레이");
                        ui.checkbox(
                            &mut self.draft.minimize_to_tray,
                            "창을 닫으면 트레이로 숨기기 (계속 받기)",
                        );
                        ui.end_row();

                        ui.label("");
                        ui.checkbox(
                            &mut self.draft.start_hidden,
                            "시작할 때 트레이로 숨긴 채 실행",
                        );
                        ui.end_row();
                    }
                });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("저장").clicked() {
                    close = Some(true);
                }
                if ui.button("취소").clicked() {
                    close = Some(false);
                }
            });
        });
        if let Some(save) = close {
            self.show_settings = false;
            if save {
                self.apply_settings();
            }
        }
    }
}

fn progress_bar(ui: &mut egui::Ui, title: &str, p: &Progress) {
    let frac = if p.total_size == 0 {
        1.0
    } else {
        p.total_done as f32 / p.total_size as f32
    };
    ui.label(format!(
        "{title}  [{}/{}] {}",
        p.file_index + 1,
        p.file_count,
        p.path
    ));
    ui.add(egui::ProgressBar::new(frac).show_percentage().text(format!(
        "{} / {}",
        human(p.total_done),
        human(p.total_size)
    )));
}

fn os_icon(os: &str) -> &'static str {
    if os.starts_with("windows") {
        "🖥"
    } else if os.starts_with("macos") {
        "🍎"
    } else {
        "🐧"
    }
}

fn open_folder(p: &std::path::Path) {
    let _ = std::fs::create_dir_all(p);
    #[cfg(windows)]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(p).spawn();
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// egui's built-in fonts have no Hangul glyphs: load a system CJK font as fallback.
fn install_fonts(ctx: &egui::Context) {
    let candidates: &[(&str, u32)] = &[
        // Windows
        (r"C:\Windows\Fonts\malgun.ttf", 0),
        // macOS
        ("/System/Library/Fonts/AppleSDGothicNeo.ttc", 0),
        ("/System/Library/Fonts/Supplemental/AppleGothic.ttf", 0),
        // Linux
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 1),
        ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 1),
        ("/usr/share/fonts/truetype/nanum/NanumGothic.ttf", 0),
    ];
    for (path, index) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            let mut data = egui::FontData::from_owned(bytes);
            data.index = *index;
            fonts.font_data.insert("system-cjk".into(), Arc::new(data));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(fam)
                    .or_default()
                    .push("system-cjk".into());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn icns_container_is_valid() {
        let icns = super::icns::icns_bytes();
        assert_eq!(&icns[0..4], b"icns");
        assert_eq!(
            u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize,
            icns.len()
        );
        // Walk the entries: each is OSType + BE length (incl. header) + PNG.
        let (mut off, mut n) = (8, 0);
        while off < icns.len() {
            let len = u32::from_be_bytes(icns[off + 4..off + 8].try_into().unwrap()) as usize;
            assert_eq!(&icns[off + 8..off + 16], b"\x89PNG\r\n\x1a\n");
            off += len;
            n += 1;
        }
        assert_eq!((off, n), (icns.len(), 10));
    }

    #[test]
    fn ico_container_is_valid() {
        let sizes = [16, 32, 256];
        let ico = super::icon::ico_bytes(&sizes);
        assert_eq!(
            &ico[0..6],
            &[0, 0, 1, 0, 3, 0],
            "ICONDIR: reserved, type=icon, count=3"
        );
        for (i, s) in sizes.iter().enumerate() {
            let e = &ico[6 + i * 16..6 + (i + 1) * 16];
            let dim = if *s == 256 { 0 } else { *s as u8 };
            assert_eq!((e[0], e[1]), (dim, dim));
            let len = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
            let off = u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize;
            assert!(off + len <= ico.len());
            assert_eq!(
                u32::from_le_bytes(ico[off..off + 4].try_into().unwrap()),
                40,
                "BITMAPINFOHEADER"
            );
        }
    }

    #[test]
    fn icon_shape() {
        let (px, w, h) = super::icon::app_icon_rgba(64);
        assert_eq!((w, h, px.len()), (64, 64, 64 * 64 * 4));
        let at = |x: u32, y: u32| &px[((y * w + x) * 4) as usize..((y * w + x) * 4 + 4) as usize];
        assert_eq!(at(0, 0)[3], 0, "corner is transparent");
        assert_eq!(at(32, 24), [255, 255, 255, 255], "arrow stem is white");
        assert_eq!(at(10, 32)[..3], [0x2f, 0x80, 0xed], "disc is blue");
        if let Ok(path) = std::env::var("ANYDROP_ICON_DUMP") {
            std::fs::write(path, &px).unwrap();
        }
    }
}
