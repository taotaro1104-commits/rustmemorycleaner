//! 停止計画の組み立てと認可（純粋ロジック）。設計書5・6章:
//! 安全分類は「組み込み → ユーザー設定 → 実行時」の最も厳しいものを採用し、サービス側で必ず再照合する。
use memopt_core::stop::{MethodOption, StopMethod, StopPlan};
use memopt_core::{Feature, FeatureKind, ProcessInfo, SafetyClass};

pub const SERVICE_AUTO: u32 = 2;
pub const SERVICE_DEMAND: u32 = 3;
pub const SERVICE_DISABLED: u32 = 4;

/// ネットワーク・音声・印刷など、止めると影響が広いサービス（注意扱い）。
const CAUTION_SERVICES: &[&str] = &[
    "audiosrv", "audioendpointbuilder", "dnscache", "dhcp", "nlasvc", "netprofm", "spooler", "wlansvc",
    "lanmanserver", "lanmanworkstation", "bits", "wuauserv", "eventlog", "profsvc", "themes", "winmgmt",
];

/// 止めると OS が不安定になるサービス。停止要求を受け付けても、本ツールは保護扱いにする
/// （設計書より保守的な運用判断。必要なら見直す）。
const CRITICAL_SERVICES: &[&str] = &[
    "rpcss", "dcomlaunch", "rpcepmapper", "eventlog", "schedule", "plugplay", "power", "samss", "lsm", "winmgmt",
    "cryptsvc", "bfe", "mpssvc", "windefend", "wscsvc", "sens", "gpsvc", "brokerinfrastructure", "systemeventsbroker",
    "userprofile", "profsvc", "trustedinstaller", "tiledatamodelsvc",
];

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceFacts {
    pub name: String,
    pub accepts_stop: bool,
    pub running: bool,
    /// SERVICE_AUTO_START=2 / DEMAND_START=3 / DISABLED=4
    pub start_type: u32,
    /// 実行中の依存サービス（このサービスを止めると一緒に止まる）。
    pub dependents: Vec<String>,
}

pub struct Facts<'a> {
    pub feature: &'a Feature,
    pub procs: Vec<&'a ProcessInfo>,
    pub services: Vec<ServiceFacts>,
    pub user_protected: &'a [String],
}

/// 実行時の安全分類と、その理由。
pub fn classify(f: &Facts) -> (SafetyClass, Vec<String>) {
    let mut class = f.feature.safety;
    let mut why = Vec::new();
    fn raise(class: &mut SafetyClass, why: &mut Vec<String>, c: SafetyClass, msg: String) {
        *class = class.strictest(c);
        why.push(msg);
    }
    if f.feature.safety == SafetyClass::Protected {
        why.push("組み込みの保護対象です".into());
    }
    for p in &f.procs {
        if p.exe_name.to_ascii_lowercase().starts_with("memopt-") {
            raise(&mut class, &mut why, SafetyClass::Protected, "本ツール自身は停止できません".into());
        }
        if p.is_ppl {
            raise(&mut class, &mut why, SafetyClass::Protected, format!("{} は保護プロセス(PPL)です", p.exe_name));
        }
        if memopt_grouping::is_protected(p, f.user_protected) {
            raise(&mut class, &mut why, SafetyClass::Protected, format!("{} は保護リストに含まれます", p.exe_name));
        }
        if p.session_id == 0 && f.services.is_empty() && p.pid != 0 {
            raise(&mut class, &mut why, SafetyClass::Caution, format!("{} はセッション 0 で動作しています", p.exe_name));
        }
    }
    for s in &f.services {
        if !s.accepts_stop {
            raise(&mut class, &mut why, SafetyClass::Protected, format!("サービス {} は停止要求を受け付けません", s.name));
        }
        if CRITICAL_SERVICES.contains(&s.name.to_ascii_lowercase().as_str()) {
            raise(&mut class, &mut why, SafetyClass::Protected, format!("サービス {} は OS の中核のため停止できません", s.name));
        }
        if !s.dependents.is_empty() {
            raise(&mut class, &mut why, SafetyClass::Caution, format!("サービス {} には依存サービスがあります: {}", s.name, s.dependents.join(", ")));
        }
        if CAUTION_SERVICES.contains(&s.name.to_ascii_lowercase().as_str()) {
            raise(&mut class, &mut why, SafetyClass::Caution, format!("サービス {} はネットワーク/音声/印刷などに影響します", s.name));
        }
    }
    (class, why)
}

