//! driver 测试:工厂注入 + 并发预算 + 终态推进。

use super::*;
use crate::AgentThread;
use crate::config::AgentConfig;
use std::sync::atomic::AtomicUsize;

#[tokio::test]
async fn context_is_snapshotted_before_background_factory_runs() {
    let workspace = Arc::new(parking_lot::RwLock::new(std::path::PathBuf::from(
        "/before",
    )));
    let model = Arc::new(parking_lot::RwLock::new("mock/old".to_string()));
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let sender = Mutex::new(Some(sender));
    let factory: SideContextThreadFactory = Arc::new(move |_, _, context| {
        if let Some(sender) = sender.lock().take() {
            let _ = sender.send(context);
        }
        Err(anyhow::anyhow!("测试工厂"))
    });
    let driver = SideChannelDriver::new_with_context(
        SideChannelRegistry::new(),
        workspace.clone(),
        model.clone(),
        factory,
    );
    driver.start("agent", "prompt").unwrap();
    *workspace.write() = "/after".into();
    *model.write() = "mock/new".into();
    let context = receiver.await.unwrap();
    assert_eq!(context.workspace, std::path::PathBuf::from("/before"));
    assert_eq!(context.model, "mock/old");
}

/// 计数工厂:记录被构造次数(经 oneshot 通知,测试确定性等待),并返回
/// 可提交的 AgentThread。
fn counting_factory(
    counter: Arc<AtomicUsize>,
) -> (SideThreadFactory, tokio::sync::oneshot::Receiver<()>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = std::sync::Mutex::new(Some(tx));
    let factory: SideThreadFactory = Arc::new(move |_agent, cancel| {
        counter.fetch_add(1, Ordering::Relaxed);
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        let cfg = AgentConfig::new("mock", "/tmp").with_cancel(cancel);
        Ok(AgentThread::new(
            cfg,
            Default::default(),
            Default::default(),
            None,
            None,
        ))
    });
    (factory, rx)
}

#[tokio::test]
async fn start_registers_and_drives_thread() {
    let reg = SideChannelRegistry::new();
    let counter = Arc::new(AtomicUsize::new(0));
    let (factory, constructed) = counting_factory(counter.clone());
    let driver = SideChannelDriver::new(reg.clone(), factory);

    let (id, _cancel) = driver.start("agent", "hello").expect("start");
    assert!(id.starts_with("side-"));
    // 工厂被调用一次(oneshot 确定性等待,不靠睡眠竞态)。
    constructed.await.expect("factory 应被调用");
    assert_eq!(counter.load(Ordering::Relaxed), 1, "工厂应被调用一次");
    // Started 事件已同步发出(registry.start 内)。
    let info = reg.get(&id).expect("快照应存在");
    assert_eq!(info.agent_name, "agent");
    assert_eq!(info.prompt, "hello");
    // 注:完整 turn 的完成时序取决于引擎内部(空 registry 下的 model_call
    // 行为),不属于本模块关注面;终态推进由下面的 failure/cancel 用例覆盖。
}

