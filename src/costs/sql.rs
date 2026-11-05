//! The queries of the billing export (the detailed, resource-level one).
//!
//! BigQuery bills the bytes of every column a query reads, over every partition it reads, and
//! the table is partitioned by export day (`_PARTITIONTIME`). Each query therefore names the
//! partitions it needs ([`partitions`]): without that, a query reads the whole table, however
//! few days it asks for. Columns are read once per query however often they are named, so
//! each query reads only what its view shows (`resource.global_name` alone costs more than
//! all the rest: it is never read).
//!
//! Values (the project id, dates) are query parameters, never SQL text. The table name cannot
//! be one, so it is checked ([`table`]) before it is put in backquotes.

use super::day::{Day, Range};
use crate::gcp::bigquery::Param;

/// Cloud Run's service id in the export (a Google constant). Only its rows carry an app:
/// `resource.name` is the Cloud Run service.
pub const CLOUD_RUN: &str = "152E-C115-5142";

/// `project.dataset.table`, checked and quoted for the SQL; an error says what is wrong.
pub fn table(name: &str) -> Result<String, String> {
    let parts: Vec<&str> = name.split('.').collect();
    let ok = parts.len() == 3
        && parts.iter().all(|p| !p.is_empty())
        // Project ids hold letters, digits and `-`; datasets and tables letters, digits and `_`.
        && parts[0].chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && parts[1..].iter().all(|p| p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    if ok { Ok(format!("`{name}`")) } else { Err(format!("'{name}' is not project.dataset.table")) }
}

/// The export days (partitions) that can hold the usage of `days`. A row reaches the export on
/// its usage day or later, and two days before is a margin for the export's own clock. Late
/// rows keep coming for a while: up to a week and more inside the month, and a final batch on
/// the first days of the next month. Past that a past month no longer changes, so a query of
/// an old month does not read the year since.
pub fn partitions(days: Range) -> (Day, Day) {
    let from = days.from.plus(-2);
    // Exclusive.
    let to = days.to.plus(4).max(days.to.next_month().plus(3));
    (from, to)
}

fn window_params(days: Range) -> Vec<Param> {
    let (pf, pt) = partitions(days);
    vec![
        Param::date("partition_from", &pf.to_string()),
        Param::date("partition_to", &pt.to_string()),
        Param::date("from", &days.from.to_string()),
        Param::date("to", &days.to.to_string()),
    ]
}

const WHERE: &str = "_PARTITIONTIME >= TIMESTAMP(@partition_from) AND _PARTITIONTIME < TIMESTAMP(@partition_to)
  AND DATE(usage_start_time) BETWEEN @from AND @to";

/// The project's cost per UTC day and service, and per Cloud Run service (app). Gross is
/// `cost`; credits (negative) are summed apart, so net is their sum. `latest` (the start of
/// the newest usage hour) tells how far the export has come.
pub fn daily(table: &str, project: &str, days: Range) -> (String, Vec<Param>) {
    let sql = format!(
        "SELECT
  FORMAT_DATE('%F', DATE(usage_start_time)) AS day,
  project.number AS project_number,
  service.id AS service_id,
  service.description AS service,
  IF(service.id = @cloud_run, resource.name, NULL) AS app,
  SUM(cost) AS cost,
  SUM(IFNULL((SELECT SUM(c.amount) FROM UNNEST(credits) AS c), 0)) AS credits,
  UNIX_SECONDS(MAX(usage_start_time)) AS latest,
  ANY_VALUE(currency) AS currency
FROM {table}
WHERE {WHERE}
  AND project.id = @project
GROUP BY day, project_number, service_id, service, app"
    );
    let mut params = window_params(days);
    params.push(Param::string("project", project));
    params.push(Param::string("cloud_run", CLOUD_RUN));
    (sql, params)
}

/// The project's SKUs per month: cost and usage (in the export's unit), whose ratio is the
/// effective rate.
pub fn skus(table: &str, project: &str, days: Range) -> (String, Vec<Param>) {
    let sql = format!(
        "SELECT
  FORMAT_DATE('%Y-%m', DATE(usage_start_time)) AS month,
  service.description AS service,
  sku.description AS sku,
  usage.unit AS unit,
  SUM(cost) AS cost,
  SUM(usage.amount) AS amount
FROM {table}
WHERE {WHERE}
  AND project.id = @project
GROUP BY month, service, sku, unit"
    );
    let mut params = window_params(days);
    params.push(Param::string("project", project));
    (sql, params)
}

/// The whole billing account per month, project (by number, as budgets name them) and
/// service (by id): gross rows (`credit_type` empty), then each credit type and name.
/// Budgets, account totals and credits come from it.
pub fn account(table: &str, days: Range) -> (String, Vec<Param>) {
    let sql = format!(
        "SELECT
  FORMAT_DATE('%Y-%m', DATE(usage_start_time)) AS month,
  project.number AS project_number,
  service.id AS service_id,
  '' AS credit_type,
  '' AS credit_name,
  SUM(cost) AS amount
FROM {table}
WHERE {WHERE}
GROUP BY month, project_number, service_id
UNION ALL
SELECT
  FORMAT_DATE('%Y-%m', DATE(usage_start_time)),
  project.number,
  service.id,
  IFNULL(c.type, 'OTHER'),
  IFNULL(c.name, ''),
  SUM(c.amount)
FROM {table}, UNNEST(credits) AS c
WHERE {WHERE}
GROUP BY 1, 2, 3, 4, 5"
    );
    (sql, window_params(days))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> Day {
        Day::parse(s).unwrap()
    }

    #[test]
    fn tables_are_checked() {
        assert_eq!(
            table("my-billing-project.billing_export.gcp_billing_export_resource_v1_012345_6789AB_CDEF01").unwrap(),
            "`my-billing-project.billing_export.gcp_billing_export_resource_v1_012345_6789AB_CDEF01`"
        );
        for bad in ["ds.t", "p.ds.t.x", "p.ds.t` WHERE 1=1 --", "p..t", "p.d-s.t"] {
            assert!(table(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn queries_read_only_the_partitions_they_need() {
        // A recent window: from two days before, to the first days of the next month.
        let (from, to) = partitions(Range::new(day("2026-10-01"), day("2026-11-05")));
        assert_eq!((from.to_string(), to.to_string()), ("2026-09-29".into(), "2026-12-04".into()));
        // A past month: its late rows arrive by the 3rd of the next month.
        let (from, to) = partitions(Range::new(day("2026-09-01"), day("2026-10-31")));
        assert_eq!((from.to_string(), to.to_string()), ("2026-08-30".into(), "2026-11-04".into()));
        let (sql, params) = daily("`p.d.t`", "my-platform-123456", Range::new(day("2026-10-20"), day("2026-11-03")));
        assert!(
            sql.contains("_PARTITIONTIME >= TIMESTAMP(@partition_from) AND _PARTITIONTIME < TIMESTAMP(@partition_to)")
        );
        assert!(sql.contains("DATE(usage_start_time) BETWEEN @from AND @to") && sql.contains("project.id = @project"));
        assert!(!sql.contains("my-platform-123456"), "values are parameters");
        assert!(!sql.contains("global_name"), "the costliest column is never read");
        let names: Vec<String> = params.iter().map(|p| format!("{p:?}")).collect();
        assert!(names.iter().any(|p| p.contains("\"partition_from\"") && p.contains("2026-10-18")), "{names:?}");
        assert!(names.iter().any(|p| p.contains("\"cloud_run\"") && p.contains(CLOUD_RUN)), "{names:?}");
        for (sql, _) in [
            skus("`p.d.t`", "p", Range::new(day("2026-10-01"), day("2026-11-05"))),
            account("`p.d.t`", Range::new(day("2026-10-01"), day("2026-11-05"))),
        ] {
            assert_eq!(
                sql.matches("_PARTITIONTIME >= TIMESTAMP(@partition_from)").count(),
                sql.matches("FROM `p.d.t`").count()
            );
        }
    }
}
