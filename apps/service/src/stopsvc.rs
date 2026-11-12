//! 停止要求の解決: 最新のプロセス一覧から機能を引き直し、停止対象と安全分類（実行時判定）を求める。
//! パイプ越しの要求は信用せず、ここで必ずサービス側が再照合する（設計書6章）。
use crate::sync::{LockExt, RwLockExt};
use crate::state::{Shared, TASKS_TTL_SECS};
use memopt_actions::plan::{build_plan, quick_safety, Facts, ServiceFacts, SERVICE_AUTO};
use memopt_actions::{svc, task};
use memopt_core::stop::{StartupRef, StopPlan, StopTargets};
use memopt_core::{Feature, ProcessInfo};
use std::time::{Duration, Instant};

pub struct Resolved {
    pub feature: Feature,
    pub procs: Vec<ProcessInfo>,
    pub plan: StopPlan,
}

fn same_path(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn cached_tasks(shared: &Shared) -> Vec<task::TaskInfo> {
    let mut g = shared.tasks.lock_ok();
    if let Some((at, v)) = g.as_ref() {
        if at.elapsed() < Duration::from_secs(TASKS_TTL_SECS) {
            return v.clone();
        }
    }
    // 取得に失敗してもタスクの選択肢が無くなるだけで、他の手段は使える。
    let v = task::list_tasks().unwrap_or_else(|e| {
        tracing::warn!("タスク一覧を取得できません: {e}");
        vec![]
    });
    *g = Some((Instant::now(), v.clone()));
    v
}

fn targets_for(procs: &[&ProcessInfo], shared: &Shared) -> StopTargets {
    let mut t = StopTargets::default();
    for p in procs {
        for s in &p.services {
            if !t.services.contains(s) {
                t.services.push(s.clone());
            }
        }
    }
    let paths: Vec<&str> = procs.iter().filter_map(|p| p.exe_path.as_deref()).collect();
    for e in memopt_probe::startup_entries().into_iter().filter(|e| e.enabled) {
        if paths.iter().any(|p| same_path(p, &e.exe_path)) {
            t.startup.push(StartupRef { name: e.name, user_sid: e.user_sid });
        }
    }
    for tk in cached_tasks(shared).into_iter().filter(|t| t.enabled) {
        if tk.exe_path.as_deref().is_some_and(|x| paths.iter().any(|p| same_path(p, x))) {
            t.tasks.push(tk.path);
        }
    }
    t
}

/// 一覧表示用: 機能 + スタートアップの印 + 実行時の安全分類（SCM を叩かない軽い判定）。
pub fn list_features(shared: &Shared) -> Vec<Feature> {
    let procs = memopt_probe::processes();
    let mut features = memopt_grouping::group(&procs);
    memopt_grouping::mark_startup(&mut features, &procs, &memopt_probe::startup_entries());
    let user = shared.cfg.read_ok().protect.user_paths.clone();
    for f in features.iter_mut() {
        let fp: Vec<&ProcessInfo> = procs.iter().filter(|p| f.pids.contains(&p.pid)).collect();
        f.safety = quick_safety(f, &fp, &user);
    }
    features
}

/// `feature_id` の機能を最新の状態で引き直し、停止計画まで組み立てる。
pub fn resolve(shared: &Shared, feature_id: &str) -> Result<Resolved, String> {
    let procs = memopt_probe::processes();
    let mut feature = memopt_grouping::group(&procs)
        .into_iter()
        .find(|f| f.id == feature_id)
        .ok_or("機能が見つかりません（すでに終了した可能性があります）")?;
    let fprocs: Vec<&ProcessInfo> = procs.iter().filter(|p| feature.pids.contains(&p.pid)).collect();
    feature.targets = targets_for(&fprocs, shared);
    let user = shared.cfg.read_ok().protect.user_paths.clone();
    // 照会できないサービス（権限不足など）は「停止要求を受け付けない」として扱い、安全側（保護）に倒す。
    let services: Vec<ServiceFacts> = feature
        .targets
        .services
        .iter()
        .map(|n| {
            svc::query(n).unwrap_or(ServiceFacts { name: n.clone(), accepts_stop: false, running: true, start_type: SERVICE_AUTO, dependents: vec![] })
        })
        .collect();
    let plan = build_plan(&Facts { feature: &feature, procs: fprocs, services, user_protected: &user });
    Ok(Resolved { feature, procs, plan })
}
