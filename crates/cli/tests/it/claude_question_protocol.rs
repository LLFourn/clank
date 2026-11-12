//! Real interactive Claude hook arbitration, with a PTY and localhost model.
//! No user credentials, external inference, installed hooks or agent sessions.
//! CLANK_CLAUDE_PROTOCOL_BINARY=/path/to/claude cargo test -p clank --test it \
//!   claude_question_protocol:: -- --ignored

#[test]
#[ignore = "requires an explicitly selected installed Claude and local PTY/socket access"]
fn interactive_permission_question_hooks_arbitrate_with_the_local_terminal() {
    let binary = std::env::var_os("CLANK_CLAUDE_PROTOCOL_BINARY")
        .expect("select the installed Claude using CLANK_CLAUDE_PROTOCOL_BINARY");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_clank-test-fixture"))
        .arg("claude-question")
        .arg(binary)
        .output()
        .expect("run the isolated interactive Claude fixture");
    assert!(
        result.status.success(),
        "Claude question proof failed:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8_lossy(&result.stdout);
    for scenario in ["remote", "local", "cancel"] {
        assert!(
            output.contains(&format!("PASS: interactive Claude {scenario};")),
            "missing native {scenario} acceptance: {output}"
        );
    }
}