fn opt(method: StopMethod, target: Option<&str>, available: bool, note: Option<String>, recommended: bool) -> MethodOption {
    MethodOption { method, target: target.map(str::to_string), persistent: method.persistent(), available, note, recommended }
}

/// 一覧表示用の軽い判定。SCM への問い合わせはせず、プロセスの情報とサービス名だけで安全分類を求める。
/// （サービスの「停止要求を受け付けるか」「依存の有無」は PlanStop のときに確認する）
pub fn quick_safety(feature: &Feature, procs: &[&ProcessInfo], user_protected: &[String]) -> SafetyClass {
    let services: Vec<ServiceFacts> = procs
        .iter()
        .flat_map(|p| p.services.iter())
        .map(|n| ServiceFacts { name: n.clone(), accepts_stop: true, running: true, start_type: SERVICE_AUTO, dependents: vec![] })
        .collect();
    classify(&Facts { feature, procs: procs.to_vec(), services, user_protected }).0
}

pub fn build_plan(f: &Facts) -> StopPlan {
    let (safety, mut impacts) = classify(f);
    let protected = safety == SafetyClass::Protected;
    let mut options = Vec::new();
    let dependents: Vec<String> = f.services.iter().flat_map(|s| s.dependents.clone()).collect();
    let kind = f.feature.kind;

    if !protected {
        match kind {
            FeatureKind::Service => {
                for s in &f.services {
                    let t = Some(s.name.as_str());
                    let stop_ok = s.running;
                    let stop_note = if s.dependents.is_empty() { None } else { Some(format!("依存サービスも停止します: {}", s.dependents.join(", "))) };
                    options.push(opt(StopMethod::ServiceStop, t, stop_ok, stop_note.or((!stop_ok).then(|| "実行中ではありません".into())), true));
                    options.push(opt(StopMethod::ServiceStartupManual, t, s.start_type == SERVICE_AUTO, (s.start_type != SERVICE_AUTO).then(|| "自動起動ではありません".into()), false));
                    options.push(opt(StopMethod::ServiceStartupDisabled, t, s.start_type != SERVICE_DISABLED, (s.start_type == SERVICE_DISABLED).then(|| "すでに無効です".into()), false));
                }
                options.push(opt(StopMethod::LowerPriority, None, true, None, false));
            }
            FeatureKind::DesktopApp | FeatureKind::Browser | FeatureKind::StoreApp => {
                options.push(opt(StopMethod::GracefulClose, None, true, Some("ウィンドウを閉じます。保存確認が出る場合があります".into()), true));
                options.push(opt(StopMethod::ForceKill, None, true, Some("未保存のデータは失われます".into()), false));
                options.push(opt(StopMethod::LowerPriority, None, true, Some("アプリを残したまま、メモリ圧迫時に先に手放されるようにします".into()), false));
                for s in &f.feature.targets.startup {
                    options.push(opt(StopMethod::DisableStartup, Some(&s.name), true, Some("登録は消さず、次回ログオンから起動しなくなります".into()), false));
                }
                for t in &f.feature.targets.tasks {
                    options.push(opt(StopMethod::DisableTask, Some(t), true, None, false));
                }
            }
            FeatureKind::Wsl => impacts.push("WSL は停止ボタンを用意していません。`wsl --shutdown` を実行するか、.wslconfig でメモリ上限を設定してください".into()),
            FeatureKind::Startup | FeatureKind::ScheduledTask | FeatureKind::System => {}
        }
    }

    impacts.insert(0, format!("{} 個のプロセス / 停止で空く見込み 最大 約 {} MB", f.procs.len(), f.feature.private_working_set >> 20));
    StopPlan {
        feature_id: f.feature.id.clone(),
        name: f.feature.name.clone(),
        safety,
        process_count: f.procs.len(),
        est_release_bytes: f.feature.private_working_set,
        dependents,
        options,
        impacts,
    }
}

