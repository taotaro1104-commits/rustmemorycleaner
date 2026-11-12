//! 停止手段の実行と復元（Undo）。実行はサービス側。認可（authorize）を通った手段だけを渡すこと。
use crate::journal::Journal;
use crate::plan::{SERVICE_AUTO, SERVICE_DEMAND, SERVICE_DISABLED};
use crate::{proc, startup, svc, task};
use memopt_core::stop::*;
use memopt_core::{Feature, ProcessInfo};
use std::time::Duration;

const SERVICE_WAIT: Duration = Duration::from_secs(15);

pub struct Ctx<'a> {
    pub journal: &'a Journal,
    pub now_unix: i64,
    pub now_filetime: u64,
    pub avail_before: Option<u64>,
}

fn base(ctx: &Ctx, feature: &Feature, opt: &MethodOption) -> JournalEntry {
    JournalEntry {
        id: ctx.journal.next_id(),
        ts_unix: ctx.now_unix,
        feature_id: feature.id.clone(),
        feature_name: feature.name.clone(),
        method: opt.method,
        target: opt.target.clone(),
        change: Change::None,
        outcome: Outcome::Ok,
        avail_before: ctx.avail_before,
        avail_after: None,
        undone_ts: None,
    }
}

fn need_target(opt: &MethodOption) -> Result<&str, String> {
    opt.target.as_deref().ok_or_else(|| "対象が指定されていません".to_string())
}

/// 強制終了の対象: 機能の PID と、その子孫（プロセスツリー）。起動時刻つきで返す。
fn kill_targets(feature: &Feature, procs: &[ProcessInfo]) -> Vec<(u32, u64)> {
    let mut pids = feature.pids.clone();
    pids.extend(proc::descendants(procs, &feature.pids));
    pids.into_iter().filter_map(|pid| procs.iter().find(|p| p.pid == pid).map(|p| (pid, p.start_time))).collect()
}

/// 1 件を実行してジャーナルに追記し、記録した内容を返す。失敗も記録する（outcome = Failed）。
/// GracefulClose は GUI が実行するため、ここでは「依頼済み」として記録だけ行う。
pub fn execute(ctx: &Ctx, feature: &Feature, procs: &[ProcessInfo], opt: &MethodOption) -> Result<JournalEntry, String> {
    let mut e = base(ctx, feature, opt);
    let result: Result<Change, String> = match opt.method {
        StopMethod::GracefulClose => {
            e.outcome = Outcome::Requested;
            Ok(Change::None)
        }
        StopMethod::ForceKill => {
            let targets = kill_targets(feature, procs);
            let fails: Vec<String> = targets.iter().filter_map(|(pid, st)| proc::kill_verified(*pid, *st).err()).collect();
            if fails.is_empty() { Ok(Change::None) } else { Err(fails.join(" / ")) }
        }
        StopMethod::ServiceStop => need_target(opt)
            .and_then(|n| svc::stop(n, SERVICE_WAIT).map(|dependents| Change::ServiceStopped { name: n.to_string(), dependents })),
        StopMethod::ServiceStartupManual | StopMethod::ServiceStartupDisabled => need_target(opt).and_then(|n| {
            let new = if opt.method == StopMethod::ServiceStartupManual { SERVICE_DEMAND } else { SERVICE_DISABLED };
            svc::set_start_type(n, new, None).map(|(before, delayed_before)| Change::ServiceStartType { name: n.to_string(), before, delayed_before })
        }),
        StopMethod::DisableStartup => need_target(opt).and_then(|n| {
            let sid = feature.targets.startup.iter().find(|s| s.name == n).ok_or("この機能のスタートアップ項目ではありません")?.user_sid.clone();
            startup::disable(sid.as_deref(), n, ctx.now_filetime).map(|before_raw| Change::StartupApproved { user_sid: sid, value_name: n.to_string(), before_raw })
        }),
        StopMethod::DisableTask => need_target(opt).and_then(|t| {
            if !feature.targets.tasks.iter().any(|x| x == t) {
                return Err("この機能のタスクではありません".into());
            }
            task::set_enabled(t, false).map(|before_enabled| Change::TaskEnabled { path: t.to_string(), before_enabled })
        }),
        StopMethod::LowerPriority => {
            let (mut items, mut fails) = (Vec::new(), Vec::new());
            for (pid, st) in kill_targets(feature, procs).into_iter().filter(|(p, _)| feature.pids.contains(p)) {
                match proc::lower_priority(pid, st) {
                    Ok(before) => items.push(PriorityItem { pid, start_time: st, before }),
                    Err(m) => fails.push(m),
                }
            }
            if items.is_empty() && !fails.is_empty() { Err(fails.join(" / ")) } else { Ok(Change::MemoryPriority { items }) }
        }
    };
    match result {
        Ok(c) => e.change = c,
        Err(m) => e.outcome = Outcome::Failed(m),
    }
    ctx.journal.append_entry(&e).map_err(|er| format!("ジャーナルへ書き込めません: {er}"))?;
    Ok(e)
}

