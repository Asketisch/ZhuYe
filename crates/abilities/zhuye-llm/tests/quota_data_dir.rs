//! 配额持久化的数据目录与显式覆盖路径回归。

use std::process::Command;
use zhuye_llm::{QuotaConfig, QuotaTracker};

#[test]
fn child_writes_quota_state() {
    if std::env::var_os("ZHUYE_TEST_QUOTA_CHILD").is_none() {
        return;
    }
    let tracker = QuotaTracker::new();
    tracker.register(
        "mock",
        "local",
        QuotaConfig {
            window_secs: 3600,
            max_tokens: 100,
            check_via: None,
        },
    );
    tracker.record_usage("mock", "local", 7);
    tracker.save_state_if_configured();
}

#[test]
fn quota_state_uses_data_directory_and_explicit_override() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("custom");
    let explicit = tmp.path().join("other/state.json");
    std::fs::create_dir_all(&home).unwrap();
    let run = |override_path: Option<&std::path::Path>| {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.arg("--exact")
            .arg("child_writes_quota_state")
            .env("ZHUYE_TEST_QUOTA_CHILD", "1")
            .env("HOME", &home)
            .env("ZHUYE_HOME", &data)
            .env_remove("ZHUYE_QUOTA_STATE");
        if let Some(path) = override_path {
            cmd.env("ZHUYE_QUOTA_STATE", path);
        }
        assert!(cmd.status().unwrap().success());
    };
    run(None);
    let default_path = data.join("quota-state.json");
    let records: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&default_path).unwrap()).unwrap();
    assert_eq!(records[0]["used_tokens"], 7);
    assert!(!home.join(".zhuye/quota-state.json").exists());
    run(Some(&explicit));
    let explicit_records: serde_json::Value =
        serde_json::from_slice(&std::fs::read(explicit).unwrap()).unwrap();
    assert_eq!(explicit_records[0]["used_tokens"], 7);
}
