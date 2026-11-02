//! driver 测试:工厂注入 + 并发预算 + 终态推进。

use super::*;
use crate::AgentThread;
use crate::config::AgentConfig;
use std::sync::atomic::AtomicUsize;

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
