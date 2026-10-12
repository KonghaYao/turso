use crate::common::{limbo_exec_rows, sqlite_exec_rows, try_limbo_exec_rows, TempDatabase};

#[test]
fn official_select3_ungrouped_having() {
    let mut setup = String::from("CREATE TABLE t1 (n int, log int);");
    for number in 1_i64..32 {
        let mut logarithm = 0;
        while (1 << logarithm) < number {
            logarithm += 1;
        }
        setup.push_str(&format!("INSERT INTO t1 VALUES({number},{logarithm});"));
    }
    let database = TempDatabase::new_empty();
    let connection = database.connect_limbo();
    connection.execute(&setup).unwrap();
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    sqlite.execute_batch(&setup).unwrap();
    for query in [
        "SELECT log, count(*) FROM t1 HAVING log>=4",
        "SELECT count(*) FROM t1 HAVING log>=4",
        "SELECT count(*) FROM t1 HAVING log!=400",
    ] {
        let expected = sqlite_exec_rows(&sqlite, query);
        let actual = limbo_exec_rows(&connection, query);
        eprintln!("SQL: {query}\nSQLite: {expected:?}\nTurso: {actual:?}");
        assert_eq!(actual, expected, "{query}");
    }
}

#[test]
fn having_reads_bare_columns_before_end_of_scan() {
    let setup = "CREATE TABLE t(a, b); INSERT INTO t VALUES (4, 10), (1, 30), (9, 20);";
    let database = TempDatabase::new_empty();
    let connection = database.connect_limbo();
    connection.execute(setup).unwrap();
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    sqlite.execute_batch(setup).unwrap();
    for query in [
        "SELECT count(*) FROM t HAVING a = 4",
        "SELECT a, count(*) FROM t HAVING a = 4",
        "SELECT count(*) FROM t HAVING rowid = 1",
        "SELECT a + count(*) FROM t HAVING a = 4",
    ] {
        let expected = sqlite_exec_rows(&sqlite, query);
        let actual = limbo_exec_rows(&connection, query);
        eprintln!("SQL: {query}\nSQLite: {expected:?}\nTurso: {actual:?}");
        assert_eq!(actual, expected, "{query}");
    }
}

#[test]
fn having_reads_min_max_winning_row() {
    assert_queries_match_sqlite(
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (4, 10), (1, 30), (9, 20), (1, 40);",
        &[
            "SELECT min(a) FROM t HAVING b = 30",
            "SELECT max(a) FROM t HAVING b = 20",
            "SELECT b, min(a) FROM t HAVING rowid = 2",
            "SELECT b + min(a) FROM t HAVING b = 30",
            "SELECT b, min(a), count(*) FROM t HAVING b = 30",
            "SELECT b, max(a), count(*) FROM t HAVING b = 20",
            "SELECT count(*) FROM t HAVING min(a) = 1 AND b = 30",
            "SELECT b, min(a) FILTER (WHERE b >= 20) FROM t HAVING b = 30",
            "SELECT b, min(DISTINCT a) FROM t HAVING b = 40",
        ],
    );
}

#[test]
fn having_filtered_extremum_does_not_replace_unfiltered_winner_flag() {
    assert_queries_match_sqlite(
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (2, 'old'), (1, 'winner'); CREATE TABLE ascending(a, b); INSERT INTO ascending VALUES (1, 'old'), (2, 'winner');",
        &[
            "SELECT b, min(a), max(a) FILTER (WHERE 0) FROM t NOT INDEXED HAVING b = 'winner'",
            "SELECT b, max(a) FILTER (WHERE 0), min(a) FROM t NOT INDEXED HAVING b = 'winner'",
            "SELECT b, min(a), max(DISTINCT a) FILTER (WHERE 0) FROM t NOT INDEXED HAVING b = 'winner'",
            "SELECT b, max(a), min(a) FILTER (WHERE 0) FROM ascending NOT INDEXED HAVING b = 'winner'",
            "SELECT b, min(a) FILTER (WHERE 0), max(a) FROM ascending NOT INDEXED HAVING b = 'winner'",
            "SELECT b, min(a) FILTER (WHERE 0), max(a) FILTER (WHERE 0) FROM t NOT INDEXED HAVING b = 'old'",
            "SELECT b, min(a), max(a) FILTER (WHERE 0) FROM t NOT INDEXED WHERE 0 HAVING b IS NULL",
        ],
    );
}

