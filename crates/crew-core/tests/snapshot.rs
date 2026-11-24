mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::{git, repo};
use crew_core::snapshot::{Caps, Copy, copy_path, id_of, take};

#[test]
fn a_snapshot_keeps_copies_within_its_caps() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("api");
    repo(&root);
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    git(&root, &["add", ".gitignore"]);
    git(&root, &["commit", "-q", "-m", "ignore"]);
    // Tracked edits, one executable and one over the file cap.
    fs::write(root.join("a.txt"), "mine\n").unwrap();
    fs::set_permissions(root.join("a.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("b.txt"), "x".repeat(300)).unwrap();
    // Untracked: a small note, one over the untracked cap, an ignored log.
    fs::write(root.join("note.md"), "hi\n").unwrap();
    fs::write(root.join("big.bin"), "y".repeat(200)).unwrap();
    fs::write(root.join("app.log"), "log\n").unwrap();
    let store = tmp.path().join("blobs");
    let caps = Caps {
        file: 100,
        untracked: 100,
        total: 1000,
    };
    let snap = take(tmp.path(), &["api".to_string()], &store, caps);
    let dirty = &snap.repos[0].dirty;
    let find = |p: &str| dirty.iter().find(|d| d.path == p).unwrap();
    assert!(dirty.iter().all(|d| d.path != "app.log"));
    let a = find("a.txt");
    assert_eq!(a.copy, Copy::Kept);
    assert_eq!(a.mode, Some(0o755));
    // The copy is named by its own content.
    let copy = copy_path(&store, a.blob.as_deref().unwrap()).unwrap();
    assert_eq!(fs::read_to_string(&copy).unwrap(), "mine\n");
    assert_eq!(&id_of(&root, &copy).unwrap(), a.blob.as_ref().unwrap());
    assert_eq!(
        fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&store).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(find("note.md").copy, Copy::Kept);
    // Over a cap: the id only.
    let b = find("b.txt");
    assert_eq!(b.copy, Copy::TooBig);
    assert!(b.blob.is_some());
    assert_eq!(find("big.bin").copy, Copy::TooBig);
    assert_eq!(snap.skipped(), 2);
    assert_eq!(snap.copies().count(), 2);

    // The run total caps what is copied.
    let small = Caps {
        file: 100,
        untracked: 100,
        total: 6,
    };
    let snap = take(tmp.path(), &["api".to_string()], &store, small);
    let kept: Vec<&str> = snap.repos[0]
        .dirty
        .iter()
        .filter(|d| d.copy == Copy::Kept)
        .map(|d| d.path.as_str())
        .collect();
    assert_eq!(kept, ["a.txt"]);
}