#[tokio::test]
async fn model_failure_marks_error_terminal() {
    let registry = SideChannelRegistry::new();
    let (factory, _) = counting_factory(Arc::default());
    let driver = SideChannelDriver::new(registry, factory);
    let (id, _) = driver.start("agent", "prompt").unwrap();
    for _ in 0..100 {
        if let Some(info) = driver.registry().get(&id) {
            if info.status == "error" {
                return;
            }
            assert_ne!(info.status, "done", "模型失败不应报告成功");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("模型失败后必须进入 error 终态");
}

#[tokio::test]
async fn factory_failure_marks_error_terminal() {
    let reg = SideChannelRegistry::new();
    let factory: SideThreadFactory =
        Arc::new(|_agent, _cancel| Err(anyhow::anyhow!("agent def missing")));
    let driver = SideChannelDriver::new(reg.clone(), factory);

    let (id, _cancel) = driver.start("missing", "p").expect("注册成功");
    // 等待 Error 终态。
    for _ in 0..100 {
        if let Some(info) = driver.registry().get(&id)
            && info.status == "error"
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "factory 失败应推进 error 终态:{:?}",
        driver.registry().get(&id)
    );
}

#[test]
fn dropped_task_guard_marks_error_and_releases_slot() {
    let registry = SideChannelRegistry::new();
    let (id, _) = registry.start("agent".into(), "prompt".into());
    let counter = Arc::new(AtomicUsize::new(1));
    let routes = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = mpsc::channel(1);
    routes.lock().insert(
        (InteractionKind::Approval, "request".into()),
        (id.clone(), tx),
    );
    drop(InFlightGuard {
        in_flight: counter.clone(),
        registry: registry.clone(),
        id: id.clone(),
        approval_routes: routes.clone(),
    });
    assert_eq!(counter.load(Ordering::Relaxed), 0);
    assert_eq!(registry.get(&id).unwrap().status, "error");
    assert!(routes.lock().is_empty());
}

#[tokio::test]
async fn factory_panic_marks_error_terminal() {
    let registry = SideChannelRegistry::new();
    let factory: SideThreadFactory = Arc::new(|_, _| panic!("测试工厂异常"));
    let driver = SideChannelDriver::new(registry, factory);
    let (id, _) = driver.start("agent", "prompt").unwrap();
    for _ in 0..100 {
        if driver
            .registry()
            .get(&id)
            .is_some_and(|info| info.status == "error")
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("工厂异常后必须进入 error 终态");
}

#[tokio::test]
async fn busy_when_concurrency_exhausted() {
    let reg = SideChannelRegistry::new();
    let (factory, _rx) = counting_factory(Arc::default());
    let driver = SideChannelDriver::new(reg.clone(), factory);
    // 直接塞满在途计数(不真的起线程)。
    driver.in_flight.store(MAX_CONCURRENT, Ordering::Relaxed);
    let err = driver.start("a", "p").expect_err("应 Busy");
    assert_eq!(err, SideChannelStartError::Busy);
    // registry 未被污染(Busy 发生在注册前)。
    assert_eq!(reg.running_count(), 0);
}

#[tokio::test]
async fn concurrent_start_respects_limit() {
    let driver = SideChannelDriver::without_factory(SideChannelRegistry::new());
    driver
        .in_flight
        .store(MAX_CONCURRENT - 1, Ordering::Relaxed);
    // 暂停后台任务,使成功取得的额度保持占用,只验证并发入场。
    let barrier = Arc::new(std::sync::Barrier::new(17));
    let runtime = tokio::runtime::Handle::current();
    let success = std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..16 {
            let driver = driver.clone();
            let barrier = barrier.clone();
            let runtime = runtime.clone();
            workers.push(scope.spawn(move || {
                let _entered = runtime.enter();
                barrier.wait();
                driver.start("agent", "prompt").is_ok()
            }));
        }
        barrier.wait();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|success| *success)
            .count()
    });
    assert_eq!(success, 1);
}

#[tokio::test]
async fn cancel_is_independent_of_other_channels() {
    let reg = SideChannelRegistry::new();
    let (factory, _rx) = counting_factory(Arc::default());
    let driver = SideChannelDriver::new(reg.clone(), factory);
    let (id1, _c1) = driver.start("a", "p1").unwrap();
    let (id2, _c2) = driver.start("a", "p2").unwrap();
    assert!(driver.cancel(&id1));
    assert!(!driver.cancel(&id1), "终态不可再取消");
    assert_eq!(driver.registry().get(&id2).unwrap().status, "running");
}

#[tokio::test]
async fn without_factory_marks_error() {
    let reg = SideChannelRegistry::new();
    let driver = SideChannelDriver::without_factory(reg.clone());
    let (id, _c) = driver.start("a", "p").expect("注册成功");
    // 后台 task 会跑;同步等待不可行,这里只验注册语义。
    assert!(reg.get(&id).is_some());
}
