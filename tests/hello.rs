use std::process::Command;

#[test]
fn prints_hello_world() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentrun"))
        .output()
        .expect("agentrun runs");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[agentrun] hello world\n"
    );
}
