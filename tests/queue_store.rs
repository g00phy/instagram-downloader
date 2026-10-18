//! Queue operations, `queue.json` persistence and locking, and `archive.txt`.

use std::path::PathBuf;

use igdl::archive::Archive;
use igdl::error::{Error, ErrorKind};
use igdl::model::{Category, ItemError, ItemStatus, MediaFile, MediaKind};
use igdl::queue::{ClearFilter, QUEUE_VERSION, Queue, QueueStore, StatusCounts};

fn media(key: &str) -> MediaFile {
    MediaFile {
        key: key.to_string(),
        post_id: format!("post{key}"),
        shortcode: Some("C0ffeeXYZ12".to_string()),
        index: 1,
        count: 1,
        kind: MediaKind::Image,
        category: Category::Post,
        url: format!("https://cdn.example.invalid/{key}.jpg"),
        width: Some(1080),
        height: Some(1350),
        owner: "tester".to_string(),
        owner_id: Some("42".to_string()),
        taken_at: 1_700_000_000,
        caption: None,
        highlight: None,
        ext: "jpg".to_string(),
    }
}

fn files(keys: &[&str]) -> Vec<(MediaFile, PathBuf)> {
    keys.iter()
        .map(|key| (media(key), PathBuf::from(format!("tester/posts/{key}.jpg"))))
        .collect()
}

fn add(queue: &mut Queue, name: &str, keys: &[&str]) -> Option<u64> {
    queue.add_package(
        name.to_string(),
        format!("https://www.instagram.com/{name}/"),
        PathBuf::from("/downloads"),
        files(keys),
    )
}

fn set_status(queue: &mut Queue, id: u64, status: ItemStatus) {
    let item = queue.item_mut(id).expect("item exists");
    item.status = status;
    if status == ItemStatus::Failed {
        item.error = Some(ItemError {
            kind: ErrorKind::Network,
            message: "reset".to_string(),
        });
    }
}

#[test]
fn add_package_assigns_ids_and_dedupes() {
    let mut queue = Queue::default();
    let first = add(&mut queue, "a", &["1", "2", "2", "3"]).unwrap();
    assert_eq!(first, 1);
    let package = queue.package(1).unwrap();
    let ids: Vec<u64> = package.items.iter().map(|item| item.id).collect();
    assert_eq!(
        ids,
        vec![2, 3, 4],
        "duplicate key inside one package is dropped"
    );
    assert_eq!(queue.next_id, 5);
    assert!(package.created_at > 1_600_000_000);
    for item in &package.items {
        assert_eq!(item.status, ItemStatus::Queued);
        assert_eq!(item.bytes_done, 0);
        assert_eq!(item.bytes_total, None);
        assert!(item.error.is_none() && item.path.is_none() && item.note.is_none());
    }

    // Keys already queued are dropped; the rest gets fresh ids.
    let second = add(&mut queue, "b", &["3", "4"]).unwrap();
    assert_eq!(second, 5);
    let package = queue.package(5).unwrap();
    assert_eq!(package.items.len(), 1);
    assert_eq!(package.items[0].id, 6);
    assert_eq!(package.items[0].media.key, "4");
    assert_eq!(queue.next_id, 7);

    // Nothing new: no package, next_id unchanged.
    assert_eq!(add(&mut queue, "c", &["1", "4"]), None);
    assert_eq!(add(&mut queue, "d", &[]), None);
    assert_eq!(queue.next_id, 7);
    assert_eq!(queue.packages.len(), 2);

    assert!(queue.contains_key("2"));
    assert!(!queue.contains_key("9"));
    assert_eq!(queue.item(6).unwrap().media.key, "4");
    assert_eq!(queue.package_of(3).unwrap().id, 1);
    assert!(
        queue.package_of(1).is_none(),
        "package ids are not item ids"
    );
    assert!(queue.item(1).is_none());
    assert!(queue.package(2).is_none());
}

#[test]
fn pending_respects_status_and_package_filter() {
    let mut queue = Queue::default();
    add(&mut queue, "a", &["1", "2"]); // 1: items 2, 3
    add(&mut queue, "b", &["3"]); // 4: item 5
    set_status(&mut queue, 3, ItemStatus::Done);
    assert_eq!(queue.pending(&[]), vec![2, 5]);
    assert_eq!(queue.pending(&[4]), vec![5]);
    assert_eq!(queue.pending(&[1, 4]), vec![2, 5]);
    assert_eq!(queue.pending(&[99]), Vec::<u64>::new());
}