/// パイプの接続元の身元（OS が保証する値）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallerInfo {
    pub is_admin: bool,
    pub user_sid: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Deny {
    /// 呼び出し元の権限が足りない（管理者でないと実行できない操作）。
    NeedsAdmin(String),
    Protected,
    NotOffered,
    Unavailable(String),
    NeedsConfirmation,
    NeedsTarget,
}

impl Deny {
    pub fn text(&self) -> String {
        match self {
            Self::NeedsAdmin(why) => format!("管理者権限が必要です: {why}"),
            Self::Protected => "保護対象のため停止できません".into(),
            Self::NotOffered => "この機能では使えない手段です".into(),
            Self::Unavailable(n) => format!("現在は実行できません: {n}"),
            Self::NeedsConfirmation => "永続的な変更には 2 段階の確認が必要です".into(),
            Self::NeedsTarget => "対象（サービス名など）を指定してください".into(),
        }
    }
}

/// 要求された手段を認可する。計画に載っている・使える・確認済み、のすべてを満たす場合だけ通す。
pub fn authorize<'a>(plan: &'a StopPlan, method: StopMethod, target: Option<&str>, confirmed: bool) -> Result<&'a MethodOption, Deny> {
    if plan.safety == SafetyClass::Protected {
        return Err(Deny::Protected);
    }
    let mut candidates = plan.options.iter().filter(|o| o.method == method);
    let chosen = match target {
        Some(t) => candidates.find(|o| o.target.as_deref() == Some(t)),
        None => {
            let all: Vec<_> = candidates.collect();
            match all.as_slice() {
                [] => None,
                [one] => Some(*one),
                _ => return Err(Deny::NeedsTarget), // 複数対象があるのに未指定
            }
        }
    };
    let o = chosen.ok_or(Deny::NotOffered)?;
    if !o.available {
        return Err(Deny::Unavailable(o.note.clone().unwrap_or_default()));
    }
    if o.persistent && !confirmed {
        return Err(Deny::NeedsConfirmation);
    }
    Ok(o)
}

/// 呼び出し元の権限で、その手段を実行してよいか。計画の認可（`authorize`）とは別に、必ず両方を通すこと。
///
/// 管理者（昇格済み）はすべて可。標準ユーザーは「自分のものだけ」:
/// - 正常終了 / 強制終了 / 優先度: 影響するプロセスがすべて自分のユーザーのもの（所有者が不明なら不可）
/// - スタートアップ無効化: 自分の HKU 配下の項目だけ（HKLM = マシン全体は不可）
/// - サービスの停止・種別変更、タスクの無効化: 不可（SYSTEM の権限で他人に影響するため）
pub fn authorize_caller(
    method: StopMethod,
    target: Option<&str>,
    feature: &Feature,
    owner_of: &dyn Fn(u32) -> Option<String>,
    affected_pids: &[u32],
    caller: &CallerInfo,
) -> Result<(), Deny> {
    if caller.is_admin {
        return Ok(());
    }
    let me = caller.user_sid.as_deref();
    match method {
        StopMethod::ServiceStop | StopMethod::ServiceStartupManual | StopMethod::ServiceStartupDisabled | StopMethod::DisableTask => {
            Err(Deny::NeedsAdmin("サービスやスケジュールタスクの変更は管理者のみ可能です".into()))
        }
        StopMethod::DisableStartup => {
            let mine = target
                .and_then(|t| feature.targets.startup.iter().find(|s| s.name == t))
                .is_some_and(|s| me.is_some() && s.user_sid.as_deref() == me);
            if mine { Ok(()) } else { Err(Deny::NeedsAdmin("自分以外（またはマシン全体）のスタートアップ項目です".into())) }
        }
        StopMethod::GracefulClose | StopMethod::ForceKill | StopMethod::LowerPriority => {
            let all_mine = me.is_some() && !affected_pids.is_empty() && affected_pids.iter().all(|p| owner_of(*p).as_deref() == me);
            if all_mine { Ok(()) } else { Err(Deny::NeedsAdmin("他のユーザーまたはシステムのプロセスを含みます".into())) }
        }
    }
}

