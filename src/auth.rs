//! Credentials: finding one that works (environment, Azure CLI, stored login), the connection
//! probe, and the `--login` / `--logout` commands. Secrets are never printed.

use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use reqwest::Client;
use serde_json::Value;

use crate::config::*;
use crate::util::*;

/// The Azure DevOps application ID (the `resource` of Azure AD tokens); a public Microsoft constant.
const ADO_RESOURCE: &str = "499b84ac-1321-427f-aa17-267ca6975798";

pub const PAT_REJECTED: &str = "PAT expired or lacks permissions — run ado-artifacts --login";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    EnvPat(&'static str),
    EnvToken,
    AzureCli,
    Stored,
}

impl Source {
    pub fn describe(self) -> String {
        match self {
            Source::EnvPat(var) => format!("personal access token from ${var}"),
            Source::EnvToken => "access token from $ADO_TOKEN".into(),
            Source::AzureCli => "Azure CLI access token".into(),
            Source::Stored => "stored login (ado-artifacts --login)".into(),
        }
    }
}

/// A value for the `Authorization` header together with where it came from.
#[derive(Clone)]
pub struct Credential {
    header: String,
    pub source: Source,
    /// Tenant the Azure CLI token was requested for (renewals use the same one).
    pub tenant: Option<String>,
}

impl Credential {
    pub fn pat(pat: &str, source: Source) -> Credential {
        Credential { header: basic_header(pat), source, tenant: None }
    }

    pub fn bearer(token: &str, source: Source) -> Credential {
        Credential { header: format!("Bearer {token}"), source, tenant: None }
    }

    pub fn header(&self) -> &str {
        &self.header
    }

    /// Only Azure CLI tokens can be renewed mid-run.
    pub fn refreshable(&self) -> bool {
        self.source == Source::AzureCli
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// HTTP Basic header for a personal access token: empty user name, the PAT as password.
pub fn basic_header(pat: &str) -> String {
    format!("Basic {}", base64(format!(":{pat}").as_bytes()))
}

pub fn build_http() -> Result<Client> {
    Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(60))
        .tcp_nodelay(true)
        .build()
        .map_err(|e| Fatal(format!("could not build HTTP client: {}", net_err(e))))
}

// --- probing --------------------------------------------------------------------------------------

/// Who the connection data says we are: `Display Name (unique.name@example.com)`.
fn user_label(v: &Value) -> Option<String> {
    let u = v.get("authenticatedUser")?;
    let id = u.get("id").and_then(Value::as_str)?;
    if id.is_empty() || id.chars().all(|c| c == '0' || c == '-') {
        return None; // the anonymous user
    }
    let name = ["customDisplayName", "providerDisplayName"]
        .iter()
        .find_map(|k| u.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()));
    let account = u.pointer("/properties/Account/$value").and_then(Value::as_str).filter(|s| !s.is_empty());
    Some(match (name, account) {
        (Some(n), Some(a)) if n != a => format!("{n} ({a})"),
        (Some(n), _) => n.to_string(),
        (None, Some(a)) => a.to_string(),
        (None, None) => id.to_string(),
    })
}

/// Check that `header` authenticates against the organization; `Ok` carries the user's label.
pub async fn probe(http: &Client, org: &str, header: &str) -> std::result::Result<String, String> {
    let url = format!("{}/_apis/connectionData?api-version=7.1-preview", org.trim_end_matches('/'));
    let r = http
        .get(&url)
        .header("Accept", "application/json")
        .header("Authorization", header)
        .send()
        .await
        .map_err(|e| format!("could not reach {org}: {}", net_err(e)))?;
    let status = r.status().as_u16();
    match status {
        200 => {
            let body = r.bytes().await.map_err(|e| format!("could not read the response: {}", net_err(e)))?;
            let v: Value = serde_json::from_slice(&body).map_err(|_| "got a sign-in page instead of data (not authenticated)".to_string())?;
            user_label(&v).ok_or_else(|| "not authenticated (anonymous access)".to_string())
        }
        401 => Err("HTTP 401 (rejected)".into()),
        203 => Err("HTTP 203 (sign-in page; rejected or expired)".into()),
        301 | 302 | 303 | 307 | 308 => Err("redirected to sign-in (rejected or expired)".into()),
        404 => Err("HTTP 404 (organization not found?)".into()),
        s => Err(format!("HTTP {s}")),
    }
}

