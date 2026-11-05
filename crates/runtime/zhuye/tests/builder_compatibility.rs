//! 公共 Builder 的旧版构造链兼容性。

#[test]
fn with_defaults_preserves_existing_builder_chain() {
    let builder = zhuye::Zhuye::builder("mock")
        .with_defaults()
        .expect("默认配置入口应继续可用");
    assert_eq!(builder.model_spec(), "mock");
}
