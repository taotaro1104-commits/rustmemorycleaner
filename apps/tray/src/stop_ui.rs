//! 停止の確認ダイアログと変更履歴（設計書5章）。
//! 分類ごとの確認: 停止可 = 1 回の確認 / 注意 = 影響の説明 + 理解のチェック / 永続変更 = さらに 2 段階目の確認。
use crate::fmt::ago;
use crate::service_link::{Cmd, Link, View};
use crate::stop_cmds::{describe_entry, StopCmd};
use eframe::egui;
use memopt_core::stop::{MethodOption, Outcome, StopMethod, StopPlan};
use memopt_core::SafetyClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Choose,
    /// 永続変更の 2 段階目（「本当に変更しますか？」）。
    ConfirmPersistent,
}

#[derive(Default)]
pub struct StopUi {
    pub dialog: Option<Dialog>,
    pub show_journal: bool,
    confirm_undo_all: bool,
}

pub struct Dialog {
    feature_id: String,
    prefer_force: bool,
    selected: Option<usize>,
    ack: bool,
    step: Step,
}

/// 既定で選ぶ手段: 推奨（最も穏やか）で使えるもの。強制終了の再確認時は強制終了。
pub fn default_selection(plan: &StopPlan, prefer_force: bool) -> Option<usize> {
    if prefer_force {
        if let Some(i) = plan.options.iter().position(|o| o.method == StopMethod::ForceKill && o.available) {
            return Some(i);
        }
    }
    plan.options.iter().position(|o| o.recommended && o.available).or_else(|| plan.options.iter().position(|o| o.available))
}

/// 実行ボタンを有効にしてよいか。「注意」分類は影響を理解したことのチェックが必要。
pub fn can_execute(plan: &StopPlan, selected: Option<usize>, ack: bool) -> bool {
    let Some(o) = selected.and_then(|i| plan.options.get(i)) else { return false };
    o.available && (plan.safety != SafetyClass::Caution || ack)
}

/// 永続変更は 2 段階目の確認を挟む。
pub fn needs_second_step(o: &MethodOption) -> bool {
    o.persistent
}

fn option_label(o: &MethodOption) -> String {
    let base = match &o.target {
        Some(t) => format!("{} — {t}", o.method.label()),
        None => o.method.label().to_string(),
    };
    if o.persistent { format!("{base}（永続）") } else { base }
}

impl StopUi {
    /// 行の「停止…」から開く。計画の取得をサービスへ依頼する。
    pub fn open(&mut self, link: &Link, feature_id: &str, prefer_force: bool) {
        self.dialog = Some(Dialog { feature_id: feature_id.to_string(), prefer_force, selected: None, ack: false, step: Step::Choose });
        link.send(Cmd::Stop(StopCmd::Plan(feature_id.to_string())));
    }

    fn close(&mut self, link: &Link) {
        self.dialog = None;
        link.send(Cmd::Stop(StopCmd::ClearPlan));
    }

