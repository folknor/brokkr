#![allow(clippy::unwrap_used)]

use super::*;
use crate::pbfhogg::binary::tests::{FAKE_PBFHOGG, execute, install, typed_ctx};

#[test]
fn materialize_runs_the_handles_resolved_argv_not_the_runners_binary() {
    // The runner hands materialize its own executable; the pbfhogg plan must
    // ignore it and resolve the planned argv through the handle planning
    // built, and the result must survive a reused output against a refusing
    // binary.
    let (root, path) = install("materialize", FAKE_PBFHOGG);
    let ctx = typed_ctx(&root);
    let command = PbfhoggCommand::Sort;
    let plan = CommandPlan {
        argv: command.build_args(&ctx, ArgMode::Bench).unwrap(),
        scratch_dirs: vec![],
        subject: RowSubject {
            command: command.id().into(),
            input_file: None,
            input_mb: None,
            cli_args: None,
            metadata: vec![],
            arm_qualifier: None,
        },
        state: PbfhoggState {
            ctx,
            handle: Some(PbfhoggBinary::fixture(&path, &root).unwrap()),
        },
    };
    let params = CommandParams::default();
    let planned = PbfhoggPlan {
        command: &command,
        osc_seq: None,
        params: &params,
    };
    let decoy = Path::new("not-the-binary");
    for _ in 0..2 {
        let invocation = planned
            .materialize(&plan, Executable::for_test(decoy, &Project::Pbfhogg))
            .unwrap();
        assert_eq!(invocation.program_for_test(), path.as_path());
        assert!(
            invocation
                .cli_args()
                .contains(&format!(" {OVERWRITE_FLAG}"))
        );
        assert_eq!(execute(&invocation, &root), Some(0));
    }
}

#[test]
fn a_pbfhogg_executable_cannot_become_an_invocation_without_its_handle() {
    // Whatever a pbfhogg materialize is written as, pairing the runner's
    // executable with argv directly is refused; only the handle resolves it.
    let path = Path::new("pbfhogg");
    let argv = vec!["sort".to_owned()];
    assert!(
        Invocation::built(Executable::for_test(path, &Project::Pbfhogg), argv.clone()).is_err()
    );
    assert!(Invocation::built(Executable::for_test(path, &Project::Elivagar), argv).is_ok());
}
