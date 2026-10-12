use std::sync::Arc;
use turso_core::{Connection, Database, MemoryIO, SqliteDialect, StepResult};

fn connection() -> Arc<Connection> {
    let database = Database::open_file(
        Arc::new(MemoryIO::new()),
        ":memory:",
        Arc::new(SqliteDialect),
    )
    .unwrap();
    let connection = database.connect().unwrap();
    connection.execute("CREATE TABLE t5(a,b); CREATE TABLE t6(c,d); INSERT INTO t5 VALUES(1,2),(3,4),(5,6); INSERT INTO t6 VALUES('a','b'),('c','d'); CREATE INDEX i5 ON t5(a); CREATE INDEX i6 ON t6(c);").unwrap();
    connection
}

fn callback_rows(delete: &str) -> (Vec<(Option<i64>, String, String)>, i64) {
    let connection = connection();
    let mut statement = connection
        .prepare("SELECT t5.rowid AS r,c,d FROM t5,t6 ORDER BY a")
        .unwrap();
    let mut rows = Vec::new();
    loop {
        match statement.step().unwrap() {
            StepResult::Row => {
                let row = statement.row().unwrap();
                let rowid = row.get::<&turso_core::Value>(0).unwrap().as_int();
                let first = row.get::<String>(1).unwrap();
                let second = row.get::<String>(2).unwrap();
                if rowid == Some(2) {
                    connection.execute(delete).unwrap();
                }
                rows.push((rowid, first, second));
            }
            StepResult::Done => break,
            StepResult::IO => statement._io().step().unwrap(),
            StepResult::Yield => continue,
            other => panic!("unexpected step: {other:?}"),
        }
    }
    let remaining = connection
        .prepare("SELECT count(*) FROM t5")
        .unwrap()
        .run_collect_rows()
        .unwrap()[0][0]
        .as_int()
        .unwrap();
    let integrity = connection
        .prepare("PRAGMA integrity_check")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert_eq!(integrity[0][0].to_string(), "ok");
    connection
        .execute(
            "BEGIN; INSERT INTO t5 VALUES(7,8); UPDATE t6 SET d='changed' WHERE c='a'; ROLLBACK",
        )
        .unwrap();
    (rows, remaining)
}

fn assert_no_sorter(connection: &Arc<Connection>, query: &str) {
    let bytecode = connection
        .prepare(format!("EXPLAIN {query}"))
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert!(
        bytecode
            .iter()
            .all(|row| row[1].to_string() != "SorterOpen"),
        "unexpected sorter for {query}"
    );
}

#[test]
fn ordered_join_returns_static_rows_without_materializing() {
    let connection = connection();
    let query = "SELECT t5.rowid AS r,c,d FROM t5,t6 ORDER BY a";
    let rows = connection
        .prepare(query)
        .unwrap()
        .run_collect_rows()
        .unwrap();
    let rows: Vec<_> = rows
        .iter()
        .map(|row| (row[0].as_int(), row[1].to_string(), row[2].to_string()))
        .collect();
    assert_eq!(
        rows,
        expected(&[Some(1), Some(1), Some(2), Some(2), Some(3), Some(3)])
    );
    assert_no_sorter(&connection, query);
}

