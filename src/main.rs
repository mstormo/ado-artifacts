//! Fast, parallel, hash-verified downloader for Azure DevOps pipeline artifacts.

mod ado;
mod auth;
mod blob;
mod config;
mod download;
mod plan;
mod progress;
mod util;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use clap::Parser;
use serde_json::Value;

use ado::*;
use blob::{BlobClient, BlobId};
use config::*;
use plan::*;
use util::*;

const AFTER_HELP: &str = "\
Examples:
  ado-artifacts 'https://dev.azure.com/contoso/MyProject/_build/results?buildId=1234'
  ado-artifacts Nightly-Build                 # latest run of the pipeline, any branch
  ado-artifacts 219 -a manifest -o ~/Downloads
  ado-artifacts --login                       # one-time sign-in with a personal access token

TARGET is either a build results URL (downloads that exact run's artifacts), or a pipeline
name/ID. For a pipeline, the latest run is used regardless of branch; if that run did not
succeed, its failures are reported (exit code 2) and nothing is downloaded.

Files land in <out>/<artifact>/<path>. Interrupted downloads resume where they left off, and
reruns skip files already downloaded from the same build (tracked in <out>/.ado-artifacts.json).
Classic build (\"Container\") artifacts are fetched file by file, many files in parallel.

Organization and project (first match wins, each on its own):
  1. a build results URL as TARGET (it names both)
  2. --org / --project (--org takes a URL or a bare name)
  3. environment ADO_ORG / ADO_PROJECT
  4. Azure CLI devops defaults (`az devops configure --defaults organization=... project=...`),
     read from ~/.azure/azuredevops/config ($AZURE_CONFIG_DIR/azuredevops/config if set)
  5. what `--login` saved in ~/.ado-artifacts

Credentials (the first one that authenticates against the organization is used):
  1. environment AZURE_DEVOPS_EXT_PAT or ADO_PAT (personal access token)
  2. environment ADO_TOKEN (Azure AD access token)
  3. Azure CLI: `az account get-access-token` (current `az login`; --tenant/ADO_TENANT selects a tenant)
  4. the personal access token saved by `--login`, if saved for the same organization
Azure CLI tokens are renewed automatically when they expire; a PAT is not.

--login prints the steps to create a personal access token (scope: Build: Read) for your
account, reads the token (hidden on a terminal; from stdin when piped), checks it and saves it
in ~/.ado-artifacts/credentials.json (directory 0700, file 0600; $ADO_ARTIFACTS_HOME overrides
the directory). --logout deletes it. Tokens are never printed.";

#[derive(Parser)]
#[command(
    name = "ado-artifacts",
    version,
    about = "Fast, parallel, hash-verified downloader for Azure DevOps pipeline artifacts.",
    after_help = AFTER_HELP
)]
struct Args {
    /// build results URL, or pipeline name/ID
    #[arg(required_unless_present_any = ["login", "logout"], conflicts_with_all = ["login", "logout"])]
    target: Option<String>,
    /// only this artifact (repeatable; default: all)
    #[arg(short = 'a', long = "artifact")]
    artifact: Vec<String>,
    /// output directory
    #[arg(short = 'o', long = "out", default_value = ".")]
    out: String,
    /// max concurrent downloads (default: 128)
    #[arg(short = 'j', long = "concurrency", value_name = "N", default_value_t = 128, hide_default_value = true, value_parser = positive_int)]
    concurrency: usize,
    /// no progress bar or informational output (warnings and errors only)
    #[arg(short = 'q', long = "quiet", conflicts_with = "verbose")]
    quiet: bool,
    /// also show "X/Y blocks" at the start of the second progress line
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,
    /// list artifacts and files, don't download
    #[arg(short = 'l', long = "list")]
    list: bool,
    /// re-download files that already exist
    #[arg(long = "force")]
    force: bool,
    /// organization: URL (https://dev.azure.com/NAME, https://NAME.visualstudio.com) or bare name
    #[arg(long = "org", env = "ADO_ORG", value_name = "URL|NAME")]
    org: Option<String>,
    /// project (for a pipeline name/ID; with --login: saved as the default project)
    #[arg(long = "project", env = "ADO_PROJECT", value_name = "NAME")]
    project: Option<String>,
    /// Azure AD tenant for the Azure CLI token (default: the az default)
    #[arg(long = "tenant", env = "ADO_TENANT", value_name = "ID")]
    tenant: Option<String>,
    /// sign in with a personal access token and save it in ~/.ado-artifacts
    #[arg(long = "login", conflicts_with = "logout")]
    login: bool,
    /// delete the saved login
    #[arg(long = "logout")]
    logout: bool,
}

