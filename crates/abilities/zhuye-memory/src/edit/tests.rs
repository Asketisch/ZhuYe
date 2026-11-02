//! 条目级编辑测试(收编 ReflectDesktop memory_store/tests.rs 的 7 例,
//! 适配框架 API:scope 枚举 + `EntryWriteOptions` + `MemoryError`)。

use super::*;
use crate::scope::resolve_path;
use tempfile::TempDir;

fn store_in(dir: &TempDir) -> FileMemoryStore {
    FileMemoryStore::new(dir.path(), dir.path().join("home"))
}

fn write_opts() -> EntryWriteOptions {
    EntryWriteOptions::default()
}

#[test]
fn exact_line_match_rejects_prefix_and_extension() {
    let text = "## about\nabout body\n## a\na body\n";
    // key "a" 不能命中 "## about" 的前缀。
    assert_eq!(find_exact_heading_line(text, "## a"), Some((20, 24)));
    assert_eq!(find_exact_heading_line(text, "## about"), Some((0, 8)));
    assert_eq!(find_exact_heading_line(text, "## missing"), None);
}

#[test]
fn sections_can_be_created_updated_and_removed_independently() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    let create = EntryWriteOptions {
        create_only: true,
        ..Default::default()
    };
    store
        .add_entry(MemoryScope::Project, "agent", "one", "first", &create)
        .unwrap();
    store
        .add_entry(MemoryScope::Project, "agent", "two", "second", &create)
        .unwrap();
    // create_only 对已存在 key 报错。
    assert!(matches!(
        store.add_entry(MemoryScope::Project, "agent", "one", "replace", &create),
        Err(MemoryError::EntryExists { .. })
    ));
    store
        .add_entry(
            MemoryScope::Project,
            "agent",
            "one",
            "updated",
            &write_opts(),
        )
        .unwrap();
    let entries = store.list_entries(MemoryScope::Project, "agent").unwrap();
    let one = entries.iter().find(|e| e.key == "one").unwrap();
    let two = entries.iter().find(|e| e.key == "two").unwrap();
    assert_eq!(one.value, "updated");
    assert_eq!(two.value.trim_end(), "second");
    store
        .remove_entry(MemoryScope::Project, "agent", "one", &write_opts())
        .unwrap();
    let entries = store.list_entries(MemoryScope::Project, "agent").unwrap();
    assert!(!entries.iter().any(|e| e.key == "one"));
    assert!(
        entries
            .iter()
            .any(|e| e.key == "two" && e.value.trim_end() == "second")
    );
}

#[test]
fn editing_an_unchanged_section_preserves_its_whitespace() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    let path = store.path(MemoryScope::Project, "agent").unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = "## one\ncontent with spaces  \n\n## two\nsecond\n";
    std::fs::write(&path, original).unwrap();
    let entries = store.list_entries(MemoryScope::Project, "agent").unwrap();
    let entry = entries.iter().find(|e| e.key == "one").unwrap();
    assert_eq!(entry.value, "content with spaces  \n");
    store
        .add_entry(
            MemoryScope::Project,
            "agent",
            "one",
            &entry.value,
            &write_opts(),
        )
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn ambiguous_or_unheaded_memory_is_editable_as_a_document() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    let path = store.path(MemoryScope::Project, "agent").unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    for content in [
        "legacy text",
        "preamble\n## one\nvalue\n",
        "## one\na\n## one\nb\n",
    ] {
        std::fs::write(&path, content).unwrap();
        let entries = store.list_entries(MemoryScope::Project, "agent").unwrap();
        let entry = entries
            .iter()
            .find(|e| e.scope == MemoryScope::Project)
            .unwrap();
        assert_eq!(entry.key, WHOLE_DOCUMENT_KEY);
        assert!(entry.whole_document);
        assert_eq!(entry.value, content);
    }
    // 非 replace_document 形态写 "(all)" → 拒绝(整份替换必须显式声明)。
    assert!(
        store
            .add_entry(
                MemoryScope::Project,
                "agent",
                WHOLE_DOCUMENT_KEY,
                "new",
                &write_opts()
            )
            .is_err()
    );
    let replace = EntryWriteOptions {
        replace_document: true,
        ..Default::default()
    };
    store
        .add_entry(
            MemoryScope::Project,
            "agent",
            WHOLE_DOCUMENT_KEY,
            "updated document",
            &replace,
        )
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated document");
    store
        .remove_entry(MemoryScope::Project, "agent", WHOLE_DOCUMENT_KEY, &replace)
        .unwrap();
    assert!(!path.exists());
}

