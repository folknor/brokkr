#![allow(clippy::unwrap_used)]

use super::*;
use crate::pbfhogg::binary::tests::{FAKE_PBFHOGG, execute, install, typed_ctx};

#[test]
fn materialize_runs_the_handles_resolved_argv_not_the_runners_binary() {
    // The runner hands materialize its own path; the pbfhogg plan must
    // ignore it and resolve through the handle that planning built, and the
    // result must survive a reused output against a refusing binary.
    let (root, path) = install("materialize", FAKE_PBFHOGG);
    let ctx = typed_ctx(&root);
    let command = PbfhoggCommand::Sort;
    let argv = command.build_args(&ctx, ArgMode::Bench).unwrap();
    let plan = CommandPlan {
        args: argv.args().to_vec(),
        scratch_dirs: vec![],
        subject: RowSubject {
            command: command.id().into(),
            input_file: None,
            input_mb: None,
            cli_args: None,
            metadata: vec![],
        },
        state: PbfhoggState {
            ctx,
            argv,
            binary: Some(PbfhoggBinary::new(&path, &root).unwrap()),
        },
    };
    let params = CommandParams::default();
    let planned = PbfhoggPlan {
        command: &command,
        osc_seq: None,
        params: &params,
    };
    for _ in 0..2 {
        let invocation = planned
            .materialize(&plan, Path::new("not-the-binary"))
            .unwrap();
        assert_eq!(invocation.program(), path.as_path());
        assert!(invocation.cli_args().ends_with(OVERWRITE_FLAG));
        assert_eq!(execute(&invocation, &root), Some(0));
    }
}
