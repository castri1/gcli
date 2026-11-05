//! Cloud Billing Budget API v1: a billing account's budgets. It gives their configuration only
//! (amount, filters, alert thresholds), never what was spent: Costs computes each budget's
//! spend from the billing export with the budget's own filters (`crate::costs::budget`).

use serde::{Deserialize, Serialize};

use super::{Gcp, Opts, Result};
use crate::util::uri_encode;

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Budget {
    /// `billingAccounts/{account}/budgets/{id}`
    pub name: String,
    pub display_name: String,
    pub budget_filter: Filter,
    pub amount: Amount,
    pub threshold_rules: Vec<ThresholdRule>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Filter {
    /// `projects/{number}`; none: every project of the billing account.
    pub projects: Vec<String>,
    /// `services/{id}` (`services/152E-C115-5142`); none: every service.
    pub services: Vec<String>,
    /// `INCLUDE_ALL_CREDITS` (the default), `EXCLUDE_ALL_CREDITS` or `INCLUDE_SPECIFIED_CREDITS`.
    pub credit_types_treatment: String,
    /// With `INCLUDE_SPECIFIED_CREDITS`: the export's credit types (`PROMOTION`, `DISCOUNT`…).
    pub credit_types: Vec<String>,
    /// `MONTH` (the default), `QUARTER` or `YEAR`.
    pub calendar_period: String,
    pub custom_period: Option<CustomPeriod>,
    /// Filters the export cannot be read by here (it is not queried by label or folder): the
    /// spend then says so.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub labels: serde_json::Map<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resource_ancestors: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subaccounts: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CustomPeriod {
    pub start_date: Date,
    /// None: open-ended.
    pub end_date: Option<Date>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Date {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Amount {
    pub specified_amount: Option<Money>,
    /// Present (empty) when the budget is last period's spend.
    pub last_period_amount: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Money {
    pub currency_code: String,
    /// int64, as a string.
    pub units: String,
    pub nanos: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThresholdRule {
    /// 0.5 is 50 %.
    pub threshold_percent: f64,
    /// `CURRENT_SPEND` (the default) or `FORECASTED_SPEND`.
    pub spend_basis: String,
}

/// How a budget counts credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credits {
    All,
    None,
    Specified,
}

impl Filter {
    /// The project numbers it covers; empty: all of them.
    pub fn project_numbers(&self) -> Vec<&str> {
        self.projects.iter().map(|p| p.strip_prefix("projects/").unwrap_or(p)).collect()
    }

    /// The service ids it covers; empty: all of them.
    pub fn service_ids(&self) -> Vec<&str> {
        self.services.iter().map(|s| s.strip_prefix("services/").unwrap_or(s)).collect()
    }

    pub fn credits(&self) -> Credits {
        match self.credit_types_treatment.as_str() {
            "EXCLUDE_ALL_CREDITS" => Credits::None,
            "INCLUDE_SPECIFIED_CREDITS" => Credits::Specified,
            _ => Credits::All,
        }
    }

    /// Filters that the billing export is not read by here.
    pub fn unsupported(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.labels.is_empty() {
            out.push("labels");
        }
        if !self.resource_ancestors.is_empty() {
            out.push("folders or organizations");
        }
        if !self.subaccounts.is_empty() {
            out.push("subaccounts");
        }
        out
    }
}

impl Budget {
    /// Its amount when it is a fixed one (`None` for "last period's spend").
    pub fn fixed_amount(&self) -> Option<f64> {
        let m = self.amount.specified_amount.as_ref()?;
        Some(m.units.trim().parse::<f64>().unwrap_or(0.0) + m.nanos as f64 / 1e9)
    }

    pub fn currency(&self) -> &str {
        self.amount.specified_amount.as_ref().map_or("", |m| m.currency_code.as_str())
    }
}

/// Every budget of `account`. The Budget API bills nothing but needs a quota project with the
/// API enabled: `quota_project` (the environment's project, say).
pub fn list(gcp: &Gcp, account: &str, quota_project: &str) -> Result<Vec<Budget>> {
    let url = format!("https://billingbudgets.googleapis.com/v1/billingAccounts/{}/budgets", uri_encode(account));
    let opts = Opts { quota_project: Some(quota_project), idempotent: true };
    gcp.list(&url, &[("pageSize", "100")], "budgets", opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::auth;
    use crate::gcp::mock::{MockServer, Reply};

    #[test]
    fn lists_budgets_across_pages() {
        let server = MockServer::start(vec![
            Reply::json(
                200,
                r#"{"budgets":[{"name":"billingAccounts/012345-6789AB-CDEF01/budgets/b1","displayName":"platform gross",
                    "budgetFilter":{"projects":["projects/123456789012"],"services":["services/152E-C115-5142"],
                      "creditTypesTreatment":"EXCLUDE_ALL_CREDITS","calendarPeriod":"MONTH"},
                    "amount":{"specifiedAmount":{"currencyCode":"USD","units":"550","nanos":500000000}},
                    "thresholdRules":[{"thresholdPercent":0.5,"spendBasis":"CURRENT_SPEND"},{"thresholdPercent":1,"spendBasis":"FORECASTED_SPEND"}],
                    "notificationsRule":{},"etag":"1"}],"nextPageToken":"n2"}"#,
            ),
            Reply::json(
                200,
                r#"{"budgets":[{"name":"billingAccounts/012345-6789AB-CDEF01/budgets/b2","displayName":"account net",
                    "budgetFilter":{"creditTypesTreatment":"INCLUDE_SPECIFIED_CREDITS","creditTypes":["PROMOTION"],
                      "customPeriod":{"startDate":{"year":2026,"month":7,"day":1}},"labels":{"team":{"values":["a"]}}},
                    "amount":{"lastPeriodAmount":{}},"thresholdRules":[{"thresholdPercent":0.9}]}]}"#,
            ),
        ]);
        let gcp = Gcp::with("my-platform-123456", "", auth::tests::fixed("t"), Some(server.url()));
        let budgets = list(&gcp, "012345-6789AB-CDEF01", "my-platform-123456").unwrap();
        assert_eq!(budgets.len(), 2);
        let b = &budgets[0];
        assert_eq!((b.display_name.as_str(), b.currency()), ("platform gross", "USD"));
        assert_eq!(b.fixed_amount(), Some(550.5));
        assert_eq!(b.budget_filter.project_numbers(), ["123456789012"]);
        assert_eq!(b.budget_filter.service_ids(), ["152E-C115-5142"]);
        assert_eq!(b.budget_filter.credits(), Credits::None);
        assert_eq!(b.budget_filter.calendar_period, "MONTH");
        assert_eq!(
            b.threshold_rules[1],
            ThresholdRule { threshold_percent: 1.0, spend_basis: "FORECASTED_SPEND".into() }
        );
        let b = &budgets[1];
        assert_eq!(b.fixed_amount(), None);
        assert!(b.amount.last_period_amount.is_some());
        assert!(b.budget_filter.project_numbers().is_empty(), "the whole billing account");
        assert_eq!(b.budget_filter.credits(), Credits::Specified);
        assert_eq!(b.budget_filter.credit_types, ["PROMOTION"]);
        assert_eq!(b.budget_filter.custom_period.as_ref().unwrap().start_date, Date { year: 2026, month: 7, day: 1 });
        assert_eq!(b.budget_filter.unsupported(), ["labels"]);
        let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [
                "/billingbudgets/v1/billingAccounts/012345-6789AB-CDEF01/budgets?pageSize=100",
                "/billingbudgets/v1/billingAccounts/012345-6789AB-CDEF01/budgets?pageSize=100&pageToken=n2"
            ]
        );
        assert_eq!(server.requests()[0].header("x-goog-user-project"), Some("my-platform-123456"));
        // Cached as read.
        let text = serde_json::to_string(&budgets).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Budget>>(&text).unwrap(), budgets);
    }
}
