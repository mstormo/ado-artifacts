//! Where the organization, the project and a saved login come from: org URL parsing, the Azure CLI
//! devops defaults (`~/.azure/azuredevops/config`) and the credentials file of `--login`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use serde_json::{Value, json};

use crate::util::*;

/// Personal Access Token scopes the tool needs. Used by the `--login` instructions and the README.
pub const PAT_SCOPES: &str = "Build: Read";

/// Overrides the directory of the stored login (default `~/.ado-artifacts`).
pub const STORE_ENV: &str = "ADO_ARTIFACTS_HOME";

const STORE_DIR: &str = ".ado-artifacts";
const CREDENTIALS_FILE: &str = "credentials.json";

pub fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .find_map(|k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from))
}

pub fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

// --- organization URLs -------------------------------------------------------------------------

/// Normalize an organization given as a URL or a bare name: `https://dev.azure.com/<org>` or
/// `https://<org>.visualstudio.com` (no trailing slash, no further path).
pub fn normalize_org(input: &str) -> Result<String> {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return fatal("the organization is empty");
    }
    let bare = !s.contains(['/', '.', ':']);
    if bare {
        if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return fatal(format!("invalid organization name {}", repr(s)));
        }
        return Ok(format!("https://dev.azure.com/{s}"));
    }
    let with_scheme = if s.contains("://") { s.to_string() } else { format!("https://{s}") };
    let u = Url::parse(&with_scheme).or_else(|_| fatal(format!("invalid organization URL {}", repr(s))))?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return fatal(format!("invalid organization URL {} (expected http or https)", repr(s)));
    }
    let host = u.host_str().unwrap_or("").to_string();
    let first = u.path().split('/').find(|p| !p.is_empty()).map(unquote);
    if host == "dev.azure.com" {
        return match first {
            Some(o) => Ok(format!("https://dev.azure.com/{}", quote(&o))),
            None => fatal(format!("organization URL {} has no organization name", repr(s))),
        };
    }
    if host.ends_with(".visualstudio.com") && host.len() > ".visualstudio.com".len() {
        return Ok(format!("https://{host}"));
    }
    // Credentials (PATs, Azure CLI tokens) are sent to the organization, so only the Azure DevOps
    // Services hosts are accepted: any other host would receive them.
    fatal(format!(
        "unsupported organization {}: expected https://dev.azure.com/<org>, https://<org>.visualstudio.com or a bare \
         organization name (Azure DevOps Server / on-premises is not supported)",
        repr(s)
    ))
}

/// Case-insensitive comparison key of an organization.
pub fn org_key(org: &str) -> String {
    normalize_org(org).unwrap_or_else(|_| org.trim().trim_end_matches('/').to_string()).to_lowercase()
}

/// Short organization name: `contoso` for both URL forms, the URL itself for anything else.
pub fn org_name(org: &str) -> String {
    let Ok(u) = Url::parse(org) else { return org.to_string() };
    let host = u.host_str().unwrap_or("");
    if host == "dev.azure.com" {
        if let Some(first) = u.path().split('/').find(|p| !p.is_empty()) {
            return unquote(first);
        }
    } else if let Some(name) = host.strip_suffix(".visualstudio.com") {
        return name.to_string();
    }
    org.to_string()
}

/// Page where the signed-in user creates a personal access token for this organization.
pub fn pat_url(org: &str) -> String {
    format!("{}/_usersSettings/tokens", org.trim_end_matches('/'))
}

// --- Azure CLI devops defaults ---------------------------------------------------------------------

/// `[defaults]` `organization` and `project` of an Azure CLI devops config file (INI).
pub fn parse_azure_defaults(text: &str) -> (Option<String>, Option<String>) {
    let (mut org, mut project) = (None, None);
    let mut in_defaults = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_defaults = name.trim().eq_ignore_ascii_case("defaults");
            continue;
        }
        if !in_defaults {
            continue;
        }
        let Some(pos) = line.find(['=', ':']) else { continue };
        let (key, value) = (line[..pos].trim().to_ascii_lowercase(), line[pos + 1..].trim());
        if value.is_empty() {
            continue;
        }
        match key.as_str() {
            "organization" => org = Some(value.to_string()),
            "project" => project = Some(value.to_string()),
            _ => {}
        }
    }
    (org, project)
}

/// `$AZURE_CONFIG_DIR/azuredevops/config`, else `~/.azure/azuredevops/config`.
pub fn azure_config_file(config_dir_env: Option<&str>, home: Option<&Path>) -> Option<PathBuf> {
    let dir = match config_dir_env.filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => home?.join(".azure"),
    };
    Some(dir.join("azuredevops").join("config"))
}

