//! The iteration protocol against real children spawned through the choke
//! point (`process::spawn::spawn`): borrow, conflict, crash mid-borrow and
//! crash after a child's exit, and the spool.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::LaunchSpec;
use super::iteration::{Call, Iteration};
use super::store::{Access, Scan};

const SH: &str = "/bin/sh";

/// A scan of the process's test store, inside the critical section.
fn scan() -> Scan {
    let _serial = super::serialize().unwrap();
    super::store().unwrap().scan()
}

/// The reservation whose resources include `dir`, if the scan kept one.
fn kept(scan: &Scan, dir: &Path) -> bool {
    scan.reservations
        .iter()
        .any(|r| r.resources.iter().any(|res| res.lexical == dir))
}

/// Run `script` under `sh` as the call's child, stdout into its spool.
fn spawn(call: &Call, script: &str, args: &[&Path]) -> Child {
    let mut cmd = Command::new(SH);
    cmd.arg("-c").arg(script).arg("sh").args(args);
    cmd.stdout(Stdio::from(call.stdout().unwrap()));
    crate::process::spawn::spawn(cmd, None).unwrap()
}

fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let root = crate::test_scratch::scratch("reservations", name);
    let input = root.join("in.txt");
    std::fs::write(&input, b"input\n").unwrap();
    (root, input)
}

/// The whole cycle: the child holds only its borrow, writes its output and
/// stdout, and is refused nothing; a third party is refused the output while
/// it runs; the finished call hands back its spool; the owner's release
/// removes the iteration.
#[test]
fn a_call_borrows_from_its_iteration_and_the_owner_release_reclaims_it() {
    let _lock = crate::test_scratch::process_global_lock();
    let (root, input) = fixture("iteration-cycle");
    let mut iteration = Iteration::admit(&root, std::slice::from_ref(&input)).unwrap();
    let dir = iteration.dir().to_path_buf();
    let out = iteration.output("copy.txt").unwrap();
    let scratch = iteration.scratch("tmp").unwrap();
    assert!(scratch.is_dir() && scratch.starts_with(&dir));
    let call = iteration
        .call(
            Path::new(SH),
            &[
                (input.clone(), Access::Read),
                (out.clone(), Access::Write),
                (scratch.clone(), Access::Write),
            ],
        )
        .unwrap();
    let mut child = spawn(
        &call,
        "sleep 0.5; cat \"$1\" > \"$2\"; echo done",
        &[&input, &out],
    );
    let rival = LaunchSpec::new(root.clone(), Vec::new(), vec![out.clone()], None);
    assert!(
        rival
            .launch(Path::new("/bin/true"), &[], &root, &[])
            .is_err(),
        "the iteration's output is reserved while the call runs"
    );
    assert!(child.wait().unwrap().success());
    let spool = call.finish().unwrap();
    assert_eq!(spool.preview(64).unwrap().bytes, b"done\n");
    assert_eq!(std::fs::read(&out).unwrap(), b"input\n");
    iteration.release().unwrap();
    assert!(!dir.exists(), "the owner's release removes the iteration");
    assert!(input.exists(), "an input is never removed");
}

/// Admission refuses a third party writing into the iteration, a call writing
/// an input or an earlier spool, a call reaching outside what the iteration
/// holds, and a call conflicting with a sibling borrow still held.
#[test]
fn conflicting_declarations_are_refused_at_the_call() {
    let _lock = crate::test_scratch::process_global_lock();
    let (root, input) = fixture("iteration-conflict");
    let outside = root.join("outside.txt");
    std::fs::write(&outside, b"").unwrap();
    let mut iteration = Iteration::admit(&root, std::slice::from_ref(&input)).unwrap();
    let dir = iteration.dir().to_path_buf();
    let out = iteration.output("o").unwrap();

    let third = LaunchSpec::new(root.clone(), Vec::new(), vec![dir.join("x")], None);
    assert!(
        third
            .launch(Path::new("/bin/true"), &[], &root, &[])
            .is_err()
    );

    let err = iteration
        .call(Path::new(SH), &[(input.clone(), Access::Write)])
        .unwrap_err();
    assert!(err.to_string().contains("read-only for the run"), "{err}");
    let err = iteration
        .call(Path::new(SH), &[(outside.clone(), Access::Read)])
        .unwrap_err();
    assert!(err.to_string().contains("not within"), "{err}");

    let first = iteration
        .call(Path::new(SH), &[(out.clone(), Access::Write)])
        .unwrap();
    let err = iteration
        .call(Path::new(SH), &[(out.clone(), Access::Read)])
        .unwrap_err();
    assert!(
        err.to_string().contains("overlaps"),
        "a sibling borrow still held: {err}"
    );
    let spool = first.finish().unwrap();
    let err = iteration
        .call(
            Path::new(SH),
            &[(spool.path().to_path_buf(), Access::Write)],
        )
        .unwrap_err();
    assert!(err.to_string().contains("read-only for the run"), "{err}");
    iteration
        .call(
            Path::new(SH),
            &[
                (out.clone(), Access::Read),
                (spool.path().to_path_buf(), Access::Read),
            ],
        )
        .unwrap()
        .finish()
        .unwrap();
    iteration.release().unwrap();
}

