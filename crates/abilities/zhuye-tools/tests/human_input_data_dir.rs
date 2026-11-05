//! 人工输入持久化的数据目录与显式覆盖路径回归。

use std::process::Command;
use zhuye_tools::HumanInputStore;

#[test]
fn child_writes_pending_request() {
    if std::env::var_os("ZHUYE_TEST_INPUT_CHILD").is_none() {
        return;
    }
    HumanInputStore::from_env_or_default()
        .unwrap()
        .write_pending("test", "确认输入");
}

#[test]
fn pending_request_uses_data_directory_and_explicit_override() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("custom");
    let explicit = tmp.path().join("other");
    std::fs::create_dir_all(&home).unwrap();
    let run = |override_path: Option<&std::path::Path>| {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.arg("--exact")
            .arg("child_writes_pending_request")
            .env("ZHUYE_TEST_INPUT_CHILD", "1")
            .env("HOME", &home)
            .env("ZHUYE_HOME", &data)
            .env_remove("ZHUYE_HUMAN_INPUT_DIR");
        if let Some(path) = override_path {
            cmd.env("ZHUYE_HUMAN_INPUT_DIR", path);
        }
        assert!(cmd.status().unwrap().success());
    };
    run(None);
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data.join("human_input/test.json")).unwrap())
            .unwrap();
    assert_eq!(request["prompt"], "确认输入");
    assert!(!home.join(".zhuye/human_input").exists());
    run(Some(&explicit));
    let explicit_request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(explicit.join("test.json")).unwrap()).unwrap();
    assert_eq!(explicit_request["prompt"], "确认输入");
}