#[test]
fn reset_interrupted_requeues_downloading() {
    let mut queue = Queue::default();
    add(&mut queue, "a", &["1", "2", "3"]);
    set_status(&mut queue, 2, ItemStatus::Downloading);
    set_status(&mut queue, 3, ItemStatus::Downloading);
    set_status(&mut queue, 4, ItemStatus::Done);
    assert_eq!(queue.reset_interrupted(), 2);
    assert_eq!(queue.item(2).unwrap().status, ItemStatus::Queued);
    assert_eq!(queue.item(4).unwrap().status, ItemStatus::Done);
    assert_eq!(queue.reset_interrupted(), 0);
}

#[test]
fn retry_accepts_package_and_item_ids() {
    let mut queue = Queue::default();
    add(&mut queue, "a", &["1", "2"]); // 1: 2, 3
    add(&mut queue, "b", &["3", "4"]); // 4: 5, 6
    for id in [2, 3, 5, 6] {
        set_status(&mut queue, id, ItemStatus::Failed);
    }
    // Package 1 and item 6.
    assert_eq!(queue.retry(&[1, 6]), 3);
    for id in [2, 3, 6] {
        let item = queue.item(id).unwrap();
        assert_eq!(item.status, ItemStatus::Queued);
        assert!(item.error.is_none());
    }
    assert_eq!(queue.item(5).unwrap().status, ItemStatus::Failed);
    assert_eq!(queue.retry(&[99]), 0);
    // Empty = every failed item.
    assert_eq!(queue.retry(&[]), 1);
    assert_eq!(queue.counts().failed, 0);
}

#[test]
fn remove_mixes_package_and_item_ids() {
    let mut queue = Queue::default();
    add(&mut queue, "a", &["1", "2"]); // 1: 2, 3
    add(&mut queue, "b", &["3", "4"]); // 4: 5, 6
    add(&mut queue, "c", &["5"]); // 7: 8
    assert_eq!(queue.remove(&[1, 5, 8, 999]), 4);
    let ids: Vec<u64> = queue.packages.iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![4], "package 7 was emptied and dropped");
    assert_eq!(queue.package(4).unwrap().items[0].id, 6);
    assert_eq!(queue.remove(&[]), 0);
    // Ids are never reused.
    assert_eq!(add(&mut queue, "d", &["1"]), Some(9));
}

#[test]
fn clear_filters() {
    let mut queue = Queue::default();
    add(&mut queue, "done", &["1", "2"]); // 1: 2, 3
    add(&mut queue, "mixed", &["3", "4"]); // 4: 5, 6
    add(&mut queue, "failed", &["5"]); // 7: 8
    set_status(&mut queue, 2, ItemStatus::Done);
    set_status(&mut queue, 3, ItemStatus::Skipped);
    set_status(&mut queue, 5, ItemStatus::Done);
    set_status(&mut queue, 6, ItemStatus::Failed);
    set_status(&mut queue, 8, ItemStatus::Failed);

    let mut finished = queue.clone();
    assert_eq!(finished.clear(ClearFilter::Finished), 2);
    let ids: Vec<u64> = finished.packages.iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![4, 7]);

    let mut failed = queue.clone();
    assert_eq!(failed.clear(ClearFilter::Failed), 2);
    let ids: Vec<u64> = failed.packages.iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![1, 4]);
    assert_eq!(failed.package(4).unwrap().items.len(), 1);

    let next = queue.next_id;
    assert_eq!(queue.clear(ClearFilter::All), 5);
    assert!(queue.packages.is_empty());
    assert_eq!(queue.next_id, next);
}

#[test]
fn counts_per_status() {
    let mut queue = Queue::default();
    add(&mut queue, "a", &["1", "2", "3", "4", "5", "6"]);
    set_status(&mut queue, 3, ItemStatus::Downloading);
    set_status(&mut queue, 4, ItemStatus::Done);
    set_status(&mut queue, 5, ItemStatus::Skipped);
    set_status(&mut queue, 6, ItemStatus::Failed);
    let counts = queue.counts();
    assert_eq!(
        counts,
        StatusCounts {
            queued: 2,
            downloading: 1,
            done: 1,
            skipped: 1,
            failed: 1,
        }
    );
    assert_eq!(counts.total(), 6);
    let package = queue.package(1).unwrap();
    let first_two = StatusCounts::of(&package.items[..2]);
    assert_eq!((first_two.queued, first_two.downloading), (1, 1));
}