/// brokkr's own scratch creation is a write admitted like a call's: a
/// sibling borrow still reserving `scratch/` refuses it.
#[test]
fn scratch_creation_respects_a_sibling_borrow() {
    let _lock = crate::test_scratch::process_global_lock();
    let (root, input) = fixture("iteration-scratch-sibling");
    let mut iteration = Iteration::admit(&root, std::slice::from_ref(&input)).unwrap();
    let area = iteration.dir().join("scratch");
    let reader = iteration
        .call(Path::new(SH), &[(area.clone(), Access::Read)])
        .unwrap();
    let err = iteration.scratch("tmp").unwrap_err();
    assert!(err.to_string().contains("overlaps"), "{err}");
    assert!(!area.join("tmp").exists());
    reader.finish().unwrap();
    iteration.scratch("tmp2").unwrap();
    iteration.release().unwrap();
}

/// The veto resolves an input as the caller spelled it, at the call: a link
/// in that spelling retargeted since admission still vetoes a write to
/// where the input now resolves.
#[test]
fn the_veto_follows_an_input_spelling_retargeted_since_admission() {
    let _lock = crate::test_scratch::process_global_lock();
    let root = crate::test_scratch::scratch("reservations", "iteration-retarget");
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join("a/in.txt"), b"x").unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(root.join("a/b"), &link).unwrap();
    let input = link.join("..").join("in.txt");
    let mut iteration = Iteration::admit(&root, std::slice::from_ref(&input)).unwrap();
    let out = iteration.dir().join("out");
    std::fs::create_dir(out.join("sub")).unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(out.join("sub"), &link).unwrap();
    let err = iteration
        .call(Path::new(SH), &[(out.join("in.txt"), Access::Write)])
        .unwrap_err();
    assert!(err.to_string().contains("read-only for the run"), "{err}");
    iteration.release().unwrap();
}

/// Crash mid-borrow: brokkr's own descriptors on the owner and the borrow
/// close unreleased while the child still runs. Every scan keeps the
/// iteration until the child is gone; the next one reclaims it.
#[test]
fn a_crash_mid_borrow_keeps_the_iteration_until_the_child_is_gone() {
    let _lock = crate::test_scratch::process_global_lock();
    let (root, input) = fixture("iteration-crash-mid");
    let mut iteration = Iteration::admit(&root, std::slice::from_ref(&input)).unwrap();
    let dir = iteration.dir().to_path_buf();
    let call = iteration.call(Path::new(SH), &[]).unwrap();
    let mut child = spawn(&call, "exec sleep 30", &[]);
    call.abandon_for_test();
    iteration.abandon_for_test();

    let found = scan();
    assert!(kept(&found, &dir), "{found:?}");
    assert!(
        dir.exists(),
        "the borrow the child carries pins the iteration"
    );
    assert!(dir.exists() && kept(&scan(), &dir), "and keeps pinning it");

    child.kill().unwrap();
    child.wait().unwrap();
    let found = scan();
    assert!(!kept(&found, &dir), "{found:?}");
    assert!(!dir.exists(), "reclaimed once the child is gone");
}

/// Crash after the child's exit: nothing was released and nothing holds
/// either lease, so the next scan reclaims the iteration.
#[test]
fn a_crash_after_the_child_exited_is_reclaimed_by_the_next_scan() {
    let _lock = crate::test_scratch::process_global_lock();
    let (root, input) = fixture("iteration-crash-after");
    let mut iteration = Iteration::admit(&root, std::slice::from_ref(&input)).unwrap();
    let dir = iteration.dir().to_path_buf();
    let call = iteration.call(Path::new(SH), &[]).unwrap();
    let mut child = spawn(&call, "echo gone", &[]);
    assert!(child.wait().unwrap().success());
    call.abandon_for_test();
    iteration.abandon_for_test();
    let found = scan();
    assert!(!kept(&found, &dir), "{found:?}");
    assert!(!dir.exists());
}