#[test]
fn aggregate_without_bare_columns_does_not_copy_extrema_per_row() {
    let setup = "CREATE TABLE t(a); INSERT INTO t VALUES ('z'), ('a'), ('m');";
    let database = TempDatabase::new_empty();
    let connection = database.connect_limbo();
    connection.execute(setup).unwrap();
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    sqlite.execute_batch(setup).unwrap();
    for query in [
        "SELECT min(a), count(*) FROM t NOT INDEXED",
        "SELECT max(a), count(*) FROM t NOT INDEXED",
        "SELECT length(min(a)) FROM t NOT INDEXED",
        "SELECT min(a) FROM t NOT INDEXED HAVING count(*) > 0",
        "SELECT 42, min(a), count(*) FROM t NOT INDEXED",
        "SELECT typeof(random()), min(a), count(*) FROM t NOT INDEXED",
        "SELECT 42, min(a), count(*) FROM t NOT INDEXED WHERE 0",
    ] {
        let expected = sqlite_exec_rows(&sqlite, query);
        let actual = limbo_exec_rows(&connection, query);
        assert_eq!(actual, expected, "{query}");
        let explain = limbo_exec_rows(&connection, &format!("EXPLAIN {query}"));
        let value_copies = explain
            .iter()
            .filter(|row| {
                matches!(row.get(1), Some(rusqlite::types::Value::Text(opcode)) if opcode == "AggValue")
            })
            .count();
        eprintln!("SQL: {query}\nSQLite: {expected:?}\nTurso: {actual:?}\nAggValue instructions: {value_copies}");
        assert_eq!(value_copies, 0, "{query}");
    }
}

#[test]
fn having_mixed_filters_preserve_null_and_correlated_input_rules() {
    assert_queries_match_sqlite(
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (2, 'old'), (1, 'winner'); CREATE TABLE nulls(a, b); INSERT INTO nulls VALUES (NULL, 'old'), (NULL, 'winner'); CREATE TABLE outer_rows(k); INSERT INTO outer_rows VALUES (3), (1), (2), (4);",
        &[
            "SELECT b, min(a), max(a) FILTER (WHERE 0) FROM nulls NOT INDEXED HAVING b = 'winner'",
            "SELECT b, min(DISTINCT a), max(a) FILTER (WHERE 0) FROM nulls NOT INDEXED HAVING b = 'winner'",
            "SELECT b, min(a) FILTER (WHERE 0), max(a) FILTER (WHERE 0) FROM nulls NOT INDEXED HAVING b = 'old'",
            "SELECT k, (SELECT b || min(a) FROM t NOT INDEXED WHERE a < k HAVING max(a) FILTER (WHERE 0) IS NULL AND b = 'winner') FROM outer_rows ORDER BY k",
            "SELECT k, (SELECT b || min(DISTINCT a) FROM t NOT INDEXED WHERE a < k HAVING max(a) FILTER (WHERE 0) IS NULL AND b = 'winner') FROM outer_rows ORDER BY k",
        ],
    );
}

#[test]
fn having_empty_input_skips_output_evaluation_for_rejected_row() {
    assert_queries_match_sqlite(
        "CREATE TABLE t(a); INSERT INTO t VALUES (1);",
        &[
            "SELECT abs(-9223372036854775808), count(*) FROM t WHERE 0 LIMIT 1 OFFSET 1",
            "SELECT abs(-9223372036854775808), count(*) FROM t WHERE 0 HAVING 0",
            "SELECT abs(-9223372036854775808), count(*) FROM t WHERE a > 10 HAVING a IS NOT NULL",
            "SELECT a, count(*) FROM t WHERE 0 HAVING a IS NULL",
        ],
    );
}

#[test]
fn having_empty_input_and_repeated_correlated_execution() {
    assert_queries_match_sqlite(
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (4, 10), (1, 30), (9, 20); CREATE TABLE outer_rows(k); INSERT INTO outer_rows VALUES (4), (100), (1), (200);",
        &[
            "SELECT a, count(*) FROM t WHERE a = 100 HAVING a IS NULL",
            "SELECT rowid, count(*) FROM t WHERE a = 100 HAVING rowid IS NULL",
            "SELECT count(*) FROM t WHERE a = 100 HAVING a IS NULL",
            "SELECT count(*) FROM t WHERE 0 HAVING a IS NULL",
            "SELECT a + count(*) FROM t WHERE a = 100 HAVING a IS NULL",
            "SELECT 42, count(*) FROM t WHERE a = 100 HAVING a IS NULL",
            "SELECT count(*) FROM t WHERE a = 100 HAVING a = 4",
            "SELECT k, (SELECT count(*) FROM t WHERE a = k HAVING a IS NULL OR a = k) FROM outer_rows ORDER BY k",
            "SELECT k, (SELECT a + count(*) FROM t WHERE a = k HAVING a IS NULL OR a = k) FROM outer_rows ORDER BY k",
            "SELECT k, (SELECT count(*) FROM t WHERE 0 HAVING k > 0) FROM outer_rows ORDER BY k",
            "WITH source AS (SELECT a, b FROM t) SELECT a, count(*) FROM source WHERE a = 100 HAVING a IS NULL",
        ],
    );
}

#[test]
fn having_group_alias_and_subquery_controls() {
    assert_queries_match_sqlite(
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (4, 10), (1, 30), (9, 20), (1, 40);",
        &[
            "SELECT a, count(*) FROM t GROUP BY a HAVING a > 1 ORDER BY a",
            "SELECT a AS chosen, count(*) AS amount FROM t HAVING chosen = 4 AND amount = 4",
            "SELECT min(a) AS lowest FROM t HAVING lowest = 1 AND b = 30",
            "SELECT count(*) FROM t HAVING (SELECT 4) = 4",
            "SELECT count(*) FROM t HAVING EXISTS (SELECT 1 WHERE 4 = 4)",
            "SELECT a, count(*) FROM t HAVING a = 4 LIMIT 1 OFFSET 1",
            "SELECT a, count(*) FROM t HAVING a = 4 UNION ALL SELECT a, count(*) FROM t WHERE 0 HAVING a IS NULL",
        ],
    );
}

