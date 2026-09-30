use std::process::Command;

const CASES: &[(&str, &[&str], &str)] = &[
    ("root", &["--help"], include_str!("fixtures/help/root.txt")),
    (
        "serve",
        &["serve", "--help"],
        include_str!("fixtures/help/serve.txt"),
    ),
    (
        "get",
        &["get", "--help"],
        include_str!("fixtures/help/get.txt"),
    ),
    (
        "put",
        &["put", "--help"],
        include_str!("fixtures/help/put.txt"),
    ),
    (
        "open",
        &["open", "--help"],
        include_str!("fixtures/help/open.txt"),
    ),
    (
        "exec",
        &["exec", "--help"],
        include_str!("fixtures/help/exec.txt"),
    ),
    (
        "register",
        &["register", "--help"],
        include_str!("fixtures/help/register.txt"),
    ),
    (
        "approve",
        &["approve", "--help"],
        include_str!("fixtures/help/approve.txt"),
    ),
];

#[test]
fn ac_t0_2_1_cli_help_matches_pre_split_fixtures() {
    for (name, args, expected) in CASES {
        let output = Command::new(env!("CARGO_BIN_EXE_clipwire"))
            .args(*args)
            .output()
            .unwrap_or_else(|error| panic!("failed to run {name} help: {error}"));
        assert!(output.status.success(), "{name} help failed");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            *expected,
            "{name}"
        );
    }
}
