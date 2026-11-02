use super::*;
use std::os::unix::fs::symlink;

#[test]
fn replacing_pages_directory_after_open_does_not_redirect_write() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("wiki")).unwrap();
    std::fs::create_dir(root.path().join("wiki/pages")).unwrap();
    std::fs::write(outside.path().join("page.md"), "outside").unwrap();
    let pages = open_pages(root.path(), "wiki", false).unwrap();
    std::fs::rename(
        root.path().join("wiki/pages"),
        root.path().join("wiki/old-pages"),
    )
    .unwrap();
    symlink(outside.path(), root.path().join("wiki/pages")).unwrap();
    write_to_dir(&pages, "page", false, |file| file.write_all(b"inside")).unwrap();
    assert_eq!(
        std::fs::read_to_string(outside.path().join("page.md")).unwrap(),
        "outside"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("wiki/old-pages/page.md")).unwrap(),
        "inside"
    );
}

#[test]
fn failed_write_preserves_original_and_cleans_temporary_file() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
    std::fs::write(root.path().join("wiki/pages/page.md"), "original").unwrap();
    let pages = open_pages(root.path(), "wiki", false).unwrap();
    assert!(
        write_to_dir(&pages, "page", false, |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("simulated failure"))
        })
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("wiki/pages/page.md")).unwrap(),
        "original"
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("wiki/pages"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn replacement_preserves_permissions_and_exclusive_create() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
    let target = root.path().join("wiki/pages/page.md");
    std::fs::write(&target, "original").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
    let pages = open_pages(root.path(), "wiki", false).unwrap();
    assert_eq!(
        write_to_dir(&pages, "page", true, |file| file.write_all(b"no"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    write_to_dir(&pages, "page", false, |file| file.write_all(b"updated")).unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "updated");
    assert_eq!(
        std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn read_only_page_is_not_replaced() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
    let target = root.path().join("wiki/pages/page.md");
    std::fs::write(&target, "original").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444)).unwrap();
    let pages = open_pages(root.path(), "wiki", false).unwrap();
    let error = write_to_dir(&pages, "page", false, |file| file.write_all(b"updated")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
}

#[test]
fn replacement_stays_private_while_content_is_written() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
    let target = root.path().join("wiki/pages/page.md");
    std::fs::write(&target, "original").unwrap();
    let original = File::open(&target).unwrap();
    fs::fchmod(&original, Mode::from_raw_mode(0o600)).unwrap();
    let pages = open_pages(root.path(), "wiki", false).unwrap();
    write_to_dir(&pages, "page", false, |file| {
        assert_eq!(fs::fstat(&*file)?.st_mode & 0o077, 0);
        file.write_all(b"private")
    })
    .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "private");
}

#[cfg(target_os = "macos")]
#[test]
fn replacement_does_not_expose_content_through_inherited_acl() {
    use std::process::Command;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("wiki/pages");
    std::fs::create_dir_all(&directory).unwrap();
    let target = directory.join("page.md");
    std::fs::write(&target, "original").unwrap();
    assert!(
        Command::new("chmod")
            .args(["+a", "everyone allow read,file_inherit"])
            .arg(&directory)
            .status()
            .unwrap()
            .success()
    );
    let pages = open_pages(root.path(), "wiki", false).unwrap();
    write_to_dir(&pages, "page", false, |file| {
        let temporary = std::fs::read_dir(&directory)?
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .unwrap()
            .path();
        let acl = Command::new("ls").arg("-le").arg(temporary).output()?;
        assert!(acl.status.success());
        assert_eq!(String::from_utf8_lossy(&acl.stdout).lines().count(), 1);
        file.write_all(b"private")
    })
    .unwrap();
}

#[test]
fn replacement_preserves_owner_and_group() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
    let target = root.path().join("wiki/pages/page.md");
    std::fs::write(&target, "original").unwrap();
    let original = File::open(&target).unwrap();
    // 在有权限切换文件所有者的环境（例如 root 容器）执行此覆盖。
    if fs::fchown(
        &original,
        Some(fs::Uid::from_raw(1234)),
        Some(fs::Gid::from_raw(1235)),
    )
    .is_err()
    {
        return;
    }
    fs::fchmod(&original, Mode::from_raw_mode(0o666)).unwrap();
    let before = fs::fstat(&original).unwrap();
    let pages = open_pages(root.path(), "wiki", false).unwrap();
    write_to_dir(&pages, "page", false, |file| file.write_all(b"updated")).unwrap();
    let after = fs::statat(&pages, "page.md", AtFlags::SYMLINK_NOFOLLOW).unwrap();
    assert_eq!((after.st_uid, after.st_gid), (before.st_uid, before.st_gid));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "updated");
}