#[test]
fn having_min_max_nulls_ties_collation_and_filter() {
    assert_queries_match_sqlite(
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (NULL, 10), (NULL, 20), (2, 30), (2, 40), (NULL, 50); CREATE TABLE words(a COLLATE NOCASE, b); INSERT INTO words VALUES ('z', 10), ('B', 20), ('b', 30), ('a', 40);",
        &[
            "SELECT b, min(a) FROM t HAVING b = 30",
            "SELECT b, max(a) FROM t HAVING b = 30",
            "SELECT b, min(a) FROM t WHERE a IS NULL HAVING b = 50",
            "SELECT b, max(a) FROM t WHERE a IS NULL HAVING rowid = 5",
            "SELECT b, min(a) FROM t WHERE 0 HAVING b IS NULL",
            "SELECT b, min(a) FILTER (WHERE 0) FROM t HAVING b = 10",
            "SELECT b, min(a) FILTER (WHERE b < 40) FROM t HAVING b = 30",
            "SELECT b, min(DISTINCT a) FROM t HAVING b = 40",
            "SELECT b, min(a) FROM words HAVING b = 40",
            "SELECT b, max(a) FROM words HAVING b = 10",
            "SELECT b, min(a) FROM words WHERE b <= 30 HAVING b = 20",
            "SELECT b, min(a COLLATE NOCASE) FROM words WHERE b <= 30 HAVING b = 20",
            "SELECT b, min(a), max(b) FROM t HAVING b = 50",
            "SELECT b, max(b), min(a) FROM t HAVING b = 30",
        ],
    );
}

#[test]
fn having_distinct_min_max_preserves_winner_flag_on_duplicate() {
    assert_queries_match_sqlite(
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (4, 10), (1, 30), (1, 40);",
        &[
            "SELECT b, min(DISTINCT a) FROM t HAVING b = 40",
            "SELECT b, max(DISTINCT a) FROM t HAVING b = 10",
            "SELECT b, min(DISTINCT a) FILTER (WHERE b > 10) FROM t HAVING b = 30",
        ],
    );
}

#[test]
fn having_index_join_and_result_column_controls() {
    assert_queries_match_sqlite(
        "CREATE TABLE t(a, b); INSERT INTO t VALUES (4, 10), (1, 30), (9, 20); CREATE INDEX idx_b ON t(b); CREATE TABLE other(c); INSERT INTO other VALUES (1), (2); CREATE TABLE empty(a);",
        &[
            "SELECT count(*) FROM empty HAVING a IS NULL",
            "SELECT rowid, count(*) FROM empty HAVING rowid IS NULL",
            "SELECT count(*) FROM t INDEXED BY idx_b HAVING b = 10",
            "SELECT count(*) FROM t INDEXED BY idx_b HAVING rowid = 1",
            "SELECT b, min(a) FROM t INDEXED BY idx_b HAVING b = 30",
            "SELECT t.a, count(*) FROM t CROSS JOIN other HAVING t.a = 4 AND other.c = 1",
            "SELECT t.a, count(*) FROM t JOIN other ON other.c = t.a HAVING t.a = 1",
            "SELECT count(*) + a FROM t",
            "SELECT b, min(a) FROM t",
            "SELECT a, count(*) FROM t",
            "SELECT count(*) FROM t HAVING count(*) = 3",
        ],
    );
}

#[test]
fn having_correlated_subquery_retains_existing_unsupported_error() {
    let setup = "CREATE TABLE t(a); INSERT INTO t VALUES (4), (1), (9);";
    let database = TempDatabase::new_empty();
    let connection = database.connect_limbo();
    connection.execute(setup).unwrap();
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    sqlite.execute_batch(setup).unwrap();
    let query = "SELECT count(*) FROM t HAVING (SELECT a) = 4";
    let expected = sqlite_exec_rows(&sqlite, query);
    assert_eq!(expected, vec![vec![rusqlite::types::Value::Integer(3)]]);
    let error = try_limbo_exec_rows(&database, &connection, query).unwrap_err();
    eprintln!("Existing unsupported SQL: {query}\nSQLite: {expected:?}\nTurso: {error}");
    assert!(matches!(
        error,
        turso_core::LimboError::ParseError(message)
            if message == "correlated subqueries in HAVING clause are not supported yet"
    ));
}

fn assert_queries_match_sqlite(setup: &str, queries: &[&str]) {
    let database = TempDatabase::new_empty();
    let connection = database.connect_limbo();
    connection.execute(setup).unwrap();
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    sqlite.execute_batch(setup).unwrap();
    for query in queries {
        let expected = sqlite_exec_rows(&sqlite, query);
        let actual = limbo_exec_rows(&connection, query);
        eprintln!("SQL: {query}\nSQLite: {expected:?}\nTurso: {actual:?}");
        assert_eq!(actual, expected, "{query}");
    }
}
