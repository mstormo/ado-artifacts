//! Azure DevOps REST access: authenticated keep-alive API client, target resolution, failure report.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Url;
use serde_json::Value;

use crate::auth::{self, Credential, PAT_REJECTED};
use crate::config::Stored;
use crate::util::*;

pub const SAS_BATCH: usize = 2000; // blob ids per dedup/urls request

pub struct Ado {
    pub org: String,
    pub project: String,
    tenant: Option<String>,
    cred: Mutex<Credential>,
    refresh_lock: tokio::sync::Mutex<()>,
    /// Keep-alive pooled client for all API calls (a fresh TLS connection per call stalls).
    pub http: reqwest::Client,
    pub vsblob: String,
}

/// The rule behind [`Ado::may_send_credential`]: HTTPS to the same host and port as the organization
/// URL or its blob service URL.
fn credential_allowed(org: &str, vsblob: &str, url: &Url) -> bool {
    let same = |base: &str| {
        Url::parse(base).is_ok_and(|b| b.host_str() == url.host_str() && b.port_or_known_default() == url.port_or_known_default())
    };
    url.scheme() == "https" && (same(org) || same(vsblob))
}

fn authority(u: &Url) -> String {
    match (u.host_str(), u.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    }
}

impl Ado {
    /// Connect to the organization, using the first credential that authenticates.
    pub async fn new(org: &str, project: &str, tenant: Option<&str>, stored: Option<&Stored>) -> Result<Arc<Ado>> {
        // Re-checked here, where every credential use starts: only Azure DevOps Services hosts.
        let org = crate::config::normalize_org(org)?;
        let u = Url::parse(&org).or_else(|_| fatal(format!("invalid organization URL: {org}")))?;
        let host = u.host_str().unwrap_or("").to_string();
        let vsblob = if host.ends_with(".visualstudio.com") {
            format!("https://{}.vsblob.visualstudio.com", host.split('.').next().unwrap_or(""))
        } else {
            let first = u.path().trim_matches('/').split('/').next().unwrap_or("");
            format!("https://vsblob.dev.azure.com/{first}")
        };
        let http = auth::build_http()?;
        let cred = auth::resolve_credential(&http, &org, tenant, stored).await?;
        Ok(Arc::new(Ado {
            org,
            project: project.to_string(),
            tenant: tenant.map(str::to_string),
            cred: Mutex::new(cred),
            refresh_lock: tokio::sync::Mutex::new(()),
            http,
            vsblob,
        }))
    }

    /// Where the credential in use came from.
    pub fn credential_source(&self) -> String {
        self.cred.lock().unwrap_or_else(|e| e.into_inner()).source.describe()
    }

    /// Whether the credential may be sent to `url`: only to the organization's own host or its blob
    /// service host, over HTTPS. URLs that come from API responses (e.g. Container `contentLocation`)
    /// are checked with this, so a response naming another host cannot obtain the token.
    pub fn may_send_credential(&self, url: &Url) -> bool {
        credential_allowed(&self.org, &self.vsblob, url)
    }

    /// The `Authorization` header value to send to Azure DevOps (never to blob storage).
    pub fn auth_header(&self) -> String {
        self.cred.lock().unwrap_or_else(|e| e.into_inner()).header().to_string()
    }

