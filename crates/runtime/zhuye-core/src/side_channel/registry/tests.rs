//! Registry 测试(移植 ReflectDesktop side_channel 的 9 例)。

use super::*;

#[test]
fn ids_are_stable_and_sequential() {
    let reg = SideChannelRegistry::new();
    let (id1, _c1) = reg.start("agent".into(), "p1".into());
    let (id2, _c2) = reg.start("agent".into(), "p2".into());
    assert!(id1.starts_with("side-"));
    assert_ne!(id1, id2);
}

#[test]
fn cancels_are_independent() {
    let reg = SideChannelRegistry::new();
    let (id1, c1) = reg.start("a".into(), "p".into());
    let (_id2, _c2) = reg.start("a".into(), "p".into());
    c1.cancel();
    // 直接触发 token 不算 registry 取消(状态由 cancel() 推进);
    // 经 registry.cancel 取消 id1 不影响 id2。
    assert!(reg.cancel(&id1));
    assert_eq!(reg.get(&id1).unwrap().status, "cancelled");
    assert_eq!(reg.running_count(), 1);
}

#[test]
fn terminal_state_and_duration_do_not_change_after_cancel() {
    let reg = SideChannelRegistry::new();
    let (id, _c) = reg.start("a".into(), "p".into());
    assert!(reg.cancel(&id));
    let first = reg.get(&id).unwrap();
    // 迟到 finish 是 noop。
    assert!(!reg.finish(&id, SideChannelStatus::Done, None));
    let after = reg.get(&id).unwrap();
    assert_eq!(after.status, "cancelled");
    assert_eq!(after.duration_ms, first.duration_ms);
}

#[test]
fn finish_done_emits_summary() {
    let reg = SideChannelRegistry::new();
    let (id, _c) = reg.start("a".into(), "p".into());
    let mut rx = reg.subscribe_events();
    assert!(reg.finish(&id, SideChannelStatus::Done, Some("done summary".into())));
    let ev = rx.try_recv().expect("Done event");
    assert!(matches!(ev, SideChannelEvent::Done { summary: Some(s), .. } if s == "done summary"));
    assert_eq!(reg.get(&id).unwrap().status, "done");
}

#[test]
fn finish_error_carries_message() {
    let reg = SideChannelRegistry::new();
    let (id, _c) = reg.start("a".into(), "p".into());
    let mut rx = reg.subscribe_events();
    assert!(reg.finish(&id, SideChannelStatus::Error, Some("boom".into())));
    let ev = rx.try_recv().expect("Error event");
    assert!(matches!(ev, SideChannelEvent::Error { message, .. } if message == "boom"));
}

#[test]
fn list_snapshots_sorted_by_start() {
    let reg = SideChannelRegistry::new();
    let (id1, _c1) = reg.start("a".into(), "p1".into());
    let (id2, _c2) = reg.start("a".into(), "p2".into());
    let list = reg.list();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, id1);
    assert_eq!(list[1].id, id2);
    assert!(list.iter().all(|s| s.duration_ms.is_none()), "运行中无时长");
}

#[test]
fn id_collision_does_not_replace_existing_handle() {
    // 计数器回绕后候选 id 撞上已存在的 handle 时必须跳过。
    let reg = SideChannelRegistry::new();
    let (id1, _c1) = reg.start("a".into(), "first".into());
    // 把计数器拨回 id1 的序号,下一次 start 会先撞上 id1 再跳过。
    let seq = id1.trim_start_matches("side-");
    let n = u64::from_str_radix(seq, 16).unwrap() as usize;
    reg.next_seq.store(n, Ordering::Relaxed);
    let (id2, _c2) = reg.start("a".into(), "second".into());
    assert_ne!(id1, id2);
    assert_eq!(reg.list().len(), 2);
    assert_eq!(reg.get(&id1).unwrap().prompt, "first");
}

#[test]
fn cancel_unknown_or_terminal_returns_false() {
    let reg = SideChannelRegistry::new();
    assert!(!reg.cancel("side-00000001"));
    let (id, _c) = reg.start("a".into(), "p".into());
    reg.finish(&id, SideChannelStatus::Done, None);
    assert!(!reg.cancel(&id), "终态不可再取消");
}

#[test]
fn event_serde_roundtrip() {
    let ev = SideChannelEvent::Started {
        id: "side-00000001".into(),
        agent_name: "a".into(),
        prompt: "p".into(),
        started_at_ms: 1,
    };
    let json = serde_json::to_string(&ev).unwrap();
    assert!(
        json.contains("\"kind\":\"started\""),
        "snake_case tag:{json}"
    );
    let back: SideChannelEvent = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, SideChannelEvent::Started { .. }));
}