/// 変更前の状態へ戻す。復元済みの記録（Undone）は呼び出し側が付ける。
pub fn revert(change: &Change) -> Result<(), String> {
    match change {
        Change::None => Err("この操作は元に戻せません（アプリは再起動してください）".into()),
        Change::ServiceStopped { name, dependents } => {
            svc::start(name, SERVICE_WAIT)?;
            // 依存サービスは本体の後で起動し直す。個別の失敗は集めて報告する。
            let fails: Vec<String> = dependents.iter().filter_map(|d| svc::start(d, SERVICE_WAIT).err()).collect();
            if fails.is_empty() { Ok(()) } else { Err(fails.join(" / ")) }
        }
        Change::ServiceStartType { name, before, delayed_before } => {
            let delayed = (*before == SERVICE_AUTO).then_some(*delayed_before);
            svc::set_start_type(name, *before, delayed).map(|_| ())
        }
        Change::StartupApproved { user_sid, value_name, before_raw } => startup::restore(user_sid.as_deref(), value_name, before_raw.as_deref()),
        Change::TaskEnabled { path, before_enabled } => task::set_enabled(path, *before_enabled).map(|_| ()),
        Change::MemoryPriority { items } => {
            // 終了済みのプロセス（起動時刻が合わない/開けない）は「終了で消える」ので成功扱い。
            for i in items {
                let _ = proc::restore_priority(i.pid, i.start_time, i.before);
            }
            Ok(())
        }
    }
}

/// 1 件を復元し、成功したらジャーナルに記録する。
pub fn undo(journal: &Journal, id: u64, now_unix: i64) -> Result<(), String> {
    let entry = journal.load().into_iter().find(|e| e.id == id).ok_or("その記録はありません")?;
    if entry.undone_ts.is_some() {
        return Err("すでに復元済みです".into());
    }
    if !entry.undoable() {
        return Err("この記録は元に戻せません".into());
    }
    revert(&entry.change)?;
    journal.mark_undone(id, now_unix).map_err(|e| format!("復元は成功しましたが記録に失敗: {e}"))
}

#[derive(Debug, Default, PartialEq)]
pub struct UndoReport {
    pub restored: Vec<u64>,
    pub failed: Vec<(u64, String)>,
}