    /// React to a 401/203 on a request that carried `stale`: renew an Azure CLI token (unless another
    /// task already did), or fail for credentials that cannot be renewed.
    pub async fn refresh_auth(&self, stale: &str) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.cred.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !current.refreshable() {
            return fatal(match current.source {
                auth::Source::EnvToken => "$ADO_TOKEN was rejected (expired or lacks permissions)".to_string(),
                _ => PAT_REJECTED.to_string(),
            });
        }
        if current.header() == stale {
            let fresh = auth::az_credential(current.tenant.as_deref().or(self.tenant.as_deref())).await?;
            *self.cred.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
        }
        Ok(())
    }

    /// GET (or POST with a JSON body) an API URL and return the parsed JSON response.
    pub async fn request(&self, url: &str, data: Option<&Value>) -> Result<Value> {
        const TRIES: u32 = 5;
        let place = url.split('?').next().unwrap_or(url);
        let target = Url::parse(url).or_else(|e| fatal(format!("invalid request URL {}: {e}", clean(place))))?;
        if !self.may_send_credential(&target) {
            return fatal(format!("refusing to send credentials to {} (not the organization's host)", clean(place)));
        }
        let body = data.map(|d| serde_json::to_vec(d).unwrap_or_default());
        for attempt in 1..=TRIES {
            let auth = self.auth_header();
            let mut rb = match &body {
                Some(b) => self.http.post(url).header("Content-Type", "application/json").body(b.clone()),
                None => self.http.get(url),
            };
            rb = rb.header("Accept", "application/json").header("Authorization", &auth);
            let res = async {
                let r = rb.send().await?;
                let status = r.status().as_u16();
                Ok::<_, reqwest::Error>((status, r.bytes().await?))
            }
            .await;
            let err = match res {
                Err(e) => {
                    if attempt == TRIES {
                        return fatal(format!("request to {place} failed: {}", net_err(e)));
                    }
                    net_err(e)
                }
                Ok((200, raw)) => {
                    return serde_json::from_slice(&raw)
                        .map_err(|e| Fatal(format!("invalid JSON from {place}: {e}")));
                }
                Ok((status, raw)) => {
                    if status == 203 || status == 401 {
                        // 203 = sign-in page: credential expired or rejected
                        self.refresh_auth(&auth).await?;
                        if attempt < TRIES {
                            continue;
                        }
                    }
                    if (status < 500 && status != 429) || attempt == TRIES {
                        let text: String = String::from_utf8_lossy(&raw).chars().take(500).collect();
                        return fatal(format!("HTTP {status} from {place}\n{text}"));
                    }
                    format!("HTTP {status}")
                }
            };
            warn(&format!("{place}: {err}; retrying ({attempt}/{})", TRIES - 1));
            tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
        }
        unreachable!("the last attempt always returns")
    }

    pub async fn api(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let mut p: Vec<(&str, String)> = params.to_vec();
        p.push(("api-version", "7.1".into()));
        let url = format!("{}/{}/_apis/{}?{}", self.org, quote(&self.project), path, urlencode(&p));
        self.request(&url, None).await
    }

    /// Read-only SAS URLs for blob ids (one request; callers batch by SAS_BATCH).
    pub async fn sas_urls(&self, ids: &[String]) -> Result<std::collections::HashMap<String, String>> {
        let url = format!("{}/_apis/dedup/urls?api-version=7.1-preview", self.vsblob);
        let body = Value::Array(ids.iter().map(|i| Value::String(i.clone())).collect());
        let out = self.request(&url, Some(&body)).await?;
        let mut map = std::collections::HashMap::with_capacity(ids.len());
        let mut missing: Vec<&String> = Vec::new();
        for id in ids {
            match out.get(id).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                Some(u) => {
                    map.insert(id.clone(), u.to_string());
                }
                None => missing.push(id),
            }
        }
        if !missing.is_empty() {
            return fatal(format!(
                "blob store has no URL for {} blob(s), e.g. {}",
                missing.len(),
                missing[0]
            ));
        }
        Ok(map)
    }
}

// --- target resolution --------------------------------------------------------------------

pub struct BuildUrl {
    pub org: String,
    pub project: String,
    pub build_id: u64,
}

/// Parse an ADO build results URL; `Ok(None)` if the target is not an http(s) URL.
pub fn parse_build_url(target: &str) -> Result<Option<BuildUrl>> {
    let Ok(u) = Url::parse(target) else { return Ok(None) };
    if u.scheme() != "http" && u.scheme() != "https" {
        return Ok(None);
    }
    let build_id = u.query_pairs().find(|(k, _)| k == "buildId").map(|(_, v)| v.into_owned());
    let parts: Vec<String> = u.path().split('/').filter(|p| !p.is_empty()).map(unquote).collect();
    let build_id = match build_id {
        Some(b) if !b.is_empty() && b.chars().all(|c| c.is_ascii_digit()) => b,
        _ => return fatal(format!("URL has no buildId parameter: {target}")),
    };
    let build_id: u64 = build_id.parse().or_else(|_| fatal(format!("bad buildId in URL: {target}")))?;
    let netloc = authority(&u);
    if netloc == "dev.azure.com" && parts.len() >= 2 {
        return Ok(Some(BuildUrl {
            org: format!("https://dev.azure.com/{}", parts[0]),
            project: parts[1].clone(),
            build_id,
        }));
    }
    if netloc.ends_with(".visualstudio.com") && !parts.is_empty() {
        return Ok(Some(BuildUrl { org: format!("https://{netloc}"), project: parts[0].clone(), build_id }));
    }
    fatal(format!("unrecognized Azure DevOps URL: {target}"))
}

/// Python's `str()` of an optional JSON field.
pub fn py(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(o) => o.to_string(),
    }
}

pub async fn resolve_pipeline(ado: &Ado, ident: &str) -> Result<(i64, String)> {
    if !ident.is_empty() && ident.chars().all(|c| c.is_ascii_digit()) {
        let d = ado.api(&format!("build/definitions/{ident}"), &[]).await?;
        return Ok((d["id"].as_i64().unwrap_or(0), py(d.get("name"))));
    }
    let defs = ado.api("build/definitions", &[("name", ident.to_string())]).await?;
    let defs = defs["value"].as_array().cloned().unwrap_or_default();
    if defs.is_empty() {
        return fatal(format!(
            "no pipeline named {} in {}/{} (exact name; * wildcards allowed)",
            repr(ident),
            ado.org,
            ado.project
        ));
    }
    if defs.len() > 1 {
        let lines: Vec<String> = defs
            .iter()
            .map(|d| {
                let path = d.get("path").and_then(Value::as_str).unwrap_or("").trim_end_matches('\\');
                format!("  {:>6}  {}\\{}", py(d.get("id")), path, py(d.get("name")))
            })
            .collect();
        return fatal(format!(
            "{} pipelines match {}; pass an ID instead:\n{}",
            defs.len(),
            repr(ident),
            lines.join("\n")
        ));
    }
    Ok((defs[0]["id"].as_i64().unwrap_or(0), py(defs[0].get("name"))))
}

