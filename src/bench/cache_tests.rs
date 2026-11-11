#![allow(clippy::unwrap_used)]

use super::*;

fn target(path: &std::path::Path, mode: CacheMode) -> Target {
    Target {
        paths: vec![path.display().to_string()],
        mode,
        file: File::open(path).unwrap(),
    }
}

#[test]
fn warm_reads_a_file_whole_and_cold_advises_it_out() {
    let dir = crate::test_scratch::scratch("bench-cache", "modes");
    let file = dir.join("input");
    std::fs::write(&file, vec![7u8; 3 * 4096 + 17]).unwrap();
    let never = || false;
    let (files, from, to) = prepare(&[target(&file, CacheMode::Warm)], &never).unwrap();
    assert!(from <= to);
    assert_eq!(files[0].bytes, 3 * 4096 + 17);
    assert_eq!(files[0].resident, files[0].bytes);
    match prepare(&[target(&file, CacheMode::Cold)], &never) {
        Ok((files, _, _)) => assert_eq!(files[0].resident, 0),
        // A filesystem that keeps file pages in memory cannot be made cold;
        // the refusal says what stayed.
        Err(Failure::Failed(why)) => assert!(why.contains("stayed resident"), "{why}"),
        Err(Failure::Interrupted) => panic!("interrupted"),
    }
}

#[test]
fn an_empty_file_is_both_cold_and_warm() {
    let dir = crate::test_scratch::scratch("bench-cache", "empty");
    let file = dir.join("empty");
    std::fs::write(&file, b"").unwrap();
    let never = || false;
    for mode in [CacheMode::Cold, CacheMode::Warm] {
        let (files, _, _) = prepare(&[target(&file, mode)], &never).unwrap();
        assert_eq!((files[0].bytes, files[0].resident), (0, 0));
        prerequisites(&File::open(&file).unwrap(), mode).unwrap();
    }
}

#[test]
fn a_stop_ends_a_preparation_and_a_failure_accounts_for_every_file() {
    let dir = crate::test_scratch::scratch("bench-cache", "stop");
    let file = dir.join("input");
    std::fs::write(&file, vec![1u8; 4096]).unwrap();
    let stopped = || true;
    assert_eq!(
        prepare(&[target(&file, CacheMode::Warm)], &stopped).err(),
        Some(Failure::Interrupted)
    );
    let mut readonly = std::fs::metadata(&file).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut readonly, 0o444);
    std::fs::set_permissions(&file, readonly).unwrap();
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return; // Root may write a read-only file: nothing to refuse.
    }
    let never = || false;
    let other = dir.join("other");
    std::fs::write(&other, b"x").unwrap();
    match prepare(
        &[
            target(&file, CacheMode::Warm),
            target(&other, CacheMode::Warm),
        ],
        &never,
    ) {
        Err(Failure::Failed(why)) => {
            assert!(why.contains("cannot establish"), "{why}");
            assert!(why.contains("not attempted"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    let refused = prerequisites(&File::open(&file).unwrap(), CacheMode::Warm).unwrap_err();
    assert!(refused.contains("cannot establish"), "{refused}");
}