pub fn azure_defaults() -> (Option<String>, Option<String>) {
    let env = std::env::var("AZURE_CONFIG_DIR").ok();
    let Some(path) = azure_config_file(env.as_deref(), home_dir().as_deref()) else { return (None, None) };
    std::fs::read_to_string(path).map(|t| parse_azure_defaults(&t)).unwrap_or((None, None))
}

// --- stored login --------------------------------------------------------------------------------

/// What `--login` saves.
#[derive(PartialEq)]
pub struct Stored {
    pub organization: String,
    pub project: Option<String>,
    pub pat: String,
    pub created: String,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored")
            .field("organization", &self.organization)
            .field("project", &self.project)
            .field("pat", &"<redacted>")
            .field("created", &self.created)
            .finish()
    }
}

impl Stored {
    fn to_json(&self) -> Value {
        let mut v = json!({ "organization": self.organization, "pat": self.pat, "created": self.created });
        if let Some(p) = &self.project {
            v["project"] = json!(p);
        }
        v
    }

    fn from_json(v: &Value) -> Option<Stored> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        Some(Stored {
            organization: s("organization")?,
            project: s("project"),
            pat: s("pat")?,
            created: s("created").unwrap_or_default(),
        })
    }
}

/// `$ADO_ARTIFACTS_HOME`, else `~/.ado-artifacts`.
pub fn store_dir_from(env: Option<&str>, home: Option<&Path>) -> Option<PathBuf> {
    match env.filter(|d| !d.is_empty()) {
        Some(d) => Some(PathBuf::from(d)),
        None => Some(home?.join(STORE_DIR)),
    }
}

pub fn store_dir() -> Result<PathBuf> {
    let env = std::env::var(STORE_ENV).ok();
    match store_dir_from(env.as_deref(), home_dir().as_deref()) {
        Some(d) => Ok(d),
        None => fatal(format!("cannot locate the home directory; set {STORE_ENV}")),
    }
}

pub fn credentials_path(dir: &Path) -> PathBuf {
    dir.join(CREDENTIALS_FILE)
}

/// Read the stored login; a missing or unreadable file is `None`.
pub fn load_stored_from(dir: &Path) -> Option<Stored> {
    let path = credentials_path(dir);
    let text = std::fs::read_to_string(&path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(&path)
            && m.permissions().mode() & 0o077 != 0
        {
            warn(&format!(
                "{} is accessible by other users (mode {:03o}); run `chmod 600` on it",
                path.display(),
                m.permissions().mode() & 0o777
            ));
        }
    }
    match serde_json::from_str::<Value>(&text).ok().and_then(|v| Stored::from_json(&v)) {
        Some(s) => Some(s),
        None => {
            warn(&format!("ignoring {}: not a valid credentials file", path.display()));
            None
        }
    }
}

pub fn load_stored() -> Option<Stored> {
    load_stored_from(&store_dir().ok()?)
}

/// Write the stored login atomically: directory mode 0700, file mode 0600.
pub fn save_stored_to(dir: &Path, s: &Stored) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let path = credentials_path(dir);
        let tmp = dir.join(format!("{CREDENTIALS_FILE}.{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            let mut text = serde_json::to_string_pretty(&s.to_json()).unwrap_or_default();
            text.push('\n');
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_file(&tmp);
            return fatal(format!("could not write {}: {e}", path.display()));
        }
        Ok(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)?;
        let path = credentials_path(dir);
        let tmp = dir.join(format!("{CREDENTIALS_FILE}.{}.tmp", std::process::id()));
        let mut text = serde_json::to_string_pretty(&s.to_json()).unwrap_or_default();
        text.push('\n');
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }
}

/// Delete the stored login; `Ok(None)` if there was none.
pub fn delete_stored_from(dir: &Path) -> Result<Option<Stored>> {
    let path = credentials_path(dir);
    if !path.exists() {
        return Ok(None);
    }
    let old = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    std::fs::remove_file(&path).or_else(|e| fatal(format!("could not remove {}: {e}", path.display())))?;
    Ok(Some(old.as_ref().and_then(Stored::from_json).unwrap_or(Stored {
        organization: String::new(),
        project: None,
        pat: String::new(),
        created: String::new(),
    })))
}

/// Current time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now_iso8601() -> String {
    iso8601_utc(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
}

pub fn iso8601_utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // civil-from-days (proleptic Gregorian calendar)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

// --- organization and project resolution -----------------------------------------------------------