/// 復元できるものを新しい順にすべて戻す（途中で失敗しても残りは続行）。
pub fn undo_all(journal: &Journal, now_unix: i64) -> UndoReport {
    let mut r = UndoReport::default();
    let mut todo: Vec<_> = journal.load().into_iter().filter(JournalEntry::undoable).collect();
    todo.sort_by_key(|e| std::cmp::Reverse(e.id));
    for e in todo {
        match undo(journal, e.id, now_unix) {
            Ok(()) => r.restored.push(e.id),
            Err(m) => r.failed.push((e.id, m)),
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use memopt_core::stop::StartupRef;
    use memopt_core::{FeatureKind, SafetyClass};
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::*;

    fn tmp_journal(tag: &str) -> Journal {
        let d = std::env::temp_dir().join(format!("memopt_exec_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Journal::open(&d.join("journal.jsonl"))
    }

    fn feature(pids: Vec<u32>) -> Feature {
        Feature {
            id: "app:x".into(), name: "X".into(), kind: FeatureKind::DesktopApp, pids, private_bytes: 0, working_set: 0,
            private_working_set: 0, safety: SafetyClass::Stoppable, startup: false, targets: StopTargets::default(),
        }
    }

    fn opt(method: StopMethod, target: Option<&str>) -> MethodOption {
        MethodOption { method, target: target.map(str::to_string), persistent: method.persistent(), available: true, note: None, recommended: false }
    }

    fn ctx(j: &Journal) -> Ctx<'_> {
        Ctx { journal: j, now_unix: 1000, now_filetime: 0x01D0_0000_0000_0000, avail_before: Some(7) }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    #[test]
    fn force_kill_terminates_a_child_tree_and_journals_it_as_not_undoable() {
        let mut child = std::process::Command::new("cmd.exe").args(["/c", "ping -n 60 127.0.0.1 >nul"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(800)); // 孫プロセス(ping)が立ち上がるのを待つ
        let procs = memopt_probe::processes();
        let f = feature(vec![child.id()]);
        let j = tmp_journal("kill");
        let e = execute(&ctx(&j), &f, &procs, &opt(StopMethod::ForceKill, None)).unwrap();
        assert_eq!(e.outcome, Outcome::Ok, "{e:?}");
        assert!(child.wait().is_ok());
        let after = memopt_probe::processes();
        assert!(!after.iter().any(|p| p.parent_pid == child.id() && p.exe_name.eq_ignore_ascii_case("ping.exe") && p.start_time >= procs.iter().find(|x| x.pid == child.id()).unwrap().start_time),
            "子孫の ping も終了しているはず");
        assert!(!e.undoable());
        assert_eq!(undo(&j, e.id, 1).unwrap_err(), "この記録は元に戻せません");
        assert_eq!(j.load().len(), 1);
    }

    #[test]
    fn failed_actions_are_journaled_with_the_reason_and_are_not_undoable() {
        let j = tmp_journal("fail");
        let f = feature(vec![]);
        let e = execute(&ctx(&j), &f, &[], &opt(StopMethod::ServiceStop, Some("MemOptNoSuchService"))).unwrap();
        assert!(matches!(e.outcome, Outcome::Failed(_)));
        assert!(!e.undoable());
        // 対象なし・計画外の対象は実行前に弾かれ、失敗として記録される。
        let e2 = execute(&ctx(&j), &f, &[], &opt(StopMethod::DisableStartup, None)).unwrap();
        assert!(matches!(e2.outcome, Outcome::Failed(_)));
        let e3 = execute(&ctx(&j), &f, &[], &opt(StopMethod::DisableTask, Some(r"\Evil\Task"))).unwrap();
        assert!(matches!(e3.outcome, Outcome::Failed(ref m) if m.contains("タスクではありません")));
    }

    #[test]
    fn graceful_close_is_journaled_as_requested() {
        let j = tmp_journal("graceful");
        let e = execute(&ctx(&j), &feature(vec![1]), &[], &opt(StopMethod::GracefulClose, None)).unwrap();
        assert_eq!(e.outcome, Outcome::Requested);
        assert!(!e.pending_persistent() && !e.undoable());
    }

    #[test]
    fn disable_startup_then_undo_restores_the_real_registry_value() {
        // 自分の HKCU（= HKEY_USERS\<自分の SID>）に使い捨ての Run 値を作り、サービス経路と同じ処理で往復する。
        let sid = startup::current_user_sid().expect("sid");
        let name = format!("MemOptTest{}", std::process::id());
        unsafe {
            let mut k = HKEY::default();
            let sub = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
            assert_eq!(RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), 0, KEY_SET_VALUE, &mut k).0, 0);
            let v = wide(r#""C:\nonexistent\memopt_test.exe""#);
            let bytes = std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2);
            assert_eq!(RegSetValueExW(k, PCWSTR(wide(&name).as_ptr()), 0, REG_SZ, Some(bytes)).0, 0);
            let _ = RegCloseKey(k);
        }
        let cleanup = || unsafe {
            let mut k = HKEY::default();
            let run = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
            if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(run.as_ptr()), 0, KEY_SET_VALUE, &mut k).0 == 0 {
                let _ = RegDeleteValueW(k, PCWSTR(wide(&name).as_ptr()));
                let _ = RegCloseKey(k);
            }
            let mut k = HKEY::default();
            let ap = wide(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run");
            if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(ap.as_ptr()), 0, KEY_SET_VALUE, &mut k).0 == 0 {
                let _ = RegDeleteValueW(k, PCWSTR(wide(&name).as_ptr()));
                let _ = RegCloseKey(k);
            }
        };

        let mut f = feature(vec![]);
        f.targets.startup.push(StartupRef { name: name.clone(), user_sid: Some(sid.clone()) });
        let j = tmp_journal("startup");
        let e = execute(&ctx(&j), &f, &[], &opt(StopMethod::DisableStartup, Some(&name))).unwrap();
        let entries = probe_entries(&name);
        let r = (|| -> Result<(), String> {
            assert_eq!(e.outcome, Outcome::Ok, "{e:?}");
            assert!(e.pending_persistent());
            assert!(!entries.is_empty() && entries.iter().all(|x| !x.enabled), "無効化がプローブから見えるはず: {entries:?}");
            undo(&j, e.id, 2000)?;
            assert!(probe_entries(&name).iter().all(|x| x.enabled), "復元後は有効に戻る");
            assert_eq!(j.load()[0].undone_ts, Some(2000));
            assert_eq!(undo(&j, e.id, 3).unwrap_err(), "すでに復元済みです");
            Ok(())
        })();
        cleanup();
        r.unwrap();
    }

    fn probe_entries(name: &str) -> Vec<memopt_core::StartupEntry> {
        memopt_probe::startup_entries().into_iter().filter(|e| e.name == name).collect()
    }

    #[test]
    fn undo_all_restores_newest_first_and_continues_past_failures() {
        let j = tmp_journal("undoall");
        let mk = |id: u64, change: Change, method: StopMethod| JournalEntry {
            id, ts_unix: 0, feature_id: "f".into(), feature_name: "F".into(), method, target: None, change,
            outcome: Outcome::Ok, avail_before: None, avail_after: None, undone_ts: None,
        };
        // 1: 存在しないタスク（復元に失敗する）、2: 優先度（常に成功扱い）、3: 元に戻せない操作（対象外）
        j.append_entry(&mk(1, Change::TaskEnabled { path: r"\MemOpt\Nope".into(), before_enabled: true }, StopMethod::DisableTask)).unwrap();
        j.append_entry(&mk(2, Change::MemoryPriority { items: vec![] }, StopMethod::LowerPriority)).unwrap();
        j.append_entry(&mk(3, Change::None, StopMethod::ForceKill)).unwrap();
        let r = undo_all(&j, 9);
        assert_eq!(r.restored, vec![2]);
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].0, 1);
        let after = j.load();
        assert_eq!(after.iter().find(|e| e.id == 2).unwrap().undone_ts, Some(9));
        assert_eq!(after.iter().find(|e| e.id == 1).unwrap().undone_ts, None); // 失敗したものは未復元のまま残る
    }
}
