//! 公共 Builder 的旧版构造链兼容性。

#[test]
fn with_defaults_preserves_existing_builder_chain() {
    // with_defaults 不再是 Result(旧实现恒 Ok),直接返回 builder。
    let builder = zhuye::Zhuye::builder("mock").with_defaults();
    assert_eq!(builder.model_spec(), "mock");
}
