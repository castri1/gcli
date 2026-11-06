//! Cloud SQL Admin API v1, read-only: a project's instances and their backup runs. Every call
//! here is a GET. gcli never connects to a database, and of an instance it keeps neither
//! addresses nor certificates: only what the structs below name.

use serde::Deserialize;

use super::{Gcp, Opts, Result};
use crate::util::{parse_rfc3339, uri_encode};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Instance {
    pub name: String,
    /// `RUNNABLE`, `MAINTENANCE`, `FAILED`… A stopped instance can still say `RUNNABLE`: see
    /// [`Instance::stopped`].
    pub state: String,
    /// `POSTGRES_15`, `MYSQL_8_0`…
    pub database_version: String,
    pub region: String,
    pub gce_zone: String,
    /// `CLOUD_SQL_INSTANCE`, `READ_REPLICA_INSTANCE`, `ON_PREMISES_INSTANCE`.
    pub instance_type: String,
    /// `project:region:name`
    pub connection_name: String,
    pub master_instance_name: String,
    pub create_time: String,
    pub settings: Settings,
    pub scheduled_maintenance: Option<ScheduledMaintenance>,
}

impl Instance {
    pub fn is_postgres(&self) -> bool {
        self.database_version.starts_with("POSTGRES")
    }

    pub fn is_runnable(&self) -> bool {
        self.state == "RUNNABLE"
    }

    /// Turned off by its activation policy, whatever its state says.
    pub fn stopped(&self) -> bool {
        self.settings.activation_policy == "NEVER"
    }

    pub fn is_replica(&self) -> bool {
        self.instance_type == "READ_REPLICA_INSTANCE" || !self.master_instance_name.is_empty()
    }

    /// A server outside Cloud SQL, represented here for a migration: Google reports nothing of it.
    pub fn is_external(&self) -> bool {
        self.instance_type == "ON_PREMISES_INSTANCE"
    }

    /// The `max_connections` flag. Unset, the limit depends on the tier: unknown here, never guessed.
    pub fn max_connections(&self) -> Option<f64> {
        let flag = self.settings.database_flags.iter().find(|f| f.name == "max_connections")?;
        flag.value.trim().parse::<u32>().ok().filter(|n| *n > 0).map(f64::from)
    }

    pub fn created(&self) -> Option<f64> {
        parse_rfc3339(&self.create_time)
    }