/// The Azure AD tenant that owns the organization: an unauthenticated request is answered with it
/// in the `X-VSS-ResourceTenant` header.
async fn discover_tenant(http: &Client, org: &str) -> Option<String> {
    let url = format!("{}/_apis/connectionData?api-version=7.1-preview", org.trim_end_matches('/'));
    let r = http.get(&url).send().await.ok()?;
    let t = r.headers().get("X-VSS-ResourceTenant")?.to_str().ok()?.trim().to_string();
    (!t.is_empty() && t.chars().all(|c| c.is_ascii_hexdigit() || c == '-')).then_some(t)
}

// --- Azure CLI ---------------------------------------------------------------------------------------

/// Ask `az` for an Azure DevOps access token; `Err` is why it is unavailable.
pub async fn az_token(tenant: Option<&str>) -> std::result::Result<String, String> {
    let mut cmd = tokio::process::Command::new("az");
    cmd.args(["account", "get-access-token", "--resource", ADO_RESOURCE]);
    if let Some(t) = tenant {
        cmd.args(["--tenant", t]);
    }
    cmd.args(["--query", "accessToken", "-o", "tsv"]).stdin(std::process::Stdio::null());
    match cmd.output().await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("the az CLI is not installed (not on PATH)".into()),
        Err(e) => Err(format!("could not run az: {e}")),
        Ok(o) if !o.status.success() => {
            let err = String::from_utf8_lossy(&o.stderr);
            let line = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no error output");
            Err(format!("`az account get-access-token` failed: {}", line.chars().take(300).collect::<String>()))
        }
        Ok(o) => match String::from_utf8_lossy(&o.stdout).trim() {
            "" => Err("`az account get-access-token` returned no token".into()),
            t => Ok(t.to_string()),
        },
    }
}

/// A fresh Azure CLI credential (used to renew a token mid-run).
pub async fn az_credential(tenant: Option<&str>) -> Result<Credential> {
    match az_token(tenant).await {
        Ok(t) => Ok(Credential { tenant: tenant.map(str::to_string), ..Credential::bearer(&t, Source::AzureCli) }),
        Err(e) => fatal(format!("could not renew the Azure CLI access token: {e}")),
    }
}

// --- finding a working credential -------------------------------------------------------------------

/// Try each credential source in order and return the first that authenticates against `org`.
pub async fn resolve_credential(http: &Client, org: &str, tenant: Option<&str>, stored: Option<&Stored>) -> Result<Credential> {
    let mut tried: Vec<String> = Vec::new();

    for var in ["AZURE_DEVOPS_EXT_PAT", "ADO_PAT"] {
        match env_nonempty(var) {
            None => tried.push(format!("${var}: not set")),
            Some(pat) => {
                let c = Credential::pat(&pat, Source::EnvPat(var));
                match probe(http, org, c.header()).await {
                    Ok(_) => return Ok(c),
                    Err(e) => tried.push(format!("${var}: {}", e)),
                }
            }
        }
    }

    match env_nonempty("ADO_TOKEN") {
        None => tried.push("$ADO_TOKEN: not set".into()),
        Some(t) => {
            let c = Credential::bearer(&t, Source::EnvToken);
            match probe(http, org, c.header()).await {
                Ok(_) => return Ok(c),
                Err(e) => tried.push(format!("$ADO_TOKEN: {}", e)),
            }
        }
    }

    match az_token(tenant).await {
        Err(e) => tried.push(format!("Azure CLI: {e}")),
        Ok(t) => {
            let c = Credential::bearer(&t, Source::AzureCli);
            match probe(http, org, c.header()).await {
                Ok(_) => return Ok(c),
                Err(e) => {
                    // The default tenant of `az` may not be the one owning the organization; the
                    // service names the right one when asked without credentials.
                    let mut note = format!("Azure CLI: token obtained but {e}");
                    if tenant.is_none()
                        && let Some(t) = discover_tenant(http, org).await
                    {
                        match az_token(Some(&t)).await {
                            Ok(t2) => {
                                let c = Credential { tenant: Some(t.clone()), ..Credential::bearer(&t2, Source::AzureCli) };
                                match probe(http, org, c.header()).await {
                                    Ok(_) => return Ok(c),
                                    Err(e2) => note = format!("Azure CLI: token for tenant {t} obtained but {e2}"),
                                }
                            }
                            Err(_) => note.push_str(&format!("; the organization belongs to tenant {t}: run `az login --tenant {t}`")),
                        }
                    }
                    tried.push(note);
                }
            }
        }
    }

    match stored {
        None => tried.push("stored login: none (run ado-artifacts --login)".into()),
        Some(s) if org_key(&s.organization) != org_key(org) => {
            tried.push(format!("stored login: saved for a different organization ({})", s.organization));
        }
        Some(s) => {
            let c = Credential::pat(&s.pat, Source::Stored);
            match probe(http, org, c.header()).await {
                Ok(_) => return Ok(c),
                Err(e) => tried.push(format!("stored login: {} (PAT expired? run ado-artifacts --login again)", e)),
            }
        }
    }

    fatal(format!(
        "no working credentials for {org}; tried:\n{}\nRun `ado-artifacts --login` to sign in with a personal access token.",
        tried.iter().map(|t| format!("  - {t}")).collect::<Vec<_>>().join("\n")
    ))
}