#[cfg(target_os = "linux")]
fn example_posix_acl() -> Vec<u8> {
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, permissions, id) in [
        (1u16, 6u16, u32::MAX),
        (2, 4, 65534),
        (4, 4, u32::MAX),
        (16, 4, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend_from_slice(&tag.to_le_bytes());
        acl.extend_from_slice(&permissions.to_le_bytes());
        acl.extend_from_slice(&id.to_le_bytes());
    }
    acl
}

#[cfg(target_os = "linux")]
#[test]
fn replacement_preserves_linux_xattrs_and_posix_acl() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
    let target = root.path().join("wiki/pages/page.md");
    std::fs::write(&target, "original").unwrap();
    let original = File::open(&target).unwrap();
    fs::fsetxattr(
        &original,
        "user.reflect-test",
        b"sentinel",
        fs::XattrFlags::empty(),
    )
    .unwrap();
    // Linux POSIX ACL xattr：版本、owner、指定用户、group、mask、other。
    let acl = example_posix_acl();
    fs::fsetxattr(
        &original,
        "system.posix_acl_access",
        &acl,
        fs::XattrFlags::empty(),
    )
    .unwrap();
    drop(original);
    let pages = open_pages(root.path(), "wiki", false).unwrap();
    write_to_dir(&pages, "page", false, |file| file.write_all(b"updated")).unwrap();
    let updated = File::open(target).unwrap();
    let mut attribute = [0u8; 1024];
    let len = fs::fgetxattr(&updated, "user.reflect-test", &mut attribute[..]).unwrap();
    assert_eq!(&attribute[..len], b"sentinel");
    let len = fs::fgetxattr(&updated, "system.posix_acl_access", &mut attribute[..]).unwrap();
    assert_eq!(&attribute[..len], acl);
}

#[cfg(target_os = "linux")]
#[test]
fn replacement_does_not_inherit_new_default_acl() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("wiki/pages");
    std::fs::create_dir_all(&directory).unwrap();
    let target = directory.join("page.md");
    std::fs::write(&target, "original").unwrap();
    let dir = File::open(&directory).unwrap();
    fs::fsetxattr(
        &dir,
        "system.posix_acl_default",
        &example_posix_acl(),
        fs::XattrFlags::empty(),
    )
    .unwrap();
    let pages = open_pages(root.path(), "wiki", false).unwrap();
    write_to_dir(&pages, "page", false, |file| file.write_all(b"updated")).unwrap();
    let updated = File::open(target).unwrap();
    let mut buffer = [0u8; 1024];
    assert_eq!(
        fs::fgetxattr(&updated, "system.posix_acl_access", &mut buffer[..]).unwrap_err(),
        rustix::io::Errno::NODATA
    );
}

#[cfg(target_os = "macos")]
#[test]
fn replacement_preserves_acl_and_extended_attributes() {
    use std::process::Command;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
    let target = root.path().join("wiki/pages/page.md");
    std::fs::write(&target, "original").unwrap();
    let user = Command::new("id").arg("-un").output().unwrap();
    assert!(user.status.success());
    let user = String::from_utf8(user.stdout).unwrap().trim().to_string();
    assert!(
        Command::new("chmod")
            .arg("+a")
            .arg(format!("{user} allow read"))
            .arg(&target)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("xattr")
            .args(["-w", "com.reflect.test", "sentinel"])
            .arg(&target)
            .status()
            .unwrap()
            .success()
    );
    let acl_before = Command::new("ls").arg("-le").arg(&target).output().unwrap();
    assert!(acl_before.status.success());
    let pages = open_pages(root.path(), "wiki", false).unwrap();
    write_to_dir(&pages, "page", false, |file| file.write_all(b"updated")).unwrap();
    let acl_after = Command::new("ls").arg("-le").arg(&target).output().unwrap();
    assert!(acl_after.status.success());
    let before = String::from_utf8(acl_before.stdout).unwrap();
    let after = String::from_utf8(acl_after.stdout).unwrap();
    assert_eq!(
        before.lines().skip(1).collect::<Vec<_>>(),
        after.lines().skip(1).collect::<Vec<_>>()
    );
    let attribute = Command::new("xattr")
        .args(["-p", "com.reflect.test"])
        .arg(&target)
        .output()
        .unwrap();
    assert!(attribute.status.success());
    assert_eq!(
        String::from_utf8(attribute.stdout).unwrap().trim(),
        "sentinel"
    );
}