/// First match wins, per value: flag or environment (clap resolves both), Azure CLI defaults,
/// the stored login. A stored project is only used for the organization it was saved with.
pub fn pick_org_project(
    flag_org: Option<&str>,
    flag_project: Option<&str>,
    azure: (Option<String>, Option<String>),
    stored: Option<&Stored>,
) -> Result<(Option<String>, Option<String>)> {
    let nonempty = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let raw_org = nonempty(flag_org).or(nonempty(azure.0.as_deref())).or_else(|| stored.map(|s| s.organization.clone()));
    let org = raw_org.as_deref().map(normalize_org).transpose()?;
    let project = nonempty(flag_project).or(nonempty(azure.1.as_deref())).or_else(|| {
        let (s, o) = (stored?, org.as_deref()?);
        if org_key(&s.organization) == org_key(o) { s.project.clone() } else { None }
    });
    Ok((org, project))
}

pub fn missing_org_project_msg(missing_org: bool, missing_project: bool) -> String {
    let what = match (missing_org, missing_project) {
        (true, true) => "organization and project are",
        (true, false) => "organization is",
        _ => "project is",
    };
    format!(
        "a pipeline name or ID needs an organization and a project, but the {what} unknown. Options:\n  \
         --org <url-or-name> --project <name>\n  \
         the ADO_ORG and ADO_PROJECT environment variables\n  \
         az devops configure --defaults organization=https://dev.azure.com/<org> project=<project>\n  \
         ado-artifacts --login [--project <name>]   (saves it for later runs)\n\
         Or pass a build results URL, which needs no further settings."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_forms() {
        let want = "https://dev.azure.com/contoso";
        for i in [
            "contoso",
            " contoso ",
            "https://dev.azure.com/contoso",
            "https://dev.azure.com/contoso/",
            "http://dev.azure.com/contoso",
            "dev.azure.com/contoso",
            "https://dev.azure.com/contoso/MyProject/_build/results?buildId=1",
            "HTTPS://DEV.AZURE.COM/contoso",
        ] {
            assert_eq!(normalize_org(i).unwrap(), want, "{i}");
        }
        for i in ["https://contoso.visualstudio.com", "https://contoso.visualstudio.com/", "contoso.visualstudio.com", "https://contoso.visualstudio.com/MyProject"] {
            assert_eq!(normalize_org(i).unwrap(), "https://contoso.visualstudio.com", "{i}");
        }
        // Credentials go to the organization: other hosts are refused.
        assert!(normalize_org("https://tfs.example.com:8080/tfs/Coll/").is_err());
        assert!(normalize_org("http://evil.example/contoso").is_err());
        assert!(normalize_org("https://dev.azure.com.evil.example/contoso").is_err());
        assert!(normalize_org("https://.visualstudio.com").is_err());
        assert!(normalize_org("").is_err());
        assert!(normalize_org("https://dev.azure.com/").is_err());
        assert!(normalize_org("ftp://dev.azure.com/x").is_err());
        assert!(normalize_org("bad name").is_err());
    }

    #[test]
    fn org_names_and_keys() {
        assert_eq!(org_name("https://dev.azure.com/contoso"), "contoso");
        assert_eq!(org_name("https://contoso.visualstudio.com"), "contoso");
        assert_eq!(org_key("Contoso"), org_key("https://DEV.azure.com/contoso/"));
        assert_ne!(org_key("contoso"), org_key("https://contoso.visualstudio.com"));
    }

    #[test]
    fn pat_urls() {
        assert_eq!(pat_url("https://dev.azure.com/contoso"), "https://dev.azure.com/contoso/_usersSettings/tokens");
        assert_eq!(pat_url("https://contoso.visualstudio.com"), "https://contoso.visualstudio.com/_usersSettings/tokens");
    }

    #[test]
    fn azure_ini() {
        let t = "[core]\norganization = wrong\n\n# comment\n[defaults]\n; another\nOrganization = https://dev.azure.com/contoso\nproject=My Project\nother = x\n[later]\nproject = nope\n";
        assert_eq!(
            parse_azure_defaults(t),
            (Some("https://dev.azure.com/contoso".into()), Some("My Project".into()))
        );
        assert_eq!(parse_azure_defaults("[defaults]\norganization =\n"), (None, None));
        assert_eq!(parse_azure_defaults("organization = x\n"), (None, None));
        assert_eq!(parse_azure_defaults("[defaults]\r\nproject = P\r\n"), (None, Some("P".into())));
    }

    #[test]
    fn azure_config_location() {
        let home = Path::new("/home/u");
        assert_eq!(azure_config_file(None, Some(home)), Some(PathBuf::from("/home/u/.azure/azuredevops/config")));
        assert_eq!(azure_config_file(Some(""), Some(home)), Some(PathBuf::from("/home/u/.azure/azuredevops/config")));
        assert_eq!(azure_config_file(Some("/etc/az"), Some(home)), Some(PathBuf::from("/etc/az/azuredevops/config")));
        assert_eq!(azure_config_file(Some("/etc/az"), None), Some(PathBuf::from("/etc/az/azuredevops/config")));
        assert_eq!(azure_config_file(None, None), None);
    }

    #[test]
    fn store_location() {
        let home = Path::new("/home/u");
        assert_eq!(store_dir_from(None, Some(home)), Some(PathBuf::from("/home/u/.ado-artifacts")));
        assert_eq!(store_dir_from(Some("/x/y"), Some(home)), Some(PathBuf::from("/x/y")));
        assert_eq!(store_dir_from(None, None), None);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ado-artifacts-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn credentials_round_trip_and_modes() {
        let dir = temp_dir("rt").join("nested").join("store");
        let s = Stored {
            organization: "https://dev.azure.com/contoso".into(),
            project: Some("MyProject".into()),
            pat: "secret-pat".into(),
            created: "2026-01-02T03:04:05Z".into(),
        };
        assert!(load_stored_from(&dir).is_none());
        let path = save_stored_to(&dir, &s).unwrap();
        assert_eq!(path, dir.join("credentials.json"));
        assert_eq!(load_stored_from(&dir), Some(s));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // overwrite without a project; no stray temp files remain
        let s2 = Stored { organization: "https://x.visualstudio.com".into(), project: None, pat: "p2".into(), created: String::new() };
        save_stored_to(&dir, &s2).unwrap();
        let loaded = load_stored_from(&dir).unwrap();
        assert_eq!((loaded.organization.as_str(), loaded.project, loaded.pat.as_str()), ("https://x.visualstudio.com", None, "p2"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert!(delete_stored_from(&dir).unwrap().is_some());
        assert!(delete_stored_from(&dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn stored_debug_hides_pat() {
        let s = Stored { organization: "o".into(), project: None, pat: "topsecret".into(), created: String::new() };
        assert!(!format!("{s:?}").contains("topsecret"));
    }

    #[test]
    fn malformed_credentials_are_ignored() {
        let dir = temp_dir("bad");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(credentials_path(&dir), "{\"organization\": \"x\"}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(credentials_path(&dir), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(load_stored_from(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn iso_timestamps() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601_utc(1_767_225_599), "2025-12-31T23:59:59Z");
        assert_eq!(iso8601_utc(1_781_000_000), "2026-06-09T10:13:20Z");
    }

    #[test]
    fn org_project_precedence() {
        let stored = Stored {
            organization: "https://dev.azure.com/saved".into(),
            project: Some("SavedProj".into()),
            pat: "p".into(),
            created: String::new(),
        };
        let az = || (Some("https://dev.azure.com/azorg".to_string()), Some("AzProj".to_string()));
        // flags beat Azure CLI defaults beat the stored login, per value
        let r = pick_org_project(Some("flagorg"), Some("FlagProj"), az(), Some(&stored)).unwrap();
        assert_eq!(r, (Some("https://dev.azure.com/flagorg".into()), Some("FlagProj".into())));
        let r = pick_org_project(None, None, az(), Some(&stored)).unwrap();
        assert_eq!(r, (Some("https://dev.azure.com/azorg".into()), Some("AzProj".into())));
        let r = pick_org_project(None, None, (None, None), Some(&stored)).unwrap();
        assert_eq!(r, (Some("https://dev.azure.com/saved".into()), Some("SavedProj".into())));
        // a stored project does not leak into another organization
        let r = pick_org_project(Some("other"), None, (None, None), Some(&stored)).unwrap();
        assert_eq!(r, (Some("https://dev.azure.com/other".into()), None));
        let r = pick_org_project(Some("SAVED"), None, (None, None), Some(&stored)).unwrap();
        assert_eq!(r.1.as_deref(), Some("SavedProj"));
        assert_eq!(pick_org_project(None, None, (None, None), None).unwrap(), (None, None));
        assert!(pick_org_project(Some("bad name"), None, (None, None), None).is_err());
    }

    #[test]
    fn missing_message_lists_options() {
        let m = missing_org_project_msg(true, true);
        for needle in ["--org", "ADO_ORG", "az devops configure", "--login"] {
            assert!(m.contains(needle), "{needle}");
        }
    }
}
