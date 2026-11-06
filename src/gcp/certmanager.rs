//! Certificate Manager API v1, read-only: the certificates a load balancer serves through a
//! certificate map, which Compute Engine's own list does not hold. The certificate itself
//! (`pemCertificate`) is left unread: only its names, state and dates matter here.

use serde::Deserialize;

use super::{Gcp, Opts, Result};
use crate::util::uri_encode;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Certificate {
    /// `projects/{p}/locations/{l}/certificates/{id}`
    pub name: String,
    pub san_dnsnames: Vec<String>,
    pub expire_time: String,
    /// Absent on a self-managed one.
    pub managed: Option<Managed>,
    /// The certificate map entries that serve it.
    pub used_by: Vec<UsedBy>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Managed {
    /// `ACTIVE`, `PROVISIONING`, `FAILED`.
    pub state: String,
    pub domains: Vec<String>,
    pub authorization_attempt_info: Vec<Attempt>,
}

/// How proving one domain went.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Attempt {
    pub domain: String,
    /// `AUTHORIZED`, `AUTHORIZING`, `FAILED`.
    pub state: String,
    pub failure_reason: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UsedBy {
    pub name: String,
}

impl Certificate {
    pub fn id(&self) -> &str {
        super::run::last_segment(&self.name)
    }
}

/// Every certificate of the client's project, in every location.
pub fn certificates(gcp: &Gcp) -> Result<Vec<Certificate>> {
    let url = format!(
        "https://certificatemanager.googleapis.com/v1/projects/{}/locations/-/certificates",
        uri_encode(&gcp.project)
    );
    gcp.list(&url, &[("pageSize", "200")], "certificates", Opts::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};

    #[test]
    fn reads_certificates_from_every_location_without_their_body() {
        let server = MockServer::start(vec![Reply::json(
            200,
            r#"{"certificates":[{"name":"projects/p/locations/global/certificates/wild",
                "sanDnsnames":["*.example.com"],"expireTime":"2027-01-04T02:52:34Z","pemCertificate":"-----BEGIN",
                "managed":{"state":"ACTIVE","domains":["*.example.com"],
                           "authorizationAttemptInfo":[{"domain":"*.example.com","state":"AUTHORIZED"}]},
                "usedBy":[{"name":"//certificatemanager.googleapis.com/projects/1/locations/global/certificateMaps/m/certificateMapEntries/e"}]}]}"#,
        )]);
        let gcp = Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        let all = certificates(&gcp).unwrap();
        assert_eq!(all[0].id(), "wild");
        assert_eq!(all[0].managed.as_ref().unwrap().state, "ACTIVE");
        assert_eq!(all[0].used_by.len(), 1);
        assert!(format!("{:?}", all[0]).find("BEGIN").is_none(), "the body is not kept");
        assert_eq!(
            server.requests()[0].path,
            "/certificatemanager/v1/projects/p/locations/-/certificates?pageSize=200"
        );
    }
}