fn positive_int(v: &str) -> std::result::Result<usize, String> {
    let n: i64 = v.parse().map_err(|_| format!("invalid integer value: '{v}'"))?;
    if n < 1 { Err("must be at least 1".into()) } else { Ok(n as usize) }
}

static STATE: OnceLock<Arc<State>> = OnceLock::new();

fn main() {
    raise_nofile_limit();
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: could not start the async runtime: {e}");
            std::process::exit(1);
        }
    };
    let args = Args::parse();
    progress::init(args.quiet, args.verbose);
    let code = rt.block_on(async move {
        tokio::select! {
            r = run(args) => match r {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("error: {}", clean(&e.to_string()));
                    1
                }
            },
            code = wait_for_signal() => {
                auth::restore_terminal();
                if let Some(s) = STATE.get() {
                    s.flush();
                }
                let at_line_start = progress::interrupted();
                eprintln!("{}Interrupted; rerun the same command to resume.", if at_line_start { "" } else { "\n" });
                code
            }
        }
    });
    // Do not wait for in-flight blocking work or lingering connections.
    std::process::exit(code);
}

async fn wait_for_signal() -> i32 {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut int), Ok(mut term)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = int.recv() => 130,
        _ = term.recv() => 143,
    }
}

fn size_of(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

async fn run(args: Args) -> Result<()> {
    if args.login {
        return auth::login(args.org, args.project).await;
    }
    if args.logout {
        return auth::logout();
    }
    let Some(target) = args.target.clone() else { return fatal("missing TARGET") };
    let from_url = parse_build_url(&target)?;
    let stored = load_stored();
    let (org, project) = match &from_url {
        Some(b) => (b.org.clone(), b.project.clone()),
        None => {
            let (org, project) = pick_org_project(args.org.as_deref(), args.project.as_deref(), azure_defaults(), stored.as_ref())?;
            match (org, project) {
                (Some(o), Some(p)) => (o, p),
                (o, p) => return fatal(missing_org_project_msg(o.is_none(), p.is_none())),
            }
        }
    };
    let ado = Ado::new(&org, &project, args.tenant.as_deref().filter(|t| !t.is_empty()), stored.as_ref()).await?;
    progress::info(&format!("Authenticated to {} with {}", ado.org, ado.credential_source()));

    let run: Value;
    if let Some(b) = &from_url {
        run = ado.api(&format!("build/builds/{}", b.build_id), &[]).await?;
        progress::info(&format!("Using {}", describe(&run)));
        if run.get("result").and_then(Value::as_str) != Some("succeeded") {
            warn(&format!(
                "this run did not succeed (result={}); its published artifacts are {} anyway",
                py(run.get("result")),
                if args.list { "listed" } else { "downloaded" }
            ));
        }
    } else {
        let (def_id, def_name) = resolve_pipeline(&ado, &target).await?;
        let runs = ado
            .api(
                "build/builds",
                &[("definitions", def_id.to_string()), ("queryOrder", "queueTimeDescending".into()), ("$top", "1".into())],
            )
            .await?;
        let Some(first) = runs.get("value").and_then(Value::as_array).and_then(|v| v.first()) else {
            return fatal(format!("pipeline {def_id} '{def_name}' has no runs"));
        };
        run = first.clone();
        if run.get("status").and_then(Value::as_str) != Some("completed")
            || run.get("result").and_then(Value::as_str) != Some("succeeded")
        {
            report_failure(&ado, &run).await;
        }
        progress::info(&format!("Using latest {}", describe(&run)));
    }

    let run_id = py(run.get("id"));
    let mut artifacts: Vec<Value> =
        ado.api(&format!("build/builds/{run_id}/artifacts"), &[]).await?["value"].as_array().cloned().unwrap_or_default();
    if !args.artifact.is_empty() {
        let by_name: HashMap<String, Value> =
            artifacts.iter().map(|a| (py(a.get("name")), a.clone())).collect();
        let missing: Vec<&str> = args.artifact.iter().filter(|n| !by_name.contains_key(*n)).map(String::as_str).collect();
        if !missing.is_empty() {
            let mut avail: Vec<&String> = by_name.keys().collect();
            avail.sort();
            return fatal(format!(
                "run {run_id} has no artifact(s) {}; available: {}",
                missing.join(", "),
                avail.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        let mut seen = std::collections::HashSet::new();
        artifacts = args.artifact.iter().filter(|n| seen.insert(n.as_str())).map(|n| by_name[n].clone()).collect();
    }
    if artifacts.is_empty() {
        return fatal(format!("run {run_id} has no artifacts"));
    }

    let outdir = abspath(&expand_user(&args.out));
    let blobs = Arc::new(BlobClient::new(ado.clone(), args.concurrency)?);
    let mut items: Vec<Item> = Vec::new();
    let mut empty_dirs: Vec<PathBuf> = Vec::new();
    for art in &artifacts {
        let name = py(art.get("name"));
        let res = &art["resource"];
        let rtype = res.get("type").and_then(Value::as_str).unwrap_or("");
        let new_items: Vec<Item> = match rtype {
            "PipelineArtifact" => {
                let root = BlobId::parse(&py(res.get("data")).to_ascii_uppercase())?;
                let raw = blobs.read_all(root).await?;
                let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&raw);
                let manifest: Value =
                    serde_json::from_slice(raw).map_err(|e| Fatal(format!("artifact {} has an invalid manifest: {e}", repr(&name))))?;
                if manifest.get("manifestReferences").is_some_and(|v| match v {
                    Value::Null => false,
                    Value::Array(a) => !a.is_empty(),
                    Value::Object(o) => !o.is_empty(),
                    Value::String(s) => !s.is_empty(),
                    Value::Bool(b) => *b,
                    Value::Number(n) => n.as_f64() != Some(0.0),
                }) {
                    return fatal(format!("artifact {} uses nested manifests, which this tool does not handle", repr(&name)));
                }
                let mut v = Vec::new();
                for i in manifest.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
                    let path = py(i.get("path"));
                    if let Some(b) = i.get("blob") {
                        v.push(blob_item(&name, &path, &py(b.get("id")), size_of(b.get("size")), &outdir)?);
                    } else if i.get("type").and_then(Value::as_str) == Some("EmptyDirectory") {
                        empty_dirs.push(empty_dir_path(&name, &path, &outdir)?);
                    } else {
                        warn(&format!(
                            "{name}: skipping manifest entry {} of type {}",
                            repr(&path),
                            if i.get("type").is_some() { repr(&py(i.get("type"))) } else { "None".into() }
                        ));
                    }
                }
                v
            }
            "Container" => container_files(&ado, art, &outdir).await?,
            other => {
                warn(&format!("skipping artifact {}: unsupported type {}", repr(&name), repr(other)));
                continue;
            }
        };
        if args.list {
            println!("{}  ({}, {} file(s), {})", clean(&name), mb(new_items.iter().map(|i| i.size).sum()), new_items.len(), clean(rtype));
            for i in &new_items {
                println!("  {:>12}  {}", mb(i.size), clean(&i.rel.to_string()));
            }
        }
        items.extend(new_items);
    }
    if args.list {
        return Ok(());
    }

    for d in &empty_dirs {
        std::fs::create_dir_all(d)?;
    }
    let state = Arc::new(State::load(&outdir));
    let _ = STATE.set(state.clone());
    let (mut todo, mut ctodo, mut skipped): (Vec<Item>, Vec<Item>, usize) = (Vec::new(), Vec::new(), 0);
    for f in items {
        if !args.force && state.is_current(&f) {
            skipped += 1;
        } else if f.size == 0 {
            if let Some(dir) = f.final_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::File::create(&f.final_path)?;
            state.record(&f);
        } else if matches!(f.kind, Kind::Container { .. }) {
            ctodo.push(f);
        } else {
            todo.push(f);
        }
    }
    state.flush();
    if skipped > 0 {
        progress::info(&format!("{skipped} file(s) already downloaded from this build, skipped (--force re-downloads)"));
    }
    if todo.is_empty() && ctodo.is_empty() {
        progress::info("Nothing to download.");
        return Ok(());
    }

    let n = todo.len() + ctodo.len();
    let total: u64 = todo.iter().chain(ctodo.iter()).map(|f| f.size).sum();
    progress::info(&format!("Downloading {n} file(s), {} → {outdir}", mb(total)));
    let mut files: Vec<BlobFile> = todo.into_iter().map(BlobFile::new).collect();
    let chunks = map_chunks(&files, &ado, &blobs, args.concurrency.min(32)).await?;
    open_outputs(&mut files)?;
    let ctx = download::Ctx { ado, blobs, state, concurrency: args.concurrency };
    download::download(&ctx, files, chunks, ctodo).await?;
    progress::info(&format!("Done: {n} file(s) in {outdir}"));
    Ok(())
}