#[test]
fn multi_table_order_prefixes_survive_join_enumeration() {
    let connection = connection();
    connection
        .execute("CREATE TABLE t7(e); INSERT INTO t7 VALUES('x'),('y'); CREATE INDEX i7 ON t7(e)")
        .unwrap();
    for query in [
        "SELECT a,c,e FROM t7,t6,t5 ORDER BY a,t5.rowid,c,t6.rowid,e",
        "SELECT a,c,e FROM t6,t5,t7 ORDER BY a,t5.rowid,c,t6.rowid,e",
        "SELECT a,c,e FROM t7,t6,t5 ORDER BY a DESC,t5.rowid DESC,c DESC,t6.rowid DESC,e DESC",
    ] {
        let rows = connection
            .prepare(query)
            .unwrap()
            .run_collect_rows()
            .unwrap();
        let rows: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    row[0].as_int().unwrap(),
                    row[1].to_string(),
                    row[2].to_string(),
                )
            })
            .collect();
        let mut expected_rows = vec![
            (1, "a".to_string(), "x".to_string()),
            (1, "a".to_string(), "y".to_string()),
            (1, "c".to_string(), "x".to_string()),
            (1, "c".to_string(), "y".to_string()),
            (3, "a".to_string(), "x".to_string()),
            (3, "a".to_string(), "y".to_string()),
            (3, "c".to_string(), "x".to_string()),
            (3, "c".to_string(), "y".to_string()),
            (5, "a".to_string(), "x".to_string()),
            (5, "a".to_string(), "y".to_string()),
            (5, "c".to_string(), "x".to_string()),
            (5, "c".to_string(), "y".to_string()),
        ];
        if query.contains("DESC") {
            expected_rows.reverse();
        }
        assert_eq!(rows, expected_rows);
        assert_no_sorter(&connection, query);
    }
}

#[test]
fn unavailable_order_property_keeps_real_sorter_and_duplicate_order() {
    let connection = connection();
    connection.execute("INSERT INTO t5 VALUES(1,9)").unwrap();
    let query = "SELECT a,rowid FROM t5 ORDER BY a DESC,rowid ASC";
    let rows = connection
        .prepare(query)
        .unwrap()
        .run_collect_rows()
        .unwrap();
    let values: Vec<_> = rows
        .iter()
        .map(|row| (row[0].as_int().unwrap(), row[1].as_int().unwrap()))
        .collect();
    assert_eq!(values, vec![(5, 3), (3, 2), (1, 1), (1, 4)]);
    let bytecode = connection
        .prepare(format!("EXPLAIN {query}"))
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert!(bytecode
        .iter()
        .any(|row| row[1].to_string() == "SorterOpen"));
    let joined_query = "SELECT a,t5.rowid,c FROM t5,t6 ORDER BY a DESC,t5.rowid ASC,c";
    let joined = connection
        .prepare(joined_query)
        .unwrap()
        .run_collect_rows()
        .unwrap();
    let values: Vec<_> = joined
        .iter()
        .map(|row| {
            (
                row[0].as_int().unwrap(),
                row[1].as_int().unwrap(),
                row[2].to_string(),
            )
        })
        .collect();
    assert_eq!(
        values,
        vec![
            (5, 3, "a".to_string()),
            (5, 3, "c".to_string()),
            (3, 2, "a".to_string()),
            (3, 2, "c".to_string()),
            (1, 1, "a".to_string()),
            (1, 1, "c".to_string()),
            (1, 4, "a".to_string()),
            (1, 4, "c".to_string()),
        ]
    );
    let bytecode = connection
        .prepare(format!("EXPLAIN {joined_query}"))
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert!(bytecode
        .iter()
        .any(|row| row[1].to_string() == "SorterOpen"));
}

