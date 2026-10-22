#[test]
fn census_does_not_change_help_or_version_output() {
    for option in ["--version", "-V", "--help", "-?"] {
        let invoke = |enabled| {
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_postgres"));
            command.arg(option).env_remove("PGRUST_ALLOC_TRACK");
            if enabled {
                command.env("PGRUST_MCXT_CENSUS", "1");
            } else {
                command.env_remove("PGRUST_MCXT_CENSUS");
            }
            command.output().unwrap()
        };
        let baseline = invoke(false);
        let census = invoke(true);
        assert!(baseline.status.success(), "{option}");
        assert_eq!(census.status.code(), baseline.status.code(), "{option}");
        assert_eq!(census.stdout, baseline.stdout, "{option}");
        assert!(baseline.stderr.is_empty(), "{option}: {:?}", baseline.stderr);
        assert_eq!(census.stderr, baseline.stderr, "{option}");
    }
}