pub fn describe(run: &Value) -> String {
    format!(
        "run {} ({}) of '{}', {}, status={}, result={}",
        py(run.get("id")),
        py(run.get("buildNumber")),
        py(run.get("definition").and_then(|d| d.get("name"))),
        py(run.get("sourceBranch")),
        py(run.get("status")),
        py(run.get("result")),
    )
}

/// Print why the run is not usable and exit with code 2.
pub async fn report_failure(ado: &Ado, run: &Value) -> ! {
    let web = clean(run.pointer("/_links/web/href").and_then(Value::as_str).unwrap_or(""));
    eprintln!("Latest {}", clean(&describe(run)));
    if run.get("status").and_then(Value::as_str) != Some("completed") {
        eprintln!("The run has not finished yet. {web}");
        std::process::exit(2);
    }
    eprintln!("The run did not succeed; not downloading. {web}");
    let records: Vec<Value> = match ado.api(&format!("build/builds/{}/timeline", py(run.get("id"))), &[]).await {
        Ok(t) => t.get("records").and_then(Value::as_array).cloned().unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let by_id: std::collections::HashMap<&str, &Value> =
        records.iter().filter_map(|r| r.get("id").and_then(Value::as_str).map(|i| (i, r))).collect();
    let bad = ["failed", "canceled", "succeededWithIssues"];
    let sget = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).map(str::to_string);
    let mut shown = 0;
    for r in &records {
        let result = sget(r, "result").unwrap_or_default();
        let typ = sget(r, "type").unwrap_or_default();
        if !bad.contains(&result.as_str()) || (typ != "Task" && typ != "Job") {
            continue;
        }
        let rid = sget(r, "id").unwrap_or_default();
        if typ == "Job"
            && records.iter().any(|c| {
                sget(c, "parentId").as_deref() == Some(rid.as_str())
                    && sget(c, "result").is_some_and(|x| bad.contains(&x.as_str()))
            })
        {
            continue; // the task inside it is more specific
        }
        let mut path = vec![sget(r, "name").unwrap_or_else(|| "?".into())];
        let mut p = sget(r, "parentId").and_then(|i| by_id.get(i.as_str()).copied());
        let mut guard = 0;
        while let Some(pr) = p {
            if sget(pr, "name").as_deref() != Some(path[0].as_str()) {
                path.insert(0, sget(pr, "name").unwrap_or_else(|| "?".into()));
            }
            p = sget(pr, "parentId").and_then(|i| by_id.get(i.as_str()).copied());
            guard += 1;
            if guard > 1000 {
                break;
            }
        }
        eprintln!("  ✗ {}: {}", clean(&path.join(" › ")), clean(&result));
        let issues = r.get("issues").and_then(Value::as_array).cloned().unwrap_or_default();
        for issue in issues.iter().take(5) {
            if sget(issue, "type").as_deref() == Some("error") || result == "succeededWithIssues" {
                let msg: String = sget(issue, "message").unwrap_or_default().trim().chars().take(300).collect();
                eprintln!("      {}", clean(&msg));
            }
        }
        shown += 1;
        if shown >= 10 {
            break;
        }
    }
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(org: &str, vsblob: &str, url: &str) -> bool {
        credential_allowed(org, vsblob, &Url::parse(url).unwrap())
    }

    #[test]
    fn credential_only_goes_to_the_organization() {
        let (org, blob) = ("https://dev.azure.com/contoso", "https://vsblob.dev.azure.com/contoso");
        assert!(allowed(org, blob, "https://dev.azure.com/contoso/_apis/resources/Containers/1?itemPath=a"));
        assert!(allowed(org, blob, "https://vsblob.dev.azure.com/contoso/_apis/dedup/urls"));
        assert!(!allowed(org, blob, "https://evil.example/_apis/resources/Containers/1"));
        assert!(!allowed(org, blob, "https://dev.azure.com.evil.example/contoso"));
        assert!(!allowed(org, blob, "http://dev.azure.com/contoso/_apis/x"));
        assert!(!allowed(org, blob, "https://dev.azure.com:8443/contoso/_apis/x"));
        assert!(!allowed(org, blob, "https://acct.blob.core.windows.net/c/b?sig=x"));
        let (org, blob) = ("https://contoso.visualstudio.com", "https://contoso.vsblob.visualstudio.com");
        assert!(allowed(org, blob, "https://contoso.visualstudio.com/_apis/x"));
        assert!(allowed(org, blob, "https://contoso.vsblob.visualstudio.com/_apis/dedup/urls"));
        assert!(!allowed(org, blob, "https://other.visualstudio.com/_apis/x"));
    }
}
