//! The parser dialect for `SqlDialect::DataFusionSQL`.
//!
//! sqlparser's `GenericDialect` leaves `FILTER (WHERE …)` on aggregate calls
//! off (`supports_filter_during_aggregation`), and DataFusion only selects a
//! dialect by name — so `count(x) FILTER (WHERE p)` cannot reach the planner
//! through `SessionContext::sql`. This wrapper is `GenericDialect` with that
//! one switch flipped (issue #466); `lower` parses through
//! `DFParser::parse_sql_with_dialect` with it and plans the statement itself,
//! exactly as the ClickHouse path already does.

use std::any::TypeId;

use datafusion::sql::sqlparser::dialect::{Dialect, GenericDialect};

#[derive(Debug, Default)]
pub(crate) struct GenericWithAggregateFilter;

/// Forward every boolean switch `GenericDialect` overrides, so the only
/// behavioural difference is `supports_filter_during_aggregation`.
macro_rules! forward_to_generic {
    ($($method:ident),* $(,)?) => {
        $(fn $method(&self) -> bool {
            GenericDialect.$method()
        })*
    };
}

impl Dialect for GenericWithAggregateFilter {
    /// The parser's own `dialect_of!(… is GenericDialect)` checks keep
    /// matching, so generic-only syntax paths stay enabled.
    fn dialect(&self) -> TypeId {
        GenericDialect.dialect()
    }

    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        GenericDialect.is_delimited_identifier_start(ch)
    }

    fn is_identifier_start(&self, ch: char) -> bool {
        GenericDialect.is_identifier_start(ch)
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        GenericDialect.is_identifier_part(ch)
    }

    fn supports_filter_during_aggregation(&self) -> bool {
        true
    }

    forward_to_generic!(
        supports_unicode_string_literal,
        supports_group_by_expr,
        supports_connect_by,
        supports_match_recognize,
        supports_start_transaction_modifier,
        supports_window_function_null_treatment_arg,
        supports_dictionary_syntax,
        supports_window_clause_named_window_reference,
        supports_parenthesized_set_variables,
        supports_select_wildcard_except,
        support_map_literal_syntax,
        allow_extract_custom,
        allow_extract_single_quotes,
        supports_create_index_with_clause,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::sql::parser::DFParser;

    // Every switch `GenericDialect` sets is mirrored, and only the aggregate
    // FILTER switch differs.
    #[test]
    fn mirrors_generic_except_for_aggregate_filter() {
        let ours = GenericWithAggregateFilter;
        let generic = GenericDialect;
        assert_eq!(ours.dialect(), generic.dialect());
        for ch in ['"', '`', '_', '#', '@', '$', 'a', '1', ' '] {
            assert_eq!(
                ours.is_delimited_identifier_start(ch),
                generic.is_delimited_identifier_start(ch)
            );
            assert_eq!(
                ours.is_identifier_start(ch),
                generic.is_identifier_start(ch)
            );
            assert_eq!(ours.is_identifier_part(ch), generic.is_identifier_part(ch));
        }
        assert_eq!(
            ours.supports_group_by_expr(),
            generic.supports_group_by_expr()
        );
        assert!(!generic.supports_filter_during_aggregation());
        assert!(ours.supports_filter_during_aggregation());
    }

    // The generic dialect rejects an aggregate FILTER clause; ours parses it.
    #[test]
    fn parses_aggregate_filter_clause() {
        let sql = "SELECT count(*) FILTER (WHERE a > 1) FROM t";
        assert!(DFParser::parse_sql_with_dialect(sql, &GenericDialect).is_err());
        assert_eq!(
            DFParser::parse_sql_with_dialect(sql, &GenericWithAggregateFilter)
                .unwrap()
                .len(),
            1
        );
    }
}
