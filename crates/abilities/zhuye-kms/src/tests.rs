use super::*;

fn create_test_manager() -> (KnowledgeManager, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let manager = KnowledgeManager::new(dir.path().to_path_buf());
    (manager, dir)
}

#[test]
fn create_and_list_wiki() {
    let (manager, _dir) = create_test_manager();
    let wiki = manager
        .create_wiki("test-wiki", Some("Test wiki".to_string()))
        .unwrap();
    assert_eq!(wiki.name, "test-wiki");
    assert_eq!(wiki.page_count, 0);

    let wikis = manager.list_wikis().unwrap();
    assert_eq!(wikis.len(), 1);
    assert_eq!(wikis[0].name, "test-wiki");
}

#[test]
fn concurrent_stale_saves_do_not_overwrite_the_winner() {
    use std::sync::{Arc, Barrier};
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(KnowledgeManager::new(dir.path().to_path_buf()));
    manager.create_wiki("wiki", None).unwrap();
    manager
        .save_page("wiki", "page", "original", None, vec![])
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|content| {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                manager.save_page_if_unchanged("wiki", "page", content, None, vec![], "original")
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(KmsError::PageChanged { .. })))
            .count(),
        1
    );
    assert_ne!(
        manager.get_page("wiki", "page").unwrap().content,
        "original"
    );
}

#[test]
fn stale_save_does_not_recreate_a_deleted_page() {
    let (manager, dir) = create_test_manager();
    manager.create_wiki("wiki", None).unwrap();
    manager
        .save_page("wiki", "page", "original", None, vec![])
        .unwrap();
    let path = dir.path().join("wiki/pages/page.md");
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        manager.save_page_if_unchanged("wiki", "page", "draft", None, vec![], "original"),
        Err(KmsError::PageChanged { .. })
    ));
    assert!(!path.exists());
}

#[test]
fn unavailable_root_and_unreadable_page_are_reported() {
    let (manager, dir) = create_test_manager();
    manager.create_wiki("wiki", None).unwrap();
    std::fs::write(dir.path().join("wiki/pages/broken.md"), [0xff]).unwrap();
    assert!(manager.list_pages("wiki").is_err());
    assert!(manager.search("anything").is_err());
    std::fs::remove_file(dir.path().join("wiki/pages/broken.md")).unwrap();
    std::fs::rename(dir.path(), dir.path().with_extension("moved")).unwrap();
    std::fs::write(dir.path(), b"not a directory").unwrap();
    assert!(manager.list_wikis().is_err());
    assert!(manager.search("anything").is_err());
    std::fs::remove_file(dir.path()).unwrap();
    std::fs::rename(dir.path().with_extension("moved"), dir.path()).unwrap();
}

#[test]
fn wiki_description_round_trips_special_characters_and_empty_string() {
    let (manager, dir) = create_test_manager();
    for (name, description) in [
        (
            "multiline",
            "first line\n---\nquoted \"value\" # note\\path",
        ),
        ("empty", ""),
        ("spaces", "  keep surrounding spaces  "),
    ] {
        manager
            .create_wiki(name, Some(description.to_string()))
            .unwrap();
        let listed = manager
            .list_wikis()
            .unwrap()
            .into_iter()
            .find(|wiki| wiki.name == name)
            .unwrap();
        assert_eq!(listed.description.as_deref(), Some(description));
        let contents = std::fs::read_to_string(dir.path().join(name).join("index.md")).unwrap();
        assert_eq!(contents.lines().count(), 3);
    }
}

#[test]
fn create_wiki_already_exists_error() {
    let (manager, _dir) = create_test_manager();
    manager.create_wiki("test-wiki", None).unwrap();
    let result = manager.create_wiki("test-wiki", None);
    assert!(matches!(result, Err(KmsError::WikiAlreadyExists { .. })));
}

#[test]
fn save_and_get_page() {
    let (manager, _dir) = create_test_manager();
    manager.create_wiki("test-wiki", None).unwrap();

    let page = manager
        .save_page(
            "test-wiki",
            "hello",
            "# Hello World\nThis is a test page.",
            Some("Hello".to_string()),
            vec!["test".to_string()],
        )
        .unwrap();

    assert_eq!(page.name, "hello");
    assert_eq!(page.wiki, "test-wiki");
    assert_eq!(page.title.as_deref(), Some("Hello"));
    assert_eq!(page.tags, vec!["test"]);
    assert!(page.content.contains("# Hello World"));
}

#[test]
fn saving_full_page_preserves_frontmatter_and_body() {
    let (manager, _dir) = create_test_manager();
    manager.create_wiki("wiki", None).unwrap();
    let original = manager
        .save_page(
            "wiki",
            "page",
            "body\n",
            Some("Title".into()),
            vec!["tag".into()],
        )
        .unwrap();
    let saved = manager
        .save_page("wiki", "page", &original.content, None, vec![])
        .unwrap();
    assert_eq!(saved.content, original.content);
    assert_eq!(saved.title.as_deref(), Some("Title"));
    assert_eq!(saved.tags, vec!["tag"]);
    let reloaded = manager.get_page("wiki", "page").unwrap();
    assert_eq!(reloaded.content, original.content);
    assert_eq!(reloaded.tags, vec!["tag"]);
}