    pub fn show_dialog(&mut self, ctx: &egui::Context, v: &View, link: &Link) {
        let Some(d) = self.dialog.as_mut() else { return };
        let Some(plan) = v.plan.as_ref().filter(|p| p.feature_id == d.feature_id) else {
            egui::Window::new("停止の準備中…").collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label("サービスに停止計画を問い合わせています…");
            });
            return;
        };
        if d.selected.is_none() && d.step == Step::Choose {
            d.selected = default_selection(plan, d.prefer_force);
        }
        let mut action: Option<Action> = None;
        egui::Window::new(format!("{} を停止", plan.name)).collapsible(false).resizable(false).show(ctx, |ui| {
            let (label, color) = match plan.safety {
                SafetyClass::Protected => ("保護（停止できません）", egui::Color32::from_rgb(198, 40, 40)),
                SafetyClass::Caution => ("注意（影響を確認してください）", egui::Color32::from_rgb(224, 138, 0)),
                SafetyClass::Stoppable => ("停止可", egui::Color32::from_rgb(46, 125, 50)),
            };
            ui.colored_label(color, label);
            for i in &plan.impacts {
                ui.label(format!("・{i}"));
            }
            if d.step == Step::ConfirmPersistent {
                let o = &plan.options[d.selected.unwrap_or(0)];
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(224, 138, 0), format!("「{}」は次回以降にも影響する永続的な変更です。", option_label(o)));
                ui.label("変更は記録され、「変更履歴」からいつでも元に戻せます。");
                ui.horizontal(|ui| {
                    if ui.button("変更する").clicked() {
                        action = Some(Action::Run);
                    }
                    if ui.button("戻る").clicked() {
                        d.step = Step::Choose;
                    }
                });
                return;
            }
            if plan.options.is_empty() {
                ui.label("この機能に使える停止手段はありません。");
            }
            ui.separator();
            for (i, o) in plan.options.iter().enumerate() {
                ui.add_enabled_ui(o.available, |ui| {
                    ui.radio_value(&mut d.selected, Some(i), option_label(o));
                });
                if let Some(n) = &o.note {
                    ui.indent(i, |ui| ui.weak(n));
                }
            }
            if plan.safety == SafetyClass::Caution {
                ui.checkbox(&mut d.ack, "影響を理解しました");
            }
            ui.horizontal(|ui| {
                let ok = can_execute(plan, d.selected, d.ack);
                if ui.add_enabled(ok, egui::Button::new("実行")).clicked() {
                    let persistent = d.selected.and_then(|i| plan.options.get(i)).is_some_and(needs_second_step);
                    action = Some(if persistent { Action::SecondStep } else { Action::Run });
                }
                if ui.button("キャンセル").clicked() {
                    action = Some(Action::Cancel);
                }
            });
        });
        match action {
            Some(Action::Cancel) => self.close(link),
            Some(Action::SecondStep) => d.step = Step::ConfirmPersistent,
            Some(Action::Run) => {
                if let Some(o) = d.selected.and_then(|i| plan.options.get(i)) {
                    link.send(Cmd::Stop(StopCmd::Execute {
                        feature_id: plan.feature_id.clone(),
                        method: o.method,
                        target: o.target.clone(),
                        confirmed: o.persistent, // 2 段階目を通った場合だけここに来る
                    }));
                }
                self.dialog = None;
            }
            None => {}
        }
    }

    /// 通知行: 直近の結果、元に戻すボタン、強制終了の再確認導線。
    pub fn notice_bar(&mut self, ui: &mut egui::Ui, v: &View, link: &Link) {
        if let Some(n) = &v.notice {
            ui.label(n);
        }
        ui.horizontal(|ui| {
            if let Some(e) = v.last_stop.as_ref().filter(|e| e.undoable()) {
                if ui.button(format!("元に戻す（{}）", e.method.label())).clicked() {
                    link.send(Cmd::Stop(StopCmd::Undo(e.id)));
                }
            }
            if let Some(id) = &v.force_hint {
                if ui.button("強制終了を確認…").clicked() {
                    self.open(link, id, true);
                }
            }
            ui.checkbox(&mut self.show_journal, "変更履歴");
        });
    }

    pub fn show_journal(&mut self, ctx: &egui::Context, v: &View, link: &Link) {
        if !self.show_journal {
            return;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let mut open = true;
        egui::Window::new("変更履歴").open(&mut open).default_width(720.0).show(ctx, |ui| {
            let pending = v.journal.iter().filter(|e| e.undoable()).count();
            ui.horizontal(|ui| {
                ui.label(format!("{} 件（元に戻せる変更 {pending} 件）", v.journal.len()));
                if !self.confirm_undo_all {
                    if ui.add_enabled(pending > 0, egui::Button::new("すべての変更を元に戻す")).clicked() {
                        self.confirm_undo_all = true;
                    }
                } else {
                    ui.colored_label(egui::Color32::from_rgb(224, 138, 0), format!("{pending} 件を元に戻します。よろしいですか？"));
                    if ui.button("元に戻す").clicked() {
                        link.send(Cmd::Stop(StopCmd::UndoAll));
                        self.confirm_undo_all = false;
                    }
                    if ui.button("やめる").clicked() {
                        self.confirm_undo_all = false;
                    }
                }
            });
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                egui::Grid::new("journal").striped(true).show(ui, |ui| {
                    for h in ["時刻", "機能", "手段", "状態", ""] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for e in &v.journal {
                        ui.label(ago(now, e.ts_unix));
                        ui.label(&e.feature_name);
                        ui.label(match &e.target {
                            Some(t) => format!("{} ({t})", e.method.label()),
                            None => e.method.label().to_string(),
                        });
                        ui.label(if e.undone_ts.is_some() {
                            "復元済み".to_string()
                        } else if matches!(e.outcome, Outcome::Ok) {
                            describe_entry(e).split(": ").nth(1).unwrap_or("完了").to_string()
                        } else {
                            describe_entry(e)
                        });
                        if e.undoable() && ui.button("元に戻す").clicked() {
                            link.send(Cmd::Stop(StopCmd::Undo(e.id)));
                        }
                        ui.end_row();
                    }
                });
            });
        });
        self.show_journal = open;
    }
}

