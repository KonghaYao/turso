use super::{ComparisonResult, parse_expected_rows};
use std::collections::BTreeMap;

/// Compare rows as multisets, retaining duplicate occurrences without assuming
/// an order for the result rows.
pub fn compare(actual: &[Vec<String>], expected_lines: &[String]) -> ComparisonResult {
    let expected_rows = parse_expected_rows(expected_lines);

    // Keep columns separate: joining them with `|` can conflate distinct rows.
    let mut actual_counts: BTreeMap<&[String], usize> = BTreeMap::new();
    let mut expected_counts: BTreeMap<&[String], usize> = BTreeMap::new();
    for row in actual {
        *actual_counts.entry(row).or_default() += 1;
    }
    for row in &expected_rows {
        *expected_counts.entry(row).or_default() += 1;
    }

    if actual_counts == expected_counts {
        return ComparisonResult::Match;
    }

    let mut missing = String::new();
    for (expected_row, expected_count) in &expected_counts {
        let actual_count = actual_counts.get(expected_row).copied().unwrap_or(0);
        if *expected_count > actual_count {
            missing.push_str(&format!(
                "  - {} ({} missing)\n",
                expected_row.join("|"),
                expected_count - actual_count
            ));
        }
    }
    let mut extra = String::new();
    for (actual_row, actual_count) in &actual_counts {
        let expected_count = expected_counts.get(actual_row).copied().unwrap_or(0);
        if *actual_count > expected_count {
            extra.push_str(&format!(
                "  + {} ({} extra)\n",
                actual_row.join("|"),
                actual_count - expected_count
            ));
        }
    }
    let mut reason = String::new();
    if !missing.is_empty() {
        reason.push_str("Missing rows:\n");
        reason.push_str(&missing);
    }
    if !extra.is_empty() {
        reason.push_str("Extra rows:\n");
        reason.push_str(&extra);
    }
    ComparisonResult::mismatch(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unordered_empty() {
        let actual: Vec<Vec<String>> = vec![];
        let expected: Vec<String> = vec![];
        assert!(compare(&actual, &expected).is_match());
    }

    #[test]
    fn test_unordered_same_order() {
        let actual = vec![vec!["1".to_string()], vec!["2".to_string()]];
        let expected = vec!["1".to_string(), "2".to_string()];
        assert!(compare(&actual, &expected).is_match());
    }

    #[test]
    fn test_unordered_different_order() {
        let actual = vec![vec!["2".to_string()], vec!["1".to_string()]];
        let expected = vec!["1".to_string(), "2".to_string()];
        assert!(compare(&actual, &expected).is_match());
    }

    #[test]
    fn test_unordered_multi_column() {
        let actual = vec![
            vec!["2".to_string(), "Bob".to_string()],
            vec!["1".to_string(), "Alice".to_string()],
        ];
        let expected = vec!["1|Alice".to_string(), "2|Bob".to_string()];
        assert!(compare(&actual, &expected).is_match());
    }

    #[test]
    fn test_unordered_missing_row() {
        let actual = vec![vec!["1".to_string()]];
        let expected = vec!["1".to_string(), "2".to_string()];

        let result = compare(&actual, &expected);
        assert!(!result.is_match());

        if let ComparisonResult::Mismatch { reason } = result {
            assert!(reason.contains("Missing rows"));
            assert!(reason.contains("2"));
        }
    }

    #[test]
    fn test_unordered_extra_row() {
        let actual = vec![
            vec!["1".to_string()],
            vec!["2".to_string()],
            vec!["3".to_string()],
        ];
        let expected = vec!["1".to_string(), "2".to_string()];

        let result = compare(&actual, &expected);
        assert!(!result.is_match());

        if let ComparisonResult::Mismatch { reason } = result {
            assert!(reason.contains("Extra rows"));
            assert!(reason.contains("3"));
        }
    }

    #[test]
    fn test_unordered_wrong_value() {
        let actual = vec![vec!["1".to_string()], vec!["3".to_string()]];
        let expected = vec!["1".to_string(), "2".to_string()];

        let result = compare(&actual, &expected);
        assert!(!result.is_match());

        if let ComparisonResult::Mismatch { reason } = result {
            assert!(reason.contains("Missing rows"));
            assert!(reason.contains("2"));
            assert!(reason.contains("Extra rows"));
            assert!(reason.contains("3"));
        }
    }

    #[test]
    fn test_unordered_missing_duplicate() {
        let actual = vec![vec!["1".into()], vec!["2".into()]];
        let expected = vec!["1".into(), "1".into(), "2".into()];
        let result = compare(&actual, &expected);
        assert!(!result.is_match(), "one missing occurrence must not pass");
        if let ComparisonResult::Mismatch { reason } = result {
            assert!(reason.contains("Missing rows"));
            assert!(reason.contains("1"));
        }
    }

    #[test]
    fn test_unordered_extra_duplicate() {
        let actual = vec![vec!["1".into()], vec!["1".into()], vec!["2".into()]];
        let expected = vec!["1".into(), "2".into()];
        let result = compare(&actual, &expected);
        assert!(!result.is_match(), "one extra occurrence must not pass");
        if let ComparisonResult::Mismatch { reason } = result {
            assert!(reason.contains("Extra rows"));
            assert!(reason.contains("1"));
        }
    }

    #[test]
    fn test_unordered_duplicate_rows_different_order() {
        let actual = vec![vec!["1".into()], vec!["2".into()], vec!["1".into()]];
        let expected = vec!["1".into(), "1".into(), "2".into()];
        assert!(compare(&actual, &expected).is_match());
    }

    #[test]
    fn test_unordered_column_separator_is_not_a_row_separator() {
        let actual = vec![vec!["a|b".into(), "c".into()]];
        let expected = vec!["a|b|c".into()];
        assert!(!compare(&actual, &expected).is_match());
    }
}
