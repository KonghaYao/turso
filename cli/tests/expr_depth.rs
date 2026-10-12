use std::io::Write;
use std::process::{Command, Stdio};
use turso_parser::MAX_EXPR_DEPTH;

#[test]
fn expression_depth_boundaries_in_sql_argument() {
    check_depth_boundaries(false);
}

#[test]
fn expression_depth_boundaries_in_piped_input() {
    check_depth_boundaries(true);
}

fn check_depth_boundaries(piped: bool) {
    for depth in [MAX_EXPR_DEPTH - 1, MAX_EXPR_DEPTH] {
        for (sql, expected) in expressions(depth) {
            let mut command = Command::new(env!("CARGO_BIN_EXE_tursodb"));
            command.args(["-q", "-m", "list", ":memory:"]);
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            if piped {
                command.stdin(Stdio::piped());
            } else {
                command.arg(&sql).stdin(Stdio::null());
            }
            let mut child = command.spawn().expect("failed to start CLI");
            if piped {
                child
                    .stdin
                    .take()
                    .expect("missing piped stdin")
                    .write_all(sql.as_bytes())
                    .expect("failed to write SQL");
            }
            let output = child.wait_with_output().expect("failed to wait for CLI");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if depth < MAX_EXPR_DEPTH {
                assert_eq!(output.status.code(), Some(0), "{sql}: {stderr}");
                assert_eq!(stdout.trim(), expected, "{sql}: {stderr}");
            } else {
                assert_eq!(output.status.code(), Some(1), "{sql}: {stdout} {stderr}");
                let error = format!(
                    "Parse error: Expression tree is too large (maximum depth {MAX_EXPR_DEPTH})"
                );
                assert!(
                    format!("{stdout}{stderr}").contains(&error),
                    "{sql}: {stdout} {stderr}"
                );
            }
        }
    }
}

fn expressions(depth: usize) -> Vec<(String, String)> {
    let mut expressions = Vec::new();
    for operator in ["OR", "AND", "+"] {
        let expected = if operator == "+" { depth + 1 } else { 1 };
        expressions.push((
            format!("SELECT 1{};", format!(" {operator} 1").repeat(depth)),
            expected.to_string(),
        ));
    }
    for (prefix, suffix) in [
        ("(", ")"),
        ("CASE WHEN 1 THEN ", " ELSE 0 END"),
        ("(SELECT ", ")"),
        ("abs(", ")"),
    ] {
        expressions.push((
            format!("SELECT {}1{};", prefix.repeat(depth), suffix.repeat(depth)),
            "1".into(),
        ));
    }
    for prefix in [
        "SELECT (",
        "SELECT count(*) FILTER (WHERE ",
        "SELECT count(1) FILTER (WHERE ",
        "SELECT sum(1) OVER (ORDER BY ",
        "SELECT count(*) OVER (PARTITION BY ",
    ] {
        expressions.push((
            format!("{prefix}1{});", " OR 1".repeat(depth - 1)),
            "1".into(),
        ));
    }
    expressions
}