    /// Its page in the Cloud console, under [`super::console_url`].
    pub fn console_path(&self) -> String {
        format!("sql/instances/{}/overview", uri_encode(&self.name))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub tier: String,
    /// `ZONAL` or `REGIONAL`.
    pub availability_type: String,
    /// `ALWAYS`, or `NEVER` for a stopped instance.
    pub activation_policy: String,
    /// An int64, which Google sends as a string.
    pub data_disk_size_gb: String,
    /// Google's default, when absent, is on.
    pub storage_auto_resize: Option<bool>,
    /// An int64 as a string; `0` is no limit.
    pub storage_auto_resize_limit: String,
    pub database_flags: Vec<Flag>,
    pub backup_configuration: BackupConfiguration,
    pub deletion_protection_enabled: bool,
    pub ip_configuration: IpConfiguration,
    pub maintenance_window: Option<MaintenanceWindow>,
}

impl Settings {
    pub fn auto_resize(&self) -> bool {
        self.storage_auto_resize.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Flag {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupConfiguration {
    pub enabled: bool,
    /// `HH:MM`, UTC: when the backup window starts.
    pub start_time: String,
    pub point_in_time_recovery_enabled: bool,
    pub backup_retention_settings: Option<RetentionSettings>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetentionSettings {
    pub retained_backups: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IpConfiguration {
    pub ipv4_enabled: bool,
    pub private_network: String,
    pub require_ssl: bool,
    /// `ENCRYPTED_ONLY`, `ALLOW_UNENCRYPTED_AND_ENCRYPTED`…
    pub ssl_mode: String,
    pub authorized_networks: Vec<AuthorizedNetwork>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AuthorizedNetwork {
    /// A CIDR range.
    pub value: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MaintenanceWindow {
    /// 1 is Monday, 7 Sunday; 0 or absent, any day.
    pub day: Option<u32>,
    /// UTC.
    pub hour: Option<u32>,
}

impl MaintenanceWindow {
    /// `Mon 03:00 UTC`, or `any time`.
    pub fn text(&self) -> String {
        const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let day = self.day.filter(|d| (1..=7).contains(d)).map(|d| DAYS[d as usize - 1]);
        match (day, self.hour) {
            (Some(d), Some(h)) => format!("{d} {h:02}:00 UTC"),
            (Some(d), None) => d.to_string(),
            (None, Some(h)) => format!("any day {h:02}:00 UTC"),
            (None, None) => "any time".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScheduledMaintenance {
    pub start_time: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BackupRun {
    /// `SUCCESSFUL`, `FAILED`, `OVERDUE`, `RUNNING`, `ENQUEUED`, `SKIPPED`, `DELETED`…
    pub status: String,
    pub enqueued_time: String,
    pub start_time: String,
    pub end_time: String,
    pub window_start_time: String,
    pub error: BackupError,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BackupError {
    pub code: String,
    pub message: String,
}

impl BackupRun {
    pub fn succeeded(&self) -> bool {
        self.status == "SUCCESSFUL"
    }

    /// When it was started: what orders the runs.
    pub fn started(&self) -> Option<f64> {
        [&self.start_time, &self.enqueued_time, &self.window_start_time].into_iter().find_map(|t| parse_rfc3339(t))
    }

    /// When it ended, or what is known of when it ran.
    pub fn ended(&self) -> Option<f64> {
        parse_rfc3339(&self.end_time).or_else(|| self.started())
    }
}

fn instances_url(gcp: &Gcp) -> String {
    format!("https://sqladmin.googleapis.com/v1/projects/{}/instances", uri_encode(&gcp.project))
}

/// The client's project's instances, in any region, sorted by name.
pub fn list(gcp: &Gcp) -> Result<Vec<Instance>> {
    let mut all: Vec<Instance> = gcp.list(&instances_url(gcp), &[("maxResults", "100")], "items", Opts::default())?;
    all.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(all)
}

/// One instance, by name.
pub fn get(gcp: &Gcp, name: &str) -> Result<Instance> {
    gcp.get(&format!("{}/{}", instances_url(gcp), uri_encode(name)), &[])
}

/// An instance's backup runs, newest first, as far back as the newest successful one: Google lists
/// them that way, and what is older says nothing a check needs.
pub fn backup_runs(gcp: &Gcp, name: &str) -> Result<Vec<BackupRun>> {
    let url = format!("{}/{}/backupRuns", instances_url(gcp), uri_encode(name));
    let mut all = Vec::new();
    gcp.pages(&url, &[("maxResults", "30")], "items", Opts::default(), |page: Vec<BackupRun>| {
        all.extend(page);
        !all.iter().any(BackupRun::succeeded)
    })?;
    all.sort_by(|a, b| b.started().unwrap_or(0.0).total_cmp(&a.started().unwrap_or(0.0)));
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};

    fn client(fake: &MockServer) -> std::sync::Arc<Gcp> {
        Gcp::with("my-platform-123456", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(fake.url()))
    }

    const INSTANCE: &str = r#"{
        "name":"shared-db","state":"RUNNABLE","databaseVersion":"POSTGRES_15","region":"us-east1","gceZone":"us-east1-b",
        "instanceType":"CLOUD_SQL_INSTANCE","connectionName":"my-platform-123456:us-east1:shared-db",
        "createTime":"2026-01-01T00:00:00.000Z",
        "serverCaCert":{"cert":"-----BEGIN CERTIFICATE-----","sha1Fingerprint":"ab"},"ipAddresses":[{"ipAddress":"203.0.113.9"}],
        "settings":{"tier":"db-custom-2-7680","availabilityType":"ZONAL","activationPolicy":"ALWAYS","dataDiskSizeGb":"100",
          "storageAutoResize":true,"storageAutoResizeLimit":"0","deletionProtectionEnabled":true,
          "databaseFlags":[{"name":"log_min_duration_statement","value":"100"},{"name":"max_connections","value":"500"}],
          "backupConfiguration":{"enabled":true,"startTime":"02:00","pointInTimeRecoveryEnabled":true,
            "backupRetentionSettings":{"retainedBackups":7,"retentionUnit":"COUNT"}},
          "ipConfiguration":{"ipv4Enabled":true,"privateNetwork":"projects/p/global/networks/default","sslMode":"ENCRYPTED_ONLY",
            "authorizedNetworks":[{"value":"192.0.2.0/24","name":"office"}]},
          "maintenanceWindow":{"day":1,"hour":3}},
        "scheduledMaintenance":{"startTime":"2026-11-08T06:00:00Z","canDefer":true}}"#;

    #[test]
    fn reads_what_it_names_of_an_instance() {
        let fake = MockServer::start(vec![
            Reply::json(
                200,
                &format!(r#"{{"items":[{{"name":"zeta","state":"FAILED"}},{INSTANCE}],"nextPageToken":"p2"}}"#),
            ),
            Reply::json(200, r#"{"kind":"sql#instancesList"}"#),
        ]);
        let all = list(&client(&fake)).unwrap();
        assert_eq!(all.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["shared-db", "zeta"], "by name");
        let i = &all[0];
        assert_eq!((i.database_version.as_str(), i.settings.tier.as_str()), ("POSTGRES_15", "db-custom-2-7680"));
        assert_eq!(i.settings.data_disk_size_gb, "100");
        assert!(i.is_postgres() && i.is_runnable() && !i.stopped() && !i.is_replica() && !i.is_external());
        assert_eq!(i.max_connections(), Some(500.0));
        assert!(i.settings.auto_resize() && i.settings.deletion_protection_enabled);
        let backups = &i.settings.backup_configuration;
        assert!(backups.enabled && backups.point_in_time_recovery_enabled);
        assert_eq!(backups.backup_retention_settings.as_ref().unwrap().retained_backups, Some(7));
        assert_eq!(i.settings.ip_configuration.authorized_networks[0].value, "192.0.2.0/24");
        assert_eq!(i.settings.maintenance_window.as_ref().unwrap().text(), "Mon 03:00 UTC");
        assert_eq!(i.scheduled_maintenance.as_ref().unwrap().start_time, "2026-11-08T06:00:00Z");
        assert_eq!(all[1].max_connections(), None, "no flag: unknown");
        assert!(all[1].settings.auto_resize(), "Google's default when absent");
        assert!(!format!("{i:?}").contains("203.0.113.9") && !format!("{i:?}").contains("CERTIFICATE"), "not kept");
        let requests = fake.requests();
        assert_eq!(requests[0].path, "/sqladmin/v1/projects/my-platform-123456/instances?maxResults=100");
        assert!(requests[1].path.ends_with("&pageToken=p2"));
        assert!(requests.iter().all(|r| r.method == "GET"));
    }

    #[test]
    fn one_instance_and_a_limit_that_is_not_a_number() {
        let fake = MockServer::start(vec![Reply::json(
            200,
            r#"{"name":"a b","settings":{"databaseFlags":[{"name":"max_connections","value":"lots"}],"activationPolicy":"NEVER","storageAutoResize":false}}"#,
        )]);
        let i = get(&client(&fake), "a b").unwrap();
        assert_eq!(i.max_connections(), None);
        assert!(i.stopped() && !i.settings.auto_resize());
        assert_eq!(fake.requests()[0].path, "/sqladmin/v1/projects/my-platform-123456/instances/a%20b");
        assert_eq!(i.console_path(), "sql/instances/a%20b/overview");
    }

    #[test]
    fn replicas_external_servers_and_maintenance_windows() {
        let replica = Instance { master_instance_name: "main".into(), ..Default::default() };
        assert!(replica.is_replica());
        let typed = Instance { instance_type: "READ_REPLICA_INSTANCE".into(), ..Default::default() };
        assert!(typed.is_replica());
        assert!(Instance { instance_type: "ON_PREMISES_INSTANCE".into(), ..Default::default() }.is_external());
        let window = |day, hour| MaintenanceWindow { day, hour }.text();
        assert_eq!(window(Some(7), Some(23)), "Sun 23:00 UTC");
        assert_eq!(window(Some(0), Some(3)), "any day 03:00 UTC");
        assert_eq!(window(None, None), "any time");
    }

    #[test]
    fn backup_runs_stop_at_the_newest_success() {
        let fake = MockServer::start(vec![
            Reply::json(
                200,
                r#"{"items":[{"status":"FAILED","startTime":"2026-11-05T07:00:00.5Z","error":{"code":"X","message":"disk"}},
                             {"status":"RUNNING","enqueuedTime":"2026-11-05T08:00:00Z"}],"nextPageToken":"p2"}"#,
            ),
            Reply::json(
                200,
                r#"{"items":[{"status":"SUCCESSFUL","startTime":"2026-11-04T07:00:00Z","endTime":"2026-11-04T07:02:00Z"}],"nextPageToken":"p3"}"#,
            ),
        ]);
        let runs = backup_runs(&client(&fake), "shared-db").unwrap();
        assert_eq!(runs.iter().map(|r| r.status.as_str()).collect::<Vec<_>>(), ["RUNNING", "FAILED", "SUCCESSFUL"]);
        assert_eq!(runs[1].error.message, "disk");
        assert_eq!(runs[2].ended(), parse_rfc3339("2026-11-04T07:02:00Z"));
        assert_eq!(runs[0].ended(), parse_rfc3339("2026-11-05T08:00:00Z"), "still running: when it started");
        let paths: Vec<String> = fake.requests().iter().map(|r| r.path.clone()).collect();
        assert_eq!(paths.len(), 2, "the third page is not read: {paths:?}");
        assert_eq!(paths[0], "/sqladmin/v1/projects/my-platform-123456/instances/shared-db/backupRuns?maxResults=30");
    }
}