#[test]
fn malformed_memory_keys_are_rejected() {
    assert!(validate_key("heading\ninjected").is_err());
    assert!(validate_key(" padded ").is_err());
    assert!(validate_key("").is_err());
    assert!(validate_key("ok-key").is_ok());
}

#[test]
fn value_with_level_two_heading_is_rejected() {
    // value 里的 `## ` 会污染条目边界,写入前拒绝。
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    assert!(matches!(
        store.add_entry(
            MemoryScope::Project,
            "agent",
            "k",
            "body\n## injected\nmore",
            &write_opts()
        ),
        Err(MemoryError::HeadingInValue)
    ));
}

#[test]
fn expected_value_locks_out_stale_writes() {
    // 乐观锁:基于列表快照发起的修改,原内容已变 → 拒绝。
    // 快照值从 list_entries 取(末条目的 body 带尾随换行,与读侧
    // split_sections 口径一致),不硬编码。
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    store
        .add_entry(MemoryScope::Project, "agent", "k", "v1", &write_opts())
        .unwrap();
    let snapshot = store.list_entries(MemoryScope::Project, "agent").unwrap()[0]
        .value
        .clone();
    let stale = EntryWriteOptions {
        expected_value: Some("v0".into()),
        ..Default::default()
    };
    assert!(matches!(
        store.add_entry(MemoryScope::Project, "agent", "k", "v2", &stale),
        Err(MemoryError::DocumentChanged)
    ));
    let fresh = EntryWriteOptions {
        expected_value: Some(snapshot),
        ..Default::default()
    };
    store
        .add_entry(MemoryScope::Project, "agent", "k", "v2", &fresh)
        .unwrap();
    let entries = store.list_entries(MemoryScope::Project, "agent").unwrap();
    assert_eq!(entries[0].value.trim_end(), "v2");
}

#[test]
fn entry_path_matches_core_resolution() {
    // 不变量:写侧路径 == pre_loop 注入读侧路径(同一 resolve_path)。
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = FileMemoryStore::new(dir.path(), &home);
    for scope in [MemoryScope::Project, MemoryScope::User] {
        let expected = resolve_path(scope, dir.path(), &home, "agent").unwrap();
        assert_eq!(store.path(scope, "agent").unwrap(), expected);
    }
}

#[test]
fn migrate_legacy_agent_dir_moves_only_when_target_absent() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let legacy = resolve_path(MemoryScope::User, dir.path(), &home, "reflect").unwrap();
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "legacy memory").unwrap();
    let target = resolve_path(MemoryScope::User, dir.path(), &home, "coder").unwrap();

    migrate_legacy_agent_dir(dir.path(), &home, "reflect", "coder");
    assert!(!legacy.exists(), "迁移后源文件应已移走");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "legacy memory");

    // 目标已存在 → 不覆盖(用户新写的记忆优先)。
    std::fs::write(&legacy, "again").unwrap();
    std::fs::write(&target, "newer").unwrap();
    migrate_legacy_agent_dir(dir.path(), &home, "reflect", "coder");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "newer");
    assert!(legacy.exists(), "目标存在时源文件保留");

    // from == to → 空操作。
    migrate_legacy_agent_dir(dir.path(), &home, "coder", "coder");
}

#[test]
fn session_scope_is_not_editable() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(&dir);
    assert!(matches!(
        store.list_entries(MemoryScope::Session, "agent"),
        Err(MemoryError::SessionNotPersisted)
    ));
    assert!(matches!(
        store.add_entry(MemoryScope::Session, "agent", "k", "v", &write_opts()),
        Err(MemoryError::SessionNotPersisted)
    ));
}
