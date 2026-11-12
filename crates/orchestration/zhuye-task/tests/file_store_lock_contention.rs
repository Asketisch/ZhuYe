//! 多个 `FileTaskStore` 实例争用同一列表时的跨进程锁回归。

use std::sync::Arc;

use zhuye_task::{FileTaskStore, Task, TaskId, TaskStatus, TaskStore};

fn task(id: TaskId, list: &str) -> Task {
    let now = std::time::SystemTime::UNIX_EPOCH;
    Task {
        id,
        list_id: list.into(),
        subject: format!("lock-{id}"),
        description: String::new(),
        active_form: None,
        owner: None,
        status: TaskStatus::Pending,
        blocks: vec![],
        blocked_by: vec![],
        metadata: serde_json::json!({}),
        output_path: None,
        claimed_by: None,
        claimed_at: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_file_stores_allocate_distinct_ids_and_keep_both_tasks() {
    let dir = tempfile::TempDir::new().unwrap();
    let first = Arc::new(FileTaskStore::new(dir.path()));
    let second = Arc::new(FileTaskStore::new(dir.path()));
    let list = "lock-test".to_string();

    let (a, b) = tokio::join!(
        {
            let store = first.clone();
            let list = list.clone();
            async move { store.next_id(&list).await.unwrap() }
        },
        {
            let store = second.clone();
            let list = list.clone();
            async move { store.next_id(&list).await.unwrap() }
        }
    );
    assert_ne!(a, b, "两个实例不得分配同一 task id");
    first.save(&list, &task(a, &list)).await.unwrap();
    second.save(&list, &task(b, &list)).await.unwrap();

    let listed = second.list(&list).await.unwrap();
    assert_eq!(listed.len(), 2, "并发写入后任务不应互相覆盖");
    assert!(listed.iter().any(|item| item.id == a));
    assert!(listed.iter().any(|item| item.id == b));
}
