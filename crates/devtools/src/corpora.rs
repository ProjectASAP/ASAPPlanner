//! Splitting and SQL catalogs for the checked-in query corpora, shared by
//! the corpus tools (`sketch_coverage`, `variant_coverage`, `analyze_corpora`)
//! so they read the same corpus text against the same tables.
use asap_frontend_sql::SqlCatalog;
use asap_types::ir::schema::{DataType, Field, Schema};

/// The statements of a SQL corpus: `#`/`--` comment lines dropped, then split
/// on `;`.
pub fn sql_statements(corpus: &str) -> Vec<String> {
    corpus
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .split(';')
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_string)
        .collect()
}

/// The queries of a PromQL corpus: one per non-empty, non-`#` line.
pub fn promql_lines(corpus: &str) -> impl Iterator<Item = &str> {
    corpus
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

fn col(name: &str, dtype: DataType) -> Field {
    Field::plain(name, dtype, false)
}

/// The `packets` table of the data-quality-check corpus.
pub fn dqc_catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "packets",
        Schema::new(vec![
            col("srcip", DataType::Utf8),
            col("dstip", DataType::Utf8),
            col("srcport", DataType::Int64),
            col("dstport", DataType::Int64),
            col("proto", DataType::Utf8),
            col("time", DataType::Float64),
            col("pkt_len", DataType::Int64),
        ]),
    )
}

/// The `netflow_table` of the NetFlow corpus.
pub fn netflow_catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "netflow_table",
        Schema::with_time_index(
            vec![
                col("time", DataType::Timestamp),
                col("srcip", DataType::Utf8),
                col("dstip", DataType::Utf8),
                col("srcport", DataType::Int64),
                col("dstport", DataType::Int64),
                col("proto", DataType::Utf8),
                col("pkt_len", DataType::Int64),
            ],
            0,
            vec![],
        ),
    )
}

/// The BGP update and RIB tables, under bare and `bgp.` names: the
/// `bgp_analytics` corpus reads both tables and the jan2024 workload reads
/// every update column.
pub fn bgp_catalog() -> SqlCatalog {
    let updates = Schema::new(vec![
        col("timestamp", DataType::Timestamp),
        col("collector", DataType::Utf8),
        col("peer_ip", DataType::Utf8),
        col("peer_asn", DataType::Int64),
        col("prefix", DataType::Utf8),
        col("operation", DataType::Utf8),
        col("as_path", DataType::Utf8),
        col("origin", DataType::Utf8),
        col("next_hop", DataType::Utf8),
        col("local_pref", DataType::Utf8),
        col("med", DataType::Utf8),
        col("communities", DataType::Utf8),
        col("atomic", DataType::Utf8),
        col("aggr_asn", DataType::Utf8),
        col("aggr_ip", DataType::Utf8),
        col("source_file", DataType::Utf8),
    ]);
    let rib = Schema::new(vec![
        col("snapshot_ts", DataType::Timestamp),
        col("collector", DataType::Utf8),
        col("peer_ip", DataType::Utf8),
        col("peer_asn", DataType::Int64),
        col("prefix", DataType::Utf8),
    ]);
    SqlCatalog::new()
        .with_table("bgp_updates", updates.clone())
        .with_table("bgp.bgp_updates", updates)
        .with_table("bgp_rib_state", rib.clone())
        .with_table("bgp.bgp_rib_state", rib)
}

/// The required `--sample-interval-ms` argument: the sample interval declared
/// for every metric a PromQL query reads.
pub fn parse_sample_interval() -> u64 {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--sample-interval-ms" {
            return args
                .next()
                .expect("--sample-interval-ms requires a value")
                .parse()
                .expect("--sample-interval-ms must be an unsigned integer");
        }
    }
    panic!("--sample-interval-ms is required")
}
