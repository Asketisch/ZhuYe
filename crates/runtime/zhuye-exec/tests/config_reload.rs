//! 端到端:ConfigWatcher 检测到 TOML 变更 → 重建 registry → 下一轮 `apply_to_registry` 拿到新 client。
//!
//! 这里只验证 watcher 派发新 cfg + `apply_to_registry` 不报错,而不真发 HTTP 请求。

use std::time::Duration;

use zhuye_config::{ConfigWatcher, load_from_file};
use zhuye_llm::ModelRegistry;

/// 一个 anthropic 端点的模型组(v2 的唯一接入单元)。
const GROUP_TOML_A1: &str =
    "[[model_group]]\nid = \"anthropic\"\nendpoint = \"anthropic\"\napi_key = \"sk-a-1\"\n";
const GROUP_TOML_A2: &str =
    "[[model_group]]\nid = \"anthropic\"\nendpoint = \"anthropic\"\napi_key = \"sk-a-2\"\n";

#[tokio::test(flavor = "current_thread")]
async fn watcher_triggers_registry_replacement_on_toml_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, GROUP_TOML_A1).unwrap();

    let initial = load_from_file(&path).unwrap();
    let watcher = ConfigWatcher::spawn(path.clone(), initial.clone()).unwrap();
    let registry = ModelRegistry::new();
    initial.apply_to_registry(&registry).unwrap();

    let mut rx = watcher.subscribe();

    // 改文件 —— 触发 notify event。
    std::fs::write(&path, GROUP_TOML_A2).unwrap();

    // 等待 watcher 检测 + debounce + reload + send。
    let new_cfg = tokio::time::timeout(Duration::from_secs(3), async {
        rx.changed().await.ok();
        rx.borrow().clone()
    })
    .await
    .expect("reload timed out");

    new_cfg.apply_to_registry(&registry).unwrap();
    assert_eq!(new_cfg.model_group[0].api_key.as_deref(), Some("sk-a-2"));
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_toml_does_not_clear_old_clients() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, GROUP_TOML_A1).unwrap();

    let initial = load_from_file(&path).unwrap();
    let watcher = ConfigWatcher::spawn(path.clone(), initial.clone()).unwrap();
    assert!(!watcher.current().model_group.is_empty());

    // 写非法 TOML —— watcher 会 warn 但保留上一个值。
    std::fs::write(&path, "garbage = = =").unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let last = watcher.current();
    assert!(
        !last.model_group.is_empty(),
        "previous valid config must be retained"
    );
    assert_eq!(last.model_group[0].api_key.as_deref(), Some("sk-a-1"));
}