#[test]
fn list_pages() {
    let (manager, _dir) = create_test_manager();
    manager.create_wiki("test-wiki", None).unwrap();
    manager
        .save_page("test-wiki", "page1", "Content 1", None, vec![])
        .unwrap();
    manager
        .save_page("test-wiki", "page2", "Content 2", None, vec![])
        .unwrap();

    let pages = manager.list_pages("test-wiki").unwrap();
    assert_eq!(pages.len(), 2);
}

#[test]
fn search_pages() {
    let (manager, _dir) = create_test_manager();
    manager.create_wiki("test-wiki", None).unwrap();
    manager
        .save_page(
            "test-wiki",
            "hello",
            "# Hello World\nThis is a test page.",
            None,
            vec![],
        )
        .unwrap();
    manager
        .save_page(
            "test-wiki",
            "goodbye",
            "# Goodbye\nThis is another page.",
            None,
            vec![],
        )
        .unwrap();

    let results = manager.search("test").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].wiki, "test-wiki");
    assert_eq!(results[0].page, "hello");
}

#[test]
fn delete_wiki() {
    let (manager, _dir) = create_test_manager();
    manager.create_wiki("test-wiki", None).unwrap();
    manager.delete_wiki("test-wiki").unwrap();

    let wikis = manager.list_wikis().unwrap();
    assert_eq!(wikis.len(), 0);
}

#[test]
fn concurrent_dreams_preserve_each_result() {
    let (manager, _dir) = create_test_manager();
    let manager = std::sync::Arc::new(manager);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|i| {
            let manager = manager.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                manager.dream(vec![format!("insight-{i}")], 1).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    let unique: std::collections::HashSet<_> = results.iter().map(|r| &r.page).collect();
    assert_eq!(unique.len(), 8);
    for (i, result) in results.iter().enumerate() {
        let page = manager.get_page(&result.wiki, &result.page).unwrap();
        assert!(page.content.contains(&format!("insight-{i}")));
    }
}

#[test]
fn concurrent_wiki_creation_has_one_winner() {
    let (manager, _dir) = create_test_manager();
    let manager = std::sync::Arc::new(manager);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let manager = manager.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                manager.create_wiki("shared", None)
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter(|r| r.is_err())
            .all(|r| matches!(r, Err(KmsError::WikiAlreadyExists { .. })))
    );
}

#[cfg(unix)]
#[test]
fn linked_page_cannot_read_or_overwrite_files_outside_kms() {
    use std::os::unix::fs::symlink;
    let (manager, _dir) = create_test_manager();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("private.md");
    std::fs::write(&target, "private sentinel").unwrap();
    manager.create_wiki("wiki", None).unwrap();
    symlink(&target, manager.page_path("wiki", "linked")).unwrap();
    assert!(manager.get_page("wiki", "linked").is_err());
    assert!(
        manager
            .save_page("wiki", "linked", "replacement", None, vec![])
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "private sentinel"
    );
    assert!(manager.list_pages("wiki").unwrap().is_empty());
    assert!(manager.search("private sentinel").unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn linked_wiki_and_pages_directory_do_not_escape_root() {
    use std::os::unix::fs::symlink;
    let (manager, dir) = create_test_manager();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(outside.path().join("pages")).unwrap();
    std::fs::write(outside.path().join("pages/secret.md"), "hidden data").unwrap();
    symlink(outside.path(), dir.path().join("linked-wiki")).unwrap();
    assert!(manager.list_wikis().unwrap().is_empty());
    assert!(manager.list_pages("linked-wiki").is_err());
    assert!(
        manager
            .save_page("linked-wiki", "new", "bad", None, vec![])
            .is_err()
    );
    assert!(manager.delete_wiki("linked-wiki").is_err());
    assert!(manager.search("hidden data").unwrap().is_empty());
    manager.create_wiki("normal", None).unwrap();
    std::fs::remove_dir(dir.path().join("normal/pages")).unwrap();
    symlink(
        outside.path().join("pages"),
        dir.path().join("normal/pages"),
    )
    .unwrap();
    assert!(manager.list_pages("normal").is_err());
    assert!(manager.get_page("normal", "secret").is_err());
    assert!(
        manager
            .save_page("normal", "new", "bad", None, vec![])
            .is_err()
    );
    assert!(manager.search("hidden data").unwrap().is_empty());
    assert!(!outside.path().join("pages/new.md").exists());
}

#[test]
fn direct_domain_calls_reject_traversal_names() {
    let (manager, _dir) = create_test_manager();
    manager.create_wiki("wiki", None).unwrap();
    assert!(manager.create_wiki("../outside", None).is_err());
    assert!(
        manager
            .save_page("wiki", "../outside", "bad", None, vec![])
            .is_err()
    );
    assert!(manager.get_page("../outside", "page").is_err());
    assert!(manager.delete_wiki("../outside").is_err());
}
