//! 验证评分入口对完整输出和缺失输出的判定。

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn 输入缺失时报告失败而不是通过() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zhuye-eval-score"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("启动评分器");
    child
        .stdin
        .take()
        .expect("评分器输入")
        .write_all(br#"{"cases":[{"name":"present","task":"t","assertions":[{"kind":"contains","text":"ok"}]},{"name":"missing","task":"t"}],"outputs":{"present":["ok",[]]}}"#)
        .expect("写入评分输入");
    let output = child.wait_with_output().expect("等待评分器");
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("解析评分结果");
    assert_eq!(report["total"], 2);
    assert_eq!(report["passed"], 1);
    assert_eq!(report["failed"], 1);
    assert_eq!(
        report["cases"][1]["assertions"][0]["kind"],
        "output_present"
    );
}