#[test]
fn order_objective_controls_match_sqlite_results() {
    let connection = connection();
    let reference = rusqlite::Connection::open_in_memory().unwrap();
    reference.execute_batch("CREATE TABLE t5(a,b); CREATE TABLE t6(c,d); INSERT INTO t5 VALUES(1,2),(3,4),(5,6); INSERT INTO t6 VALUES('a','b'),('c','d'); CREATE INDEX i5 ON t5(a); CREATE INDEX i6 ON t6(c);").unwrap();
    let full_setup = "CREATE TABLE f1(x); CREATE TABLE f2(y); INSERT INTO f1 VALUES(1),(3); INSERT INTO f2 VALUES(1),(2)";
    connection.execute(full_setup).unwrap();
    reference.execute_batch(full_setup).unwrap();
    for query in [
        "SELECT a,c FROM t5,t6",
        "SELECT a,c FROM t5,t6 ORDER BY a LIMIT 3",
        "SELECT a,c FROM t6,t5 ORDER BY a DESC LIMIT 3",
        "SELECT a,count(*) FROM t5,t6 GROUP BY a ORDER BY a DESC",
        "SELECT min(a),max(a) FROM t5,t6",
        "SELECT a FROM t5 WHERE EXISTS(SELECT 1 FROM t6 WHERE c='a' AND d=CASE WHEN t5.a>0 THEN 'b' ELSE 'z' END) ORDER BY a",
        "SELECT a FROM t5 WHERE EXISTS(SELECT 1 FROM t6 WHERE c=CAST(t5.a AS TEXT)) ORDER BY a",
        "SELECT a,c FROM t5 LEFT JOIN t6 ON t5.a=t6.rowid ORDER BY a,c",
        "SELECT x,y FROM f1 FULL OUTER JOIN f2 ON f1.x=f2.y ORDER BY x,y",
        "SELECT a FROM t5 ORDER BY a+1 LIMIT 2",
    ] {
        let rows = connection.prepare(query).unwrap().run_collect_rows().unwrap();
        let mut actual: Vec<Vec<Option<String>>> = rows
            .iter()
            .map(|row| {
                row.iter().map(|value| match value {
                    turso_core::Value::Null => None,
                    value => Some(value.to_string()),
                }).collect()
            })
            .collect();
        let mut statement = reference.prepare(query).unwrap();
        let columns = statement.column_count();
        let mut expected: Vec<Vec<Option<String>>> = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|column| {
                        Ok(match row.get_ref(column)? {
                            rusqlite::types::ValueRef::Null => None,
                            rusqlite::types::ValueRef::Integer(value) => Some(value.to_string()),
                            rusqlite::types::ValueRef::Real(value) => Some(value.to_string()),
                            rusqlite::types::ValueRef::Text(value) => Some(std::str::from_utf8(value).unwrap().to_string()),
                            other => panic!("unexpected reference value: {other:?}"),
                        })
                    })
                    .collect()
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        if !query.contains("ORDER BY") {
            actual.sort();
            expected.sort();
        }
        assert_eq!(actual, expected, "{query}");
    }
    assert_no_sorter(
        &connection,
        "SELECT t5.rowid,c,d FROM t6,t5 ORDER BY a DESC LIMIT 3",
    );
}

fn expected(rowids: &[Option<i64>]) -> Vec<(Option<i64>, String, String)> {
    rowids
        .iter()
        .enumerate()
        .map(|(position, rowid)| {
            let (first, second) = if position % 2 == 0 {
                ("a", "b")
            } else {
                ("c", "d")
            };
            (*rowid, first.to_string(), second.to_string())
        })
        .collect()
}

#[test]
fn official_delete_9_2_all_rows() {
    let (rows, remaining) = callback_rows("DELETE FROM t5");
    assert_eq!(remaining, 0);
    assert_eq!(rows, expected(&[Some(1), Some(1), Some(2), None]));
}

#[test]
fn official_delete_9_3_current_row() {
    let (rows, remaining) = callback_rows("DELETE FROM t5 WHERE rowid=2");
    assert_eq!(remaining, 2);
    assert_eq!(
        rows,
        expected(&[Some(1), Some(1), Some(2), None, Some(3), Some(3)])
    );
}

#[test]
fn official_delete_9_4_previous_row_control() {
    let (rows, remaining) = callback_rows("DELETE FROM t5 WHERE rowid=1");
    assert_eq!(remaining, 2);
    assert_eq!(
        rows,
        expected(&[Some(1), Some(1), Some(2), Some(2), Some(3), Some(3)])
    );
}

#[test]
fn official_delete_9_5_future_row() {
    let (rows, remaining) = callback_rows("DELETE FROM t5 WHERE rowid=3");
    assert_eq!(remaining, 2);
    assert_eq!(rows, expected(&[Some(1), Some(1), Some(2), Some(2)]));
}