enum Action {
    Run,
    SecondStep,
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(method: StopMethod, available: bool, recommended: bool, persistent: bool) -> MethodOption {
        MethodOption { method, target: None, persistent, available, note: None, recommended }
    }

    fn plan(safety: SafetyClass, options: Vec<MethodOption>) -> StopPlan {
        StopPlan { feature_id: "f".into(), name: "F".into(), safety, process_count: 1, est_release_bytes: 0, dependents: vec![], options, impacts: vec![] }
    }

    #[test]
    fn default_selection_prefers_recommended_then_any_available() {
        let p = plan(SafetyClass::Stoppable, vec![
            opt(StopMethod::ForceKill, true, false, false),
            opt(StopMethod::GracefulClose, true, true, false),
        ]);
        assert_eq!(default_selection(&p, false), Some(1));
        assert_eq!(default_selection(&p, true), Some(0)); // 強制終了の再確認
        let none = plan(SafetyClass::Stoppable, vec![opt(StopMethod::GracefulClose, false, true, false), opt(StopMethod::LowerPriority, true, false, false)]);
        assert_eq!(default_selection(&none, false), Some(1));
        assert_eq!(default_selection(&plan(SafetyClass::Protected, vec![]), false), None);
    }

    #[test]
    fn caution_requires_acknowledgement_and_unavailable_never_runs() {
        let p = plan(SafetyClass::Caution, vec![opt(StopMethod::ServiceStop, true, true, false), opt(StopMethod::DisableTask, false, false, true)]);
        assert!(!can_execute(&p, Some(0), false));
        assert!(can_execute(&p, Some(0), true));
        assert!(!can_execute(&p, Some(1), true)); // 使えない手段
        assert!(!can_execute(&p, None, true));
        let s = plan(SafetyClass::Stoppable, vec![opt(StopMethod::GracefulClose, true, true, false)]);
        assert!(can_execute(&s, Some(0), false)); // 停止可は 1 回の確認でよい
    }

    #[test]
    fn only_persistent_options_get_a_second_confirmation() {
        assert!(needs_second_step(&opt(StopMethod::DisableStartup, true, false, true)));
        assert!(!needs_second_step(&opt(StopMethod::ForceKill, true, false, false)));
    }

    #[test]
    fn option_labels_mark_persistent_changes_and_targets() {
        let mut o = opt(StopMethod::ServiceStartupDisabled, true, false, true);
        o.target = Some("WSearch".into());
        assert_eq!(option_label(&o), "スタートアップを無効に変更 — WSearch（永続）");
    }
}