// --- --login / --logout -----------------------------------------------------------------------------

/// The `--login` instructions (stdout).
pub fn login_steps(org: &str) -> String {
    format!(
        "To log in to {org}, create a personal access token (PAT) for your account:\n\
         \n  \
         1. Open {} and sign in.\n  \
         2. Click \"New Token\".\n  \
         3. Name: ado-artifacts (any name).\n  \
         4. Organization: {}.\n  \
         5. Expiration: choose a date (at most 1 year).\n  \
         6. Scopes: select \"Custom defined\", then enable: {PAT_SCOPES}\n  \
         7. Click \"Create\" and copy the token (it is shown only once).\n  \
         8. Paste it below.\n",
        pat_url(org),
        org_name(org)
    )
}

fn read_line_blocking() -> std::io::Result<String> {
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(s)
}

#[cfg(unix)]
static SAVED_TERMIOS: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);

/// Turn terminal echo back on if a hidden prompt was interrupted.
pub fn restore_terminal() {
    #[cfg(unix)]
    if let Some(t) = SAVED_TERMIOS.lock().unwrap_or_else(|e| e.into_inner()).take() {
        // SAFETY: restores the attributes saved by `read_secret_blocking` on the same descriptor.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    }
}

/// Read a line from stdin; with echo off when stdin is a terminal.
fn read_secret_blocking() -> std::io::Result<String> {
    #[cfg(unix)]
    if std::io::stdin().is_terminal() {
        // SAFETY: tcgetattr/tcsetattr on fd 0 with a zeroed local termios.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) == 0 {
                *SAVED_TERMIOS.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
                let mut quiet = t;
                quiet.c_lflag &= !libc::ECHO;
                libc::tcsetattr(0, libc::TCSANOW, &quiet);
                let line = read_line_blocking();
                restore_terminal();
                eprintln!();
                return line;
            }
        }
    }
    read_line_blocking()
}

async fn read_line() -> Result<String> {
    match tokio::task::spawn_blocking(read_line_blocking).await {
        Ok(Ok(l)) => Ok(l),
        Ok(Err(e)) => fatal(format!("could not read from stdin: {e}")),
        Err(e) => fatal(format!("could not read from stdin: {e}")),
    }
}

async fn read_secret() -> Result<String> {
    match tokio::task::spawn_blocking(read_secret_blocking).await {
        Ok(Ok(l)) => Ok(l),
        Ok(Err(e)) => fatal(format!("could not read from stdin: {e}")),
        Err(e) => fatal(format!("could not read from stdin: {e}")),
    }
}

