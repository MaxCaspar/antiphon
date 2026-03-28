use assert_cmd::Command;
use insta::assert_snapshot;

#[test]
fn help_snapshot() {
    let mut cmd = Command::cargo_bin("antiphon").expect("binary exists");
    let output = cmd
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8 stdout");
    let lines: Vec<&str> = stdout.lines().collect();
    let stable = lines
        .iter()
        .filter(|l| {
            l.starts_with("Two AI agents")
                || l.starts_with("Usage:")
                || l.contains("--agent-a")
                || l.contains("--agent-b")
                || l.contains("--turns")
                || l.contains("--debug")
                || l.contains("--output")
                || l.contains("--quiet")
                || l.contains("--version")
        })
        .map(|l| {
            l.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert_snapshot!(stable, @r###"
Two AI agents in call-and-response dialogue
Usage: antiphon [OPTIONS] [-- <INITIAL_PROMPT>]
--agent-a <AGENT_A> [default: claude]
--agent-b <AGENT_B> [default: claude]
--turns <TURNS> [default: 10]
--debug
--output <OUTPUT> [default: text] [possible values: text, json]
--quiet
-V, --version Print version
"###);
}

#[test]
fn json_error_snapshot() {
    let mut cmd = Command::cargo_bin("antiphon").expect("binary exists");
    let output = cmd
        .args(["--output", "json", "--turns", "0", "--", "hello"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("utf8 stdout");
    assert_snapshot!(stdout, @r###"
{
  "error": {
    "code": "invalid_input",
    "message": "invalid input: --turns must be >= 1"
  }
}
"###);
}