#[test]
fn save_and_load_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("nested data ü");
    let store = QueueStore::open(&data).unwrap();
    assert_eq!(store.load().unwrap(), Queue::default());

    let mut queue = Queue::default();
    add(&mut queue, "a", &["1", "2"]);
    set_status(&mut queue, 3, ItemStatus::Failed);
    store.save(&queue).unwrap();

    assert_eq!(store.load().unwrap(), queue);
    assert_eq!(QueueStore::read_unlocked(&data).unwrap(), queue);
    assert!(data.join("queue.json").is_file());
    assert!(
        !data.join("queue.json.tmp").exists(),
        "the temp file is renamed away"
    );
    let text = std::fs::read_to_string(data.join("queue.json")).unwrap();
    assert!(text.contains("\n  \"version\": 1"), "pretty JSON: {text}");

    // Saving again replaces the file.
    queue.clear(ClearFilter::All);
    store.save(&queue).unwrap();
    assert_eq!(store.load().unwrap(), queue);
    assert!(!data.join("queue.json.tmp").exists());
}

#[test]
fn read_unlocked_on_missing_dir_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let queue = QueueStore::read_unlocked(&dir.path().join("absent")).unwrap();
    assert_eq!(queue, Queue::default());
}

#[test]
fn corrupt_queue_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("queue.json"), "{ not json").unwrap();
    let store = QueueStore::open(dir.path()).unwrap();
    let err = store.load().unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err:?}");
    let message = err.to_string();
    assert!(message.contains("queue.json is corrupt"), "{message}");
    assert!(message.contains("move it away"), "{message}");

    // Valid JSON of the wrong shape is corrupt too.
    std::fs::write(dir.path().join("queue.json"), "[1, 2]").unwrap();
    assert!(matches!(store.load(), Err(Error::Config(_))));
    assert!(matches!(
        QueueStore::read_unlocked(dir.path()),
        Err(Error::Config(_))
    ));
}

#[test]
fn newer_queue_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let json = format!(
        r#"{{"version": {}, "next_id": 1, "packages": [], "new_field": true}}"#,
        QUEUE_VERSION + 1
    );
    std::fs::write(dir.path().join("queue.json"), json).unwrap();
    let err = QueueStore::read_unlocked(dir.path()).unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err:?}");
    assert!(err.to_string().contains("upgrade"), "{err}");
}

#[test]
fn second_store_on_same_dir_is_refused_until_drop() {
    let dir = tempfile::tempdir().unwrap();
    let first = QueueStore::open(dir.path()).unwrap();
    let err = QueueStore::open(dir.path()).unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err:?}");
    assert!(err.to_string().contains("another igdl process"), "{err}");
    // Reading without the lock still works while it is held.
    QueueStore::read_unlocked(dir.path()).unwrap();
    drop(first);
    let again = QueueStore::open(dir.path()).unwrap();
    again.save(&Queue::default()).unwrap();
}

#[test]
fn archive_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub dir").join("archive.txt");
    let mut archive = Archive::open(&path).unwrap();
    assert!(archive.is_empty());
    assert!(!path.exists(), "created lazily on the first insert");

    archive.insert("111").unwrap();
    archive.insert("222").unwrap();
    archive.insert("111").unwrap();
    assert_eq!(archive.len(), 2);
    assert!(archive.contains("111"));
    assert!(!archive.contains("333"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "111\n222\n");

    let mut reopened = Archive::open(&path).unwrap();
    assert_eq!(reopened.len(), 2);
    assert!(reopened.contains("222"));
    reopened.insert("333").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "111\n222\n333\n");
}

#[test]
fn archive_tolerates_blank_lines_and_missing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive.txt");
    std::fs::write(&path, "  111  \n\n\r\n222").unwrap();
    let mut archive = Archive::open(&path).unwrap();
    assert_eq!(archive.len(), 2);
    assert!(archive.contains("111"));
    archive.insert("333").unwrap();
    let reopened = Archive::open(&path).unwrap();
    assert_eq!(reopened.len(), 3);
    assert!(reopened.contains("222") && reopened.contains("333"));
}

#[test]
fn disabled_archive_remembers_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut archive = Archive::disabled();
    archive.insert("111").unwrap();
    assert!(!archive.contains("111"));
    assert!(archive.is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