/// 復元してよいか。管理者はすべて可。標準ユーザーは「自分が行った、自分の範囲の操作」だけ。
pub fn can_undo(entry: &memopt_core::stop::JournalEntry, caller: &CallerInfo) -> bool {
    use memopt_core::stop::Change;
    if caller.is_admin {
        return true;
    }
    let me = caller.user_sid.as_deref();
    if me.is_none() || entry.actor_sid.as_deref() != me {
        return false;
    }
    match &entry.change {
        Change::StartupApproved { user_sid, .. } => user_sid.as_deref() == me,
        Change::MemoryPriority { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memopt_core::stop::{StartupRef, StopTargets};

    fn proc(pid: u32, exe: &str, path: &str) -> ProcessInfo {
        ProcessInfo { pid, exe_name: exe.into(), exe_path: Some(path.into()), session_id: 1, ..Default::default() }
    }

    fn feature(kind: FeatureKind, safety: SafetyClass) -> Feature {
        Feature {
            id: "f".into(), name: "F".into(), kind, pids: vec![1], private_bytes: 100 << 20, working_set: 100 << 20,
            private_working_set: 80 << 20, safety, startup: false, targets: StopTargets::default(),
        }
    }

    fn svc(name: &str) -> ServiceFacts {
        ServiceFacts { name: name.into(), accepts_stop: true, running: true, start_type: SERVICE_AUTO, dependents: vec![] }
    }

    fn plan_for(f: &Feature, procs: &[ProcessInfo], services: Vec<ServiceFacts>, user: &[String]) -> StopPlan {
        build_plan(&Facts { feature: f, procs: procs.iter().collect(), services, user_protected: user })
    }

    fn methods(p: &StopPlan) -> Vec<StopMethod> {
        p.options.iter().map(|o| o.method).collect()
    }

    #[test]
    fn ordinary_app_offers_graceful_first_then_force_priority() {
        let f = feature(FeatureKind::DesktopApp, SafetyClass::Stoppable);
        let p = plan_for(&f, &[proc(1, "app.exe", r"C:\a\app.exe")], vec![], &[]);
        assert_eq!(p.safety, SafetyClass::Stoppable);
        assert_eq!(methods(&p), vec![StopMethod::GracefulClose, StopMethod::ForceKill, StopMethod::LowerPriority]);
        assert!(p.options[0].recommended && !p.options[1].recommended);
        assert_eq!(p.est_release_bytes, 80 << 20);
    }

    #[test]
    fn startup_and_task_targets_become_persistent_options() {
        let mut f = feature(FeatureKind::DesktopApp, SafetyClass::Stoppable);
        f.targets.startup.push(StartupRef { name: "App".into(), user_sid: None });
        f.targets.tasks.push(r"\Vendor\Updater".into());
        let p = plan_for(&f, &[proc(1, "a.exe", r"C:\a.exe")], vec![], &[]);
        let persistent: Vec<_> = p.options.iter().filter(|o| o.persistent).map(|o| (o.method, o.target.clone().unwrap())).collect();
        assert_eq!(persistent, vec![(StopMethod::DisableStartup, "App".into()), (StopMethod::DisableTask, r"\Vendor\Updater".into())]);
    }

    #[test]
    fn builtin_protected_has_no_options_and_reason_is_listed() {
        let f = feature(FeatureKind::DesktopApp, SafetyClass::Protected);
        let p = plan_for(&f, &[proc(1, "lsass.exe", r"C:\Windows\System32\lsass.exe")], vec![], &[]);
        assert!(p.options.is_empty());
        assert!(p.impacts.iter().any(|i| i.contains("保護")));
    }

    #[test]
    fn runtime_ppl_and_self_and_user_protection_escalate_to_protected() {
        let f = feature(FeatureKind::DesktopApp, SafetyClass::Stoppable);
        let mut ppl = proc(1, "av.exe", r"C:\av\av.exe");
        ppl.is_ppl = true;
        assert_eq!(plan_for(&f, &[ppl], vec![], &[]).safety, SafetyClass::Protected);
        assert_eq!(plan_for(&f, &[proc(2, "memopt-service.exe", r"C:\m\memopt-service.exe")], vec![], &[]).safety, SafetyClass::Protected);
        let user = vec![r"C:\daw\daw.exe".to_string()];
        assert_eq!(plan_for(&f, &[proc(3, "daw.exe", r"C:\DAW\daw.exe")], vec![], &user).safety, SafetyClass::Protected);
    }

    #[test]
    fn service_without_stop_support_is_protected_and_dependents_make_it_caution() {
        let f = feature(FeatureKind::Service, SafetyClass::Caution);
        let mut nostop = svc("X");
        nostop.accepts_stop = false;
        assert_eq!(plan_for(&f, &[proc(1, "svchost.exe", r"C:\Windows\System32\svchost.exe")], vec![nostop], &[]).safety, SafetyClass::Protected);
        let mut dep = svc("Y");
        dep.dependents = vec!["Z".into()];
        let p = plan_for(&f, &[proc(1, "svchost.exe", r"C:\Windows\System32\svchost.exe")], vec![dep], &[]);
        assert_eq!(p.safety, SafetyClass::Caution);
        assert_eq!(p.dependents, vec!["Z".to_string()]);
        assert!(p.options.iter().find(|o| o.method == StopMethod::ServiceStop).unwrap().note.as_ref().unwrap().contains("Z"));
    }

    #[test]
    fn service_options_reflect_current_state() {
        let f = feature(FeatureKind::Service, SafetyClass::Caution);
        let mut s = svc("Search");
        s.start_type = SERVICE_DISABLED;
        s.running = false;
        let p = plan_for(&f, &[], vec![s], &[]);
        let get = |m| p.options.iter().find(|o| o.method == m).unwrap();
        assert!(!get(StopMethod::ServiceStop).available);
        assert!(!get(StopMethod::ServiceStartupManual).available);
        assert!(!get(StopMethod::ServiceStartupDisabled).available);
    }

    #[test]
    fn os_critical_services_are_protected_even_if_they_accept_stop() {
        let f = feature(FeatureKind::Service, SafetyClass::Caution);
        for name in ["EventLog", "Schedule", "Winmgmt", "Dnscache"] {
            let mut s = svc(name);
            s.accepts_stop = true;
            let p = plan_for(&f, &[], vec![s], &[]);
            if name == "Dnscache" {
                assert_eq!(p.safety, SafetyClass::Caution, "{name}");
            } else {
                assert_eq!(p.safety, SafetyClass::Protected, "{name}");
                assert!(p.options.is_empty());
            }
        }
    }

    #[test]
    fn network_and_audio_services_are_caution() {
        let f = feature(FeatureKind::Service, SafetyClass::Caution);
        let p = plan_for(&f, &[], vec![svc("Audiosrv")], &[]);
        assert!(p.impacts.iter().any(|i| i.contains("音声")));
    }

    #[test]
    fn quick_safety_uses_names_and_process_facts_without_scm() {
        let f = feature(FeatureKind::Service, SafetyClass::Caution);
        let mut p = proc(1, "svchost.exe", r"C:\Windows\System32\svchost.exe");
        p.services = vec!["Schedule".into()];
        assert_eq!(quick_safety(&f, &[&p], &[]), SafetyClass::Protected);
        p.services = vec!["WSearch".into()];
        assert_eq!(quick_safety(&f, &[&p], &[]), SafetyClass::Caution);
        let app = feature(FeatureKind::DesktopApp, SafetyClass::Stoppable);
        let mut ppl = proc(2, "x.exe", r"C:\x.exe");
        ppl.is_ppl = true;
        assert_eq!(quick_safety(&app, &[&ppl], &[]), SafetyClass::Protected);
    }

    #[test]
    fn wsl_and_system_have_no_buttons() {
        let w = feature(FeatureKind::Wsl, SafetyClass::Caution);
        let p = plan_for(&w, &[proc(1, "vmmemWSL", "")], vec![], &[]);
        assert!(p.options.is_empty());
        assert!(p.impacts.iter().any(|i| i.contains("wsl --shutdown")));
        let s = feature(FeatureKind::System, SafetyClass::Protected);
        assert!(plan_for(&s, &[], vec![], &[]).options.is_empty());
    }

    #[test]
    fn authorize_rejects_protected_unoffered_unavailable_and_unconfirmed_persistent() {
        let mut f = feature(FeatureKind::DesktopApp, SafetyClass::Stoppable);
        f.targets.startup.push(StartupRef { name: "App".into(), user_sid: None });
        let p = plan_for(&f, &[proc(1, "a.exe", r"C:\a.exe")], vec![], &[]);
        assert!(authorize(&p, StopMethod::GracefulClose, None, false).is_ok());
        assert_eq!(authorize(&p, StopMethod::ServiceStop, None, true), Err(Deny::NotOffered));
        assert_eq!(authorize(&p, StopMethod::DisableStartup, Some("App"), false), Err(Deny::NeedsConfirmation));
        assert!(authorize(&p, StopMethod::DisableStartup, Some("App"), true).is_ok());
        assert_eq!(authorize(&p, StopMethod::DisableStartup, Some("Other"), true), Err(Deny::NotOffered));

        let prot = plan_for(&feature(FeatureKind::DesktopApp, SafetyClass::Protected), &[], vec![], &[]);
        assert_eq!(authorize(&prot, StopMethod::ForceKill, None, true), Err(Deny::Protected));
    }

    #[test]
    fn authorize_requires_a_target_when_several_exist_and_rejects_unavailable() {
        let f = feature(FeatureKind::Service, SafetyClass::Caution);
        let mut off = svc("B");
        off.running = false;
        let p = plan_for(&f, &[], vec![svc("A"), off], &[]);
        assert_eq!(authorize(&p, StopMethod::ServiceStop, None, false), Err(Deny::NeedsTarget));
        assert!(authorize(&p, StopMethod::ServiceStop, Some("A"), false).is_ok());
        assert!(matches!(authorize(&p, StopMethod::ServiceStop, Some("B"), false), Err(Deny::Unavailable(_))));
        // 永続変更は確認フラグが無ければ通らない
        assert_eq!(authorize(&p, StopMethod::ServiceStartupDisabled, Some("A"), false), Err(Deny::NeedsConfirmation));
    }

    // ---- 呼び出し元の権限（標準ユーザーは自分のものだけ）----
    const ME: &str = "S-1-5-21-1-2-3-1001";
    const OTHER: &str = "S-1-5-21-1-2-3-1002";

    fn std_user() -> CallerInfo {
        CallerInfo { is_admin: false, user_sid: Some(ME.into()) }
    }

    fn owners<'a>(map: &'a [(u32, &'a str)]) -> impl Fn(u32) -> Option<String> + 'a {
        move |pid| map.iter().find(|(p, _)| *p == pid).map(|(_, s)| s.to_string())
    }

    fn app_feature() -> Feature {
        let mut f = feature(FeatureKind::DesktopApp, SafetyClass::Stoppable);
        f.targets.startup.push(StartupRef { name: "Mine".into(), user_sid: Some(ME.into()) });
        f.targets.startup.push(StartupRef { name: "Theirs".into(), user_sid: Some(OTHER.into()) });
        f.targets.startup.push(StartupRef { name: "Machine".into(), user_sid: None });
        f
    }

    #[test]
    fn admin_may_do_everything() {
        let admin = CallerInfo { is_admin: true, user_sid: None };
        let f = app_feature();
        for m in [StopMethod::ServiceStop, StopMethod::DisableTask, StopMethod::ForceKill, StopMethod::DisableStartup] {
            assert!(authorize_caller(m, Some("Machine"), &f, &owners(&[]), &[1], &admin).is_ok(), "{m:?}");
        }
    }

    #[test]
    fn standard_user_cannot_touch_services_or_tasks_at_all() {
        let f = app_feature();
        for m in [StopMethod::ServiceStop, StopMethod::ServiceStartupManual, StopMethod::ServiceStartupDisabled, StopMethod::DisableTask] {
            let r = authorize_caller(m, Some("x"), &f, &owners(&[(1, ME)]), &[1], &std_user());
            assert!(matches!(r, Err(Deny::NeedsAdmin(_))), "{m:?}: {r:?}");
        }
    }

    #[test]
    fn standard_user_may_act_only_on_their_own_processes() {
        let f = app_feature();
        let own = owners(&[(1, ME), (2, ME)]);
        for m in [StopMethod::GracefulClose, StopMethod::ForceKill, StopMethod::LowerPriority] {
            assert!(authorize_caller(m, None, &f, &own, &[1, 2], &std_user()).is_ok(), "{m:?}");
        }
        // 1 つでも他人/システムのプロセスが混ざれば不可
        let mixed = owners(&[(1, ME), (2, OTHER)]);
        assert!(matches!(authorize_caller(StopMethod::ForceKill, None, &f, &mixed, &[1, 2], &std_user()), Err(Deny::NeedsAdmin(_))));
        let sys = owners(&[(1, "S-1-5-18")]);
        assert!(authorize_caller(StopMethod::GracefulClose, None, &f, &sys, &[1], &std_user()).is_err());
        // 所有者が分からない / 対象が空 / 身元不明は不可（安全側）
        assert!(authorize_caller(StopMethod::ForceKill, None, &f, &owners(&[]), &[9], &std_user()).is_err());
        assert!(authorize_caller(StopMethod::ForceKill, None, &f, &own, &[], &std_user()).is_err());
        assert!(authorize_caller(StopMethod::ForceKill, None, &f, &own, &[1], &CallerInfo::default()).is_err());
    }

    #[test]
    fn standard_user_may_disable_only_their_own_startup_entries() {
        let f = app_feature();
        let none = owners(&[]);
        assert!(authorize_caller(StopMethod::DisableStartup, Some("Mine"), &f, &none, &[], &std_user()).is_ok());
        for t in ["Theirs", "Machine", "Unknown"] {
            assert!(matches!(authorize_caller(StopMethod::DisableStartup, Some(t), &f, &none, &[], &std_user()), Err(Deny::NeedsAdmin(_))), "{t}");
        }
        assert!(authorize_caller(StopMethod::DisableStartup, None, &f, &none, &[], &std_user()).is_err());
        assert!(authorize_caller(StopMethod::DisableStartup, Some("Mine"), &f, &none, &[], &CallerInfo::default()).is_err());
    }

    #[test]
    fn undo_is_limited_to_the_callers_own_user_scope_changes() {
        use memopt_core::stop::{Change, JournalEntry, Outcome};
        let mk = |actor: Option<&str>, change: Change| JournalEntry {
            id: 1, ts_unix: 0, feature_id: "f".into(), feature_name: "F".into(), method: StopMethod::DisableStartup, target: None,
            change, outcome: Outcome::Ok, avail_before: None, avail_after: None, undone_ts: None, actor_sid: actor.map(str::to_string),
        };
        let mine = Change::StartupApproved { user_sid: Some(ME.into()), value_name: "x".into(), before_raw: None };
        let theirs = Change::StartupApproved { user_sid: Some(OTHER.into()), value_name: "x".into(), before_raw: None };
        let machine = Change::StartupApproved { user_sid: None, value_name: "x".into(), before_raw: None };
        assert!(can_undo(&mk(Some(ME), mine.clone()), &std_user()));
        assert!(!can_undo(&mk(Some(OTHER), mine), &std_user())); // 他人が行った記録
        assert!(!can_undo(&mk(Some(ME), theirs), &std_user())); // 他人の領域の変更
        assert!(!can_undo(&mk(Some(ME), machine), &std_user())); // HKLM
        assert!(!can_undo(&mk(Some(ME), Change::ServiceStopped { name: "s".into(), dependents: vec![] }), &std_user()));
        assert!(!can_undo(&mk(None, Change::MemoryPriority { items: vec![] }), &std_user())); // 記録者不明
        assert!(can_undo(&mk(Some(ME), Change::MemoryPriority { items: vec![] }), &std_user()));
        assert!(can_undo(&mk(Some(OTHER), Change::ServiceStopped { name: "s".into(), dependents: vec![] }), &CallerInfo { is_admin: true, user_sid: None }));
    }
}
