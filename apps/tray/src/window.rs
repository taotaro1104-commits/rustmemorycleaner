//! ダッシュボード（egui）。サービスに接続していれば操作ボタン付き、無ければ読み取り専用の監視モード。
use crate::fmt::{ago, gb, trigger_label};
use crate::service_link::{Cmd, Link, Options, View};
use crate::stop_ui::StopUi;
use eframe::egui;
use memopt_core::{Feature, Level, MemorySnapshot, SafetyClass};
use memopt_grouping::TrendTracker;
use std::time::{Duration, Instant};

const SCAN_INTERVAL: Duration = Duration::from_secs(2); // GUI 表示中は 2 秒更新

/// egui 既定フォントに日本語が無いため、Windows 同梱の CJK フォントを後ろに追加する。
pub fn install_cjk_font(ctx: &egui::Context) {
    for name in ["YuGothM.ttc", "meiryo.ttc", "msgothic.ttc"] {
        let path = std::path::Path::new(r"C:\Windows\Fonts").join(name);
        if let Ok(bytes) = std::fs::read(&path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert("cjk".into(), egui::FontData::from_owned(bytes));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(fam).or_default().push("cjk".into());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

pub fn scan_local() -> (Option<MemorySnapshot>, Vec<Feature>) {
    let procs = memopt_probe::processes();
    let mut features = memopt_grouping::group(&procs);
    memopt_grouping::mark_startup(&mut features, &procs, &memopt_probe::startup_entries());
    (memopt_probe::memory_snapshot(), features)
}

/// 推移ミニグラフ（直近30分）。
fn sparkline(ui: &mut egui::Ui, series: &[u64]) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(90.0, 18.0), egui::Sense::hover());
    if series.len() < 2 {
        return;
    }
    let (lo, hi) = (*series.iter().min().unwrap(), *series.iter().max().unwrap());
    let span = (hi - lo).max(1) as f32;
    let pts: Vec<egui::Pos2> = series
        .iter()
        .enumerate()
        .map(|(i, v)| {
            egui::pos2(
                rect.left() + rect.width() * i as f32 / (series.len() - 1) as f32,
                rect.bottom() - rect.height() * (*v - lo) as f32 / span,
            )
        })
        .collect();
    ui.painter().add(egui::Shape::line(pts, egui::Stroke::new(1.2_f32, ui.visuals().text_color())));
}

pub struct App {
    link: Link,
    local: (Option<MemorySnapshot>, Vec<Feature>),
    last_scan: Instant,
    start: Instant,
    trend: TrendTracker,
    last_trend: Option<Instant>,
    confirm_l3: bool,
    stop_ui: StopUi,
}

impl App {
    pub fn new() -> Self {
        let link = crate::service_link::spawn(Options {
            features_every: Some(SCAN_INTERVAL),
            report_session: false, // セッション情報はトレイ常駐側が中継する
            poll_every: SCAN_INTERVAL,
        });
        Self {
            link,
            local: scan_local(),
            last_scan: Instant::now(),
            start: Instant::now(),
            trend: TrendTracker::new(),
            last_trend: None,
            confirm_l3: false,
            stop_ui: StopUi::default(),
        }
    }

    /// 接続中はサービスの値、未接続ならローカル計測。
    fn data(&mut self, v: &View) -> (Option<MemorySnapshot>, Vec<Feature>) {
        if v.connected {
            return (v.mem, v.features.clone());
        }
        if self.last_scan.elapsed() >= SCAN_INTERVAL {
            self.local = scan_local();
            self.last_scan = Instant::now();
        }
        self.local.clone()
    }

    fn controls(&mut self, ui: &mut egui::Ui, v: &View) {
        ui.horizontal(|ui| {
            if ui.button("今すぐ軽く解放 (L1)").clicked() {
                self.link.send(Cmd::RunClean(Level::L1));
            }
            if ui.button("低優先度スタンバイ解放 (L2)").on_hover_text("管理者権限のサービスで実行します").clicked() {
                self.link.send(Cmd::RunClean(Level::L2));
            }
            if ui.button("スタンバイ全解放 (L3)…").on_hover_text("ファイルの再読み込みが増え、一時的に遅くなることがあります").clicked() {
                self.confirm_l3 = true;
            }
            ui.separator();
            if v.paused {
                if ui.button("自動解放を再開").clicked() {
                    self.link.send(Cmd::Pause(0));
                }
            } else if ui.button("一時停止 (1 時間)").clicked() {
                self.link.send(Cmd::Pause(3600));
            }
            let label = if v.emergency_stop { "緊急停止を解除" } else { "緊急停止" };
            if ui.button(label).on_hover_text("自動解放を即時に無効化します（手動解放は可能）").clicked() {
                self.link.send(Cmd::EmergencyStop(!v.emergency_stop));
            }
        });
    }

    fn l3_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_l3 {
            return;
        }
        egui::Window::new("スタンバイ全解放の確認").collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label("スタンバイリストと変更ページを解放します。");
            ui.label("先読みキャッシュが失われ、ファイルの再読み込みで一時的に遅くなることがあります。");
            ui.horizontal(|ui| {
                if ui.button("実行する").clicked() {
                    self.link.send(Cmd::RunClean(Level::L3));
                    self.confirm_l3 = false;
                }
                if ui.button("キャンセル").clicked() {
                    self.confirm_l3 = false;
                }
            });
        });
    }

    fn history(ui: &mut egui::Ui, v: &View) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        egui::ScrollArea::vertical().id_salt("history").max_height(130.0).show(ui, |ui| {
            egui::Grid::new("hist").striped(true).show(ui, |ui| {
                for h in ["時刻", "レベル", "契機", "空きメモリ", "スタンバイ", "60 秒後の空き", "メモ"] {
                    ui.strong(h);
                }
                ui.end_row();
                for r in &v.history {
                    ui.label(ago(now, r.ts_unix));
                    ui.label(format!("L{}", r.level));
                    ui.label(trigger_label(&r.trigger));
                    ui.label(format!("{} → {}", gb(r.avail_before), gb(r.avail_after)));
                    ui.label(match (r.standby_before, r.standby_after) {
                        (Some(a), Some(b)) => format!("{} → {}", gb(a), gb(b)),
                        _ => "-".into(),
                    });
                    ui.label(r.avail_after60.map(gb).unwrap_or_else(|| "-".into()));
                    // 自動調整で控えめにした理由（なぜ控えめにしたか）。
                    ui.label(r.note.as_deref().unwrap_or(""));
                    ui.end_row();
                }
            });
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        ctx.request_repaint_after(SCAN_INTERVAL);
        let v = self.link.snapshot();
        let (mem, features) = self.data(&v);
        // 推移は 30 秒間隔で記録（窓 30 分で約 60 点）。
        if self.last_trend.is_none_or(|t| t.elapsed() >= Duration::from_secs(30)) {
            self.trend.record(self.start.elapsed().as_secs(), features.iter().map(|f| (f.id.as_str(), f.private_bytes)));
            self.last_trend = Some(Instant::now());
        }

        egui::TopBottomPanel::top("mem").show(ctx, |ui| {
            if v.connected {
                ui.colored_label(egui::Color32::from_rgb(46, 125, 50), "● 常駐サービスに接続中");
                self.controls(ui, &v);
                if v.emergency_stop {
                    ui.colored_label(egui::Color32::from_rgb(198, 40, 40), "緊急停止中: 自動解放は無効です");
                } else if v.paused {
                    ui.colored_label(egui::Color32::from_rgb(224, 138, 0), "一時停止中");
                } else if v.pressure {
                    ui.colored_label(egui::Color32::from_rgb(224, 138, 0), "メモリ圧迫を検知中");
                }
                if !v.adaptive.is_empty() {
                    // 効果測定の結果、自動で控えめにしている設定。利用者がいつでも手動で戻せる。
                    ui.horizontal(|ui| {
                        ui.colored_label(egui::Color32::from_rgb(224, 138, 0), format!("自動調整中: {}", v.adaptive.join(" / ")));
                        if ui.small_button("通常に戻す").clicked() {
                            self.link.send(Cmd::ResetAdaptive);
                        }
                    });
                }
            } else {
                ui.colored_label(egui::Color32::GRAY, "○ 監視モード（サービス未接続・読み取り専用）");
            }
            if let Some(m) = &mem {
                let used = m.total_phys.saturating_sub(m.avail_phys);
                ui.add(egui::ProgressBar::new(used as f32 / m.total_phys.max(1) as f32).text(format!(
                    "物理メモリ使用 {} / {}（空き {:.0}%）",
                    gb(used),
                    gb(m.total_phys),
                    m.avail_pct()
                )));
                ui.label(format!("コミット {} / {}（{:.0}%）", gb(m.commit_total), gb(m.commit_limit), m.commit_pct()));
            }
        });
        egui::TopBottomPanel::bottom("hist").show(ctx, |ui| {
            if v.connected {
                self.stop_ui.notice_bar(ui, &v, &self.link);
                Self::history(ui, &v);
            } else if let Some(n) = &v.notice {
                ui.label(n);
            }
        });
        let mut open_stop: Option<String> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            let total: u64 = features.iter().map(|f| f.private_bytes).sum();
            ui.label(format!("機能 {} 件 / プライベート合計 {}", features.len(), gb(total)));
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("features").striped(true).num_columns(8).show(ui, |ui| {
                    for h in ["名前", "種類", "プライベート", "ワーキングセット", "解放見込み", "推移(30分)", "プロセス数", "安全分類"] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for f in &features {
                        ui.horizontal(|ui| {
                            // 操作ボタンは名前の左に置く（横に長い表でも常に見える）。
                            // 保護は操作ボタンを出さず、理由を示す。未接続（読み取り専用）でも出さない。
                            if f.safety == SafetyClass::Protected {
                                ui.weak(" — ").on_hover_text("停止できません: 組み込みの保護対象 / 保護プロセス / 保護リスト / 停止要求を受け付けないサービス / 本ツール自身");
                            } else if v.connected {
                                if ui.small_button("停止…").clicked() {
                                    open_stop = Some(f.id.clone());
                                }
                            } else {
                                ui.weak(" ");
                            }
                            ui.label(&f.name);
                            if f.startup {
                                ui.weak("[起動時]");
                            }
                            if self.trend.is_rising(&f.id) {
                                ui.colored_label(egui::Color32::from_rgb(220, 120, 0), "増加中");
                            }
                        });
                        ui.label(format!("{:?}", f.kind));
                        ui.label(gb(f.private_bytes));
                        ui.label(gb(f.working_set)); // 共有ページを含むため合計は出さない
                        ui.label(format!("約 {}", gb(f.private_working_set))); // 見込み値
                        sparkline(ui, &self.trend.series(&f.id));
                        ui.label(f.pids.len().to_string());
                        ui.label(match f.safety {
                            SafetyClass::Protected => "保護",
                            SafetyClass::Caution => "注意",
                            SafetyClass::Stoppable => "停止可",
                        });
                        ui.end_row();
                    }
                });
            });
        });
        if let Some(id) = open_stop {
            self.stop_ui.open(&self.link, &id, false);
        }
        self.stop_ui.show_dialog(ctx, &v, &self.link);
        self.stop_ui.show_journal(ctx, &v, &self.link);
        self.l3_dialog(ctx);
    }
}

pub fn run() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1180.0, 700.0]).with_title("MemOpt"),
        ..Default::default()
    };
    eframe::run_native(
        "MemOpt",
        opts,
        Box::new(|cc| {
            install_cjk_font(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}
