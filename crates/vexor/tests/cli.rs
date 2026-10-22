use assert_cmd::Command;

fn vexor() -> Command {
    Command::cargo_bin("vexor").expect("vexor binary is built")
}

#[test]
fn version_prints_crate_version() {
    let out = vexor().arg("--version").assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert_eq!(
        stdout.trim(),
        format!("vexor {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_argument_is_a_usage_error() {
    // Exit code 2 = usage error (docs/compatibility.md §4).
    vexor().arg("--no-such-flag").assert().code(2);
}