/// The owner's release while a descendant still holds a borrow keeps the
/// iteration, and is the error; the scan after the holder is gone reclaims.
#[test]
fn an_owner_release_under_a_surviving_descendant_keeps_the_iteration() {
    let _lock = crate::test_scratch::process_global_lock();
    let (root, input) = fixture("iteration-survivor");
    let mut iteration = Iteration::admit(&root, std::slice::from_ref(&input)).unwrap();
    let dir = iteration.dir().to_path_buf();
    let call = iteration.call(Path::new(SH), &[]).unwrap();
    let mut child = spawn(&call, "exec sleep 30", &[]);
    let err = call.finish().unwrap_err();
    assert!(err.to_string().contains("still held"), "{err}");
    let err = iteration.release().unwrap_err();
    assert!(err.to_string().contains("borrow"), "{err}");
    assert!(dir.exists());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!kept(&scan(), &dir));
    assert!(!dir.exists());
}

/// A child that displaced `stdout/` or `scratch/`, by a plain directory or a
/// link, makes the next creation there fail, and nothing is made where the
/// link leads.
#[test]
fn a_displaced_area_fails_the_next_creation() {
    let _lock = crate::test_scratch::process_global_lock();
    let (root, input) = fixture("iteration-displaced");
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let mut iteration = Iteration::admit(&root, std::slice::from_ref(&input)).unwrap();
    let dir = iteration.dir().to_path_buf();

    std::fs::rename(dir.join("stdout"), dir.join("moved")).unwrap();
    std::fs::create_dir(dir.join("stdout")).unwrap();
    let err = iteration.call(Path::new(SH), &[]).unwrap_err();
    assert!(err.to_string().contains("replaced"), "{err}");
    std::fs::remove_dir(dir.join("stdout")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, dir.join("stdout")).unwrap();
    assert!(iteration.call(Path::new(SH), &[]).is_err());

    std::fs::rename(dir.join("scratch"), dir.join("moved-scratch")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, dir.join("scratch")).unwrap();
    assert!(iteration.scratch("tmp").is_err());
    assert_eq!(
        std::fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "nothing made through a link"
    );
    iteration.release().unwrap();
    assert!(!dir.exists());
    assert!(elsewhere.exists(), "a link's target is not the iteration's");
}

/// A spool keeps all of a child's stdout; the preview is bounded and says
/// whether there was more.
#[test]
fn a_spool_keeps_everything_and_previews_a_bounded_start() {
    let _lock = crate::test_scratch::process_global_lock();
    let (root, input) = fixture("iteration-spool");
    let mut iteration = Iteration::admit(&root, std::slice::from_ref(&input)).unwrap();
    let call = iteration.call(Path::new(SH), &[]).unwrap();
    let mut child = spawn(
        &call,
        "i=0; while [ $i -lt 100 ]; do printf x; i=$((i+1)); done",
        &[],
    );
    assert!(child.wait().unwrap().success());
    let spool = call.finish().unwrap();
    let short = spool.preview(10).unwrap();
    assert_eq!((short.bytes.len(), short.truncated), (10, true));
    let whole = spool.preview(1000).unwrap();
    assert_eq!((whole.bytes.len(), whole.truncated), (100, false));
    assert_eq!(std::fs::metadata(spool.path()).unwrap().len(), 100);
    iteration.release().unwrap();
}

#[test]
fn output_and_scratch_names_are_single_components_given_once() {
    let _lock = crate::test_scratch::process_global_lock();
    let (root, input) = fixture("iteration-names");
    let mut iteration = Iteration::admit(&root, std::slice::from_ref(&input)).unwrap();
    for bad in ["", ".", "..", "a/b"] {
        assert!(iteration.output(bad).is_err(), "{bad:?}");
        assert!(iteration.scratch(bad).is_err(), "{bad:?}");
    }
    iteration.output("x").unwrap();
    assert!(iteration.output("x").is_err());
    iteration.scratch("x").unwrap();
    assert!(iteration.scratch("x").is_err());
    iteration.release().unwrap();
}