/// Log in with a personal access token and save it.
pub async fn login(flag_org: Option<String>, flag_project: Option<String>) -> Result<()> {
    let dir = store_dir()?;
    let stored = load_stored_from(&dir);
    let (org, _) = pick_org_project(flag_org.as_deref(), None, azure_defaults(), stored.as_ref())?;
    let org = match org {
        Some(o) => o,
        None => {
            eprint!("Azure DevOps organization (URL or name): ");
            let _ = std::io::stderr().flush();
            normalize_org(&read_line().await?)?
        }
    };

    let mut out = std::io::stdout();
    let _ = writeln!(out, "{}", login_steps(&org));
    let _ = out.flush();
    if std::io::stdin().is_terminal() {
        eprint!("Personal access token (input hidden): ");
        let _ = std::io::stderr().flush();
    }
    let pat = read_secret().await?;
    let pat = pat.trim();
    if pat.is_empty() {
        return fatal("no token entered; nothing saved");
    }

    let http = build_http()?;
    let who = match probe(&http, &org, &basic_header(pat)).await {
        Ok(w) => w,
        Err(e) => return fatal(format!("login to {org} failed: {}; nothing saved. Check the token, its organization and its scopes ({PAT_SCOPES}).", e)),
    };

    let project = flag_project
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .or_else(|| stored.as_ref().filter(|s| org_key(&s.organization) == org_key(&org)).and_then(|s| s.project.clone()));
    let path = save_stored_to(&dir, &Stored { organization: org.clone(), project: project.clone(), pat: pat.to_string(), created: now_iso8601() })?;
    println!("Logged in to {org} as {}", clean(&who));
    if let Some(p) = project {
        println!("Default project: {p}");
    }
    println!("Saved to {}", path.display());
    Ok(())
}

/// Delete the stored login.
pub fn logout() -> Result<()> {
    let dir = store_dir()?;
    match delete_stored_from(&dir)? {
        None => println!("No stored credentials at {}; nothing to remove.", credentials_path(&dir).display()),
        Some(s) if s.organization.is_empty() => println!("Removed {}", credentials_path(&dir).display()),
        Some(s) => println!("Removed stored credentials for {} ({})", s.organization, credentials_path(&dir).display()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn basic_header_format() {
        // ":pat" -> OnBhdA==
        assert_eq!(basic_header("pat"), "Basic OnBhdA==");
        assert_eq!(basic_header(""), "Basic Og==");
        let long = basic_header("abcdefghijklmnopqrstuvwxyz0123456789");
        assert_eq!(long, "Basic OmFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6MDEyMzQ1Njc4OQ==");
    }

    #[test]
    fn credential_headers() {
        assert_eq!(Credential::pat("pat", Source::Stored).header(), "Basic OnBhdA==");
        assert_eq!(Credential::bearer("tok", Source::EnvToken).header(), "Bearer tok");
        assert!(Credential::bearer("t", Source::AzureCli).refreshable());
        assert!(!Credential::bearer("t", Source::EnvToken).refreshable());
        assert!(!Credential::pat("t", Source::Stored).refreshable());
    }

    #[test]
    fn user_labels() {
        let v = serde_json::json!({"authenticatedUser": {"id": "1a2b", "providerDisplayName": "Ada Lovelace",
            "properties": {"Account": {"$type": "System.String", "$value": "ada@example.com"}}}});
        assert_eq!(user_label(&v).as_deref(), Some("Ada Lovelace (ada@example.com)"));
        let v = serde_json::json!({"authenticatedUser": {"id": "1a2b"}});
        assert_eq!(user_label(&v).as_deref(), Some("1a2b"));
        let anon = serde_json::json!({"authenticatedUser": {"id": "00000000-0000-0000-0000-000000000000"}});
        assert_eq!(user_label(&anon), None);
        assert_eq!(user_label(&serde_json::json!({})), None);
    }

    #[test]
    fn steps_have_the_right_url_and_scopes() {
        let s = login_steps("https://dev.azure.com/contoso");
        assert!(s.contains("https://dev.azure.com/contoso/_usersSettings/tokens"));
        assert!(s.contains("Organization: contoso."));
        assert!(s.contains(PAT_SCOPES));
        let s = login_steps("https://contoso.visualstudio.com");
        assert!(s.contains("https://contoso.visualstudio.com/_usersSettings/tokens"));
        assert!(s.contains("Organization: contoso."));
    }

    #[test]
    fn readme_documents_the_scopes() {
        assert!(include_str!("../README.md").contains(PAT_SCOPES));
    }
}
