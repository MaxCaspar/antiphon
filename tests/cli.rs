use antiphon::cli::Cli;
use assert_cmd::Command;
use clap::Parser;
use predicates::prelude::*;

#[test]
fn version_works() {
    let mut cmd = Command::cargo_bin("antiphon").expect("binary exists");
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn invalid_flag_fails() {
    let mut cmd = Command::cargo_bin("antiphon").expect("binary exists");
    cmd.arg("--definitely-not-a-flag")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn debug_flag_still_parses() {
    let cli = Cli::parse_from(["antiphon", "--debug"]);
    assert!(cli.debug);
}
