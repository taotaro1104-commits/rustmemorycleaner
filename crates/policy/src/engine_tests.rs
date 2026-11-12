//! engine.rs のテスト（トリガー判定・抑止・クールダウン・自動調整の境界値）。
    use super::*;

    const GB: u64 = 1 << 30;

    fn mem(avail_pct: f64, commit_pct: f64) -> MemorySnapshot {
        MemorySnapshot {
            total_phys: 16 * GB,
            avail_phys: (16.0 * GB as f64 * avail_pct / 100.0) as u64,
            commit_total: (100.0 * commit_pct) as u64,
            commit_limit: 10_000,
            system_cache: 0,
        }
    }

    fn eng() -> Engine {
        Engine::new(&Config::default())
    }

    fn run_levels(d: &Decision) -> Vec<u8> {
        match d {
            Decision::Run { levels, .. } => levels.iter().map(|l| *l as u8).collect(),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn pressure_needs_sustain_before_firing() {
        let mut e = eng();
        assert_eq!(e.evaluate(&Input::quiet(0, mem(10.0, 50.0))), Decision::None);
        assert_eq!(e.evaluate(&Input::quiet(29, mem(10.0, 50.0))), Decision::None);
        let d = e.evaluate(&Input::quiet(30, mem(10.0, 50.0)));
        assert_eq!(d, Decision::Run { trigger: Trigger::Pressure, levels: vec![Level::L1] });
    }

    #[test]
    fn short_spike_resets_sustain_timer() {
        let mut e = eng();
        e.evaluate(&Input::quiet(0, mem(10.0, 50.0)));
        e.evaluate(&Input::quiet(20, mem(40.0, 50.0))); // 回復
        e.evaluate(&Input::quiet(25, mem(10.0, 50.0)));
        assert_eq!(e.evaluate(&Input::quiet(50, mem(10.0, 50.0))), Decision::None);
        assert!(matches!(e.evaluate(&Input::quiet(55, mem(10.0, 50.0))), Decision::Run { .. }));
    }

    #[test]
    fn commit_over_threshold_also_triggers() {
        let mut e = eng();
        e.evaluate(&Input::quiet(0, mem(50.0, 90.0)));
        assert!(matches!(e.evaluate(&Input::quiet(30, mem(50.0, 90.0))), Decision::Run { .. }));
    }

    #[test]
    fn escalates_to_l2_after_60s_if_still_pressured_and_stops_at_max() {
        let mut e = eng();
        e.evaluate(&Input::quiet(0, mem(10.0, 50.0)));
        assert_eq!(run_levels(&e.evaluate(&Input::quiet(30, mem(10.0, 50.0)))), vec![1]);
        assert_eq!(e.evaluate(&Input::quiet(60, mem(10.0, 50.0))), Decision::None); // 検証待ち
        assert_eq!(run_levels(&e.evaluate(&Input::quiet(90, mem(10.0, 50.0)))), vec![2]);
        // L2 の 60 秒後もまだ逼迫していても L3 へは自動で上がらない → クールダウンへ
        let d = e.evaluate(&Input::quiet(150, mem(10.0, 50.0)));
        assert_eq!(d, Decision::None);
        assert_eq!(e.evaluate(&Input::quiet(200, mem(10.0, 50.0))), Decision::None);
    }

    #[test]
    fn no_escalation_when_pressure_resolved_and_cooldown_blocks_next() {
        let mut e = eng();
        e.evaluate(&Input::quiet(0, mem(10.0, 50.0)));
        e.evaluate(&Input::quiet(30, mem(10.0, 50.0)));
        // 60 秒後に回復 → 段階上げなし、クールダウン開始（90 + 300）
        assert_eq!(e.evaluate(&Input::quiet(90, mem(20.0, 50.0))), Decision::None);
        // 低メモリ通知はクールダウン中は抑止される
        let mut i = Input::quiet(100, mem(20.0, 50.0));
        i.low_memory_signal = true;
        assert_eq!(e.evaluate(&i), Decision::Skip { trigger: Trigger::LowMemory, reason: SkipReason::Cooldown });
        let mut i = Input::quiet(400, mem(20.0, 50.0));
        i.low_memory_signal = true;
        assert_eq!(run_levels(&e.evaluate(&i)), vec![1, 2]);
    }

    #[test]
    fn hysteresis_prevents_refire_until_released_above_25() {
        let mut e = eng();
        e.evaluate(&Input::quiet(0, mem(10.0, 50.0)));
        e.evaluate(&Input::quiet(30, mem(10.0, 50.0)));
        e.evaluate(&Input::quiet(90, mem(18.0, 50.0))); // 回復、クールダウンへ
        // 25% 未満のまま再び 14% に落ちても武装前なので発動しない
        e.evaluate(&Input::quiet(500, mem(14.0, 50.0)));
        assert_eq!(e.evaluate(&Input::quiet(600, mem(14.0, 50.0))), Decision::None);
        // 25% 以上に回復 → 再武装 → 再び逼迫で発動
        e.evaluate(&Input::quiet(700, mem(30.0, 50.0)));
        e.evaluate(&Input::quiet(800, mem(10.0, 50.0)));
        assert!(matches!(e.evaluate(&Input::quiet(830, mem(10.0, 50.0))), Decision::Run { .. }));
    }

    #[test]
    fn suppression_reasons_are_reported_not_run() {
        let cases: Vec<(Box<dyn Fn(&mut Input)>, SkipReason)> = vec![
            (Box::new(|i| i.fullscreen = true), SkipReason::Fullscreen),
            (Box::new(|i| i.disk_busy_pct = Some(80.0)), SkipReason::DiskBusy),
            (Box::new(|i| i.battery_saver = true), SkipReason::BatterySaver),
            (Box::new(|i| i.uptime_secs = 120), SkipReason::Startup),
            (Box::new(|i| i.paused = true), SkipReason::Paused),
        ];
        for (mutate, want) in cases {
            let mut e = eng();
            e.evaluate(&Input::quiet(0, mem(10.0, 50.0)));
            let mut i = Input::quiet(30, mem(10.0, 50.0));
            mutate(&mut i);
            assert_eq!(e.evaluate(&i), Decision::Skip { trigger: Trigger::Pressure, reason: want });
        }
    }

    #[test]
    fn disk_busy_exactly_at_threshold_is_not_suppressed() {
        let mut e = eng();
        e.evaluate(&Input::quiet(0, mem(10.0, 50.0)));
        let mut i = Input::quiet(30, mem(10.0, 50.0));
        i.disk_busy_pct = Some(70.0);
        assert!(matches!(e.evaluate(&i), Decision::Run { .. }));
    }

    #[test]
    fn emergency_stop_blocks_auto_but_not_manual() {
        let mut e = eng();
        e.set_emergency_stop(true);
        e.evaluate(&Input::quiet(0, mem(10.0, 50.0)));
        let d = e.evaluate(&Input::quiet(30, mem(10.0, 50.0)));
        assert_eq!(d, Decision::Skip { trigger: Trigger::Pressure, reason: SkipReason::EmergencyStop });
        let mut m = Input::quiet(31, mem(10.0, 50.0));
        m.manual = Some(Level::L3);
        assert_eq!(run_levels(&e.evaluate(&m)), vec![3]);
    }

    #[test]
    fn hourly_cap_stops_auto_runs_and_recovers_after_an_hour() {
        let mut cfg = Config::default();
        cfg.auto_clean.max_runs_per_hour = 2;
        cfg.auto_clean.cooldown_sec = 30;
        let mut e = Engine::new(&cfg);
        let mut t = 0;
        let mut fired = 0;
        for _ in 0..4 {
            // 毎回、再武装してから逼迫させる
            e.evaluate(&Input::quiet(t, mem(40.0, 50.0)));
            t += 10;
            e.evaluate(&Input::quiet(t, mem(10.0, 50.0)));
            t += 30;
            match e.evaluate(&Input::quiet(t, mem(10.0, 50.0))) {
                Decision::Run { .. } => fired += 1,
                Decision::Skip { reason, .. } => assert_eq!(reason, SkipReason::HourlyCap),
                Decision::None => {}
            }
            t += 100; // 検証待ち・クールダウンを抜ける
            e.evaluate(&Input::quiet(t, mem(40.0, 50.0)));
            t += 100;
        }
        assert_eq!(fired, 2);
        assert!(e.runs_last_hour(t) <= 2);
        assert_eq!(e.runs_last_hour(t + 4000), 0);
    }

    #[test]
    fn manual_bypasses_all_suppression_and_cooldown() {
        let mut e = eng();
        let mut i = Input::quiet(0, mem(50.0, 50.0));
        i.fullscreen = true;
        i.uptime_secs = 10;
        i.manual = Some(Level::L2);
        assert_eq!(run_levels(&e.evaluate(&i)), vec![2]);
    }

    #[test]
    fn idle_fires_once_per_idle_period() {
        let mut e = eng();
        let mut i = Input::quiet(0, mem(50.0, 50.0));
        i.user_idle_secs = Some(599);
        assert_eq!(e.evaluate(&i), Decision::None);
        i.now = 1;
        i.user_idle_secs = Some(600);
        assert_eq!(e.evaluate(&i), Decision::Run { trigger: Trigger::Idle, levels: vec![Level::L1] });
        i.now = 1000;
        i.user_idle_secs = Some(1600);
        assert_eq!(e.evaluate(&i), Decision::None); // 同じ無操作期間では 2 回目なし
        i.now = 1100;
        i.user_idle_secs = Some(5); // 復帰
        e.evaluate(&i);
        i.now = 2000;
        i.user_idle_secs = Some(700);
        assert!(matches!(e.evaluate(&i), Decision::Run { trigger: Trigger::Idle, .. }));
    }

    #[test]
    fn schedule_runs_configured_level_once_per_key_even_during_cooldown() {
        let mut e = eng();
        let mut i = Input::quiet(0, mem(50.0, 50.0));
        i.schedule_due = Some((7, Level::L3));
        assert_eq!(e.evaluate(&i), Decision::Run { trigger: Trigger::Schedule, levels: vec![Level::L3] });
        i.now = 15;
        assert_eq!(e.evaluate(&i), Decision::None); // 同じ分キーは再実行しない
        i.now = 100_000;
        i.schedule_due = Some((8, Level::L2));
        assert_eq!(e.evaluate(&i), Decision::Run { trigger: Trigger::Schedule, levels: vec![Level::L2] });
    }

    #[test]
    fn suppressed_schedule_is_retried_within_the_same_minute() {
        let mut e = eng();
        let mut i = Input::quiet(0, mem(50.0, 50.0));
        i.schedule_due = Some((7, Level::L2));
        i.fullscreen = true;
        assert_eq!(e.evaluate(&i), Decision::Skip { trigger: Trigger::Schedule, reason: SkipReason::Fullscreen });
        i.now = 15;
        i.fullscreen = false;
        assert!(matches!(e.evaluate(&i), Decision::Run { trigger: Trigger::Schedule, .. }));
    }

    #[test]
    fn disabled_auto_clean_does_nothing_automatic() {
        let mut cfg = Config::default();
        cfg.auto_clean.enabled = false;
        let mut e = Engine::new(&cfg);
        e.evaluate(&Input::quiet(0, mem(5.0, 95.0)));
        assert_eq!(e.evaluate(&Input::quiet(60, mem(5.0, 95.0))), Decision::None);
    }

    #[test]
    fn max_auto_level_one_blocks_escalation_and_trims_low_memory_levels() {
        let mut cfg = Config::default();
        cfg.auto_clean.max_auto_level = 1;
        let mut e = Engine::new(&cfg);
        let mut i = Input::quiet(0, mem(50.0, 50.0));
        i.low_memory_signal = true;
        assert_eq!(run_levels(&e.evaluate(&i)), vec![1]);
    }

    fn bad_eval(trigger: Trigger, level: Level) -> Evaluation {
        use memopt_core::LoadSample;
        Evaluation {
            trigger, top_level: level, freed_bytes: 1 << 30, total_phys: 16 * GB,
            load_before: Some(LoadSample { pages_input_per_sec: 100.0, disk_read_bytes_per_sec: 0.0 }),
            load_after: Some(LoadSample { pages_input_per_sec: 500.0, disk_read_bytes_per_sec: 0.0 }),
        }
    }

    #[test]
    fn counterproductive_result_extends_the_next_cooldown() {
        let mut e = eng(); // 基本 300 秒
        e.evaluate(&Input::quiet(0, mem(10.0, 50.0)));
        assert!(matches!(e.evaluate(&Input::quiet(30, mem(10.0, 50.0))), Decision::Run { .. }));
        assert_eq!(e.record_result(90, &bad_eval(Trigger::Pressure, Level::L1)).len(), 1);
        // 60 秒後の評価で段階上げなし（回復）→ クールダウン開始。逆効果なので 300 ではなく 600 秒。
        assert_eq!(e.evaluate(&Input::quiet(90, mem(20.0, 50.0))), Decision::None);
        let mut i = Input::quiet(100, mem(20.0, 50.0));
        i.low_memory_signal = true;
        assert!(matches!(e.evaluate(&i), Decision::Skip { reason: SkipReason::Cooldown, .. }));
        i.now = 90 + 300 + 1; // 通常ならもう抜けている時刻
        assert!(matches!(e.evaluate(&i), Decision::Skip { reason: SkipReason::Cooldown, .. }), "延長されているはず");
        i.now = 90 + 600;
        assert!(matches!(e.evaluate(&i), Decision::Run { .. }));
    }

    #[test]
    fn counterproductive_l2_limits_that_trigger_to_l1() {
        let mut e = eng();
        e.record_result(0, &bad_eval(Trigger::LowMemory, Level::L2));
        let mut i = Input::quiet(1, mem(50.0, 50.0));
        i.low_memory_signal = true;
        assert_eq!(run_levels(&e.evaluate(&i)), vec![1]); // 通常は [1, 2]
        // 圧迫トリガーには影響しない（段階上げで L2 まで行ける）
        let mut e2 = eng();
        e2.record_result(0, &bad_eval(Trigger::LowMemory, Level::L2));
        e2.evaluate(&Input::quiet(0, mem(10.0, 50.0)));
        e2.evaluate(&Input::quiet(30, mem(10.0, 50.0)));
        assert_eq!(run_levels(&e2.evaluate(&Input::quiet(90, mem(10.0, 50.0)))), vec![2]);
    }

    #[test]
    fn capped_pressure_trigger_does_not_escalate_past_l1() {
        let mut e = eng();
        e.record_result(0, &bad_eval(Trigger::Pressure, Level::L2));
        e.evaluate(&Input::quiet(0, mem(10.0, 50.0)));
        e.evaluate(&Input::quiet(30, mem(10.0, 50.0)));
        assert_eq!(e.evaluate(&Input::quiet(90, mem(10.0, 50.0))), Decision::None); // L2 へは上がらない
    }

    #[test]
    fn ineffective_result_skips_exactly_one_occurrence_and_does_not_refire_immediately() {
        let mut e = eng();
        let ineffective = Evaluation { freed_bytes: 1 << 20, load_before: None, load_after: None, ..bad_eval(Trigger::Idle, Level::L1) };
        assert_eq!(e.record_result(0, &ineffective).len(), 1);
        let mut i = Input::quiet(1, mem(50.0, 50.0));
        i.user_idle_secs = Some(700);
        assert!(matches!(e.evaluate(&i), Decision::Skip { reason: SkipReason::AdaptiveSkip, .. }));
        i.now = 16; // 15 秒後
        assert_eq!(e.evaluate(&i), Decision::None); // 同じ無操作期間では再発動しない
        // 利用者が戻って次の無操作期間に入り、クールダウンも明ければ通常どおり実行される
        i.now = 400;
        i.user_idle_secs = Some(5);
        e.evaluate(&i);
        i.now = 1500;
        i.user_idle_secs = Some(700);
        assert!(matches!(e.evaluate(&i), Decision::Run { trigger: Trigger::Idle, .. }));
    }

    #[test]
    fn manual_reset_restores_normal_behaviour() {
        let mut e = eng();
        e.record_result(0, &bad_eval(Trigger::LowMemory, Level::L2));
        assert!(!e.adaptive_notes(1).is_empty());
        e.reset_adaptive();
        assert!(e.adaptive_notes(1).is_empty());
        let mut i = Input::quiet(2, mem(50.0, 50.0));
        i.low_memory_signal = true;
        assert_eq!(run_levels(&e.evaluate(&i)), vec![1, 2]);
    }

    #[test]
    fn adaptive_disabled_in_config_changes_nothing() {
        let mut cfg = Config::default();
        cfg.adaptive.enabled = false;
        let mut e = Engine::new(&cfg);
        assert!(e.record_result(0, &bad_eval(Trigger::LowMemory, Level::L2)).is_empty());
        let mut i = Input::quiet(1, mem(50.0, 50.0));
        i.low_memory_signal = true;
        assert_eq!(run_levels(&e.evaluate(&i)), vec![1, 2]);
    }
