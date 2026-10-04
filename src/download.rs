//! The download phase: chunk pipeline for pipeline artifacts, file-level fetch for containers.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use flate2::write::GzDecoder;
use futures_util::stream::{self, StreamExt};
use reqwest::Url;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ado::{Ado, SAS_BATCH};
use crate::blob::{BlobClient, BlobId, CHUNK_TRIES, Sink};
use crate::plan::{BlobFile, ChunkMap, Item, Kind, State, finalize};
use crate::progress::{FRAME, Progress, RETRIES};
use crate::util::*;

/// Aborts the wrapped task when dropped, so nothing outlives a stop.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type Need = Vec<(usize, u64)>;
type Work = (BlobId, String, u64, Need);

pub struct Ctx {
    pub ado: Arc<Ado>,
    pub blobs: Arc<BlobClient>,
    pub state: Arc<State>,
    pub concurrency: usize,
}

/// Fetch pipeline-artifact chunks (blob level) and Container-artifact files (file level) in
/// parallel. Dropping the returned future stops everything.
pub async fn download(ctx: &Ctx, files: Vec<BlobFile>, chunks: ChunkMap, cfiles: Vec<Item>) -> Result<()> {
    let files = Arc::new(files);
    // (chunk id, size, placements still needed)
    let mut todo: Vec<(BlobId, u64, Need)> = Vec::new();
    let mut resumed = 0u64;
    for (cid, (size, places)) in chunks {
        let need: Need = places.iter().copied().filter(|(fi, off)| !files[*fi].done_offsets.contains(off)).collect();
        resumed += size * (places.len() - need.len()) as u64;
        if !need.is_empty() {
            todo.push((cid, size, need));
        }
    }
    let mut remaining: HashMap<usize, u64> = HashMap::new();
    for (_, size, need) in &todo {
        for (fi, _) in need {
            *remaining.entry(*fi).or_default() += size;
        }
    }
    for (fi, f) in files.iter().enumerate() {
        f.remaining.store(remaining.get(&fi).copied().unwrap_or(0), Relaxed);
    }
    // Process in file/offset order so files complete one after another.
    todo.sort_by_key(|(_, _, need)| need[0]);

    let total = files.iter().map(|f| f.item.size).sum::<u64>() + cfiles.iter().map(|c| c.size).sum::<u64>();
    let progress = Arc::new(Progress::new(total, (files.len() + cfiles.len()) as u64, resumed, (todo.len() + cfiles.len()) as u64));
    for f in files.iter().filter(|f| f.remaining.load(Relaxed) == 0) {
        finalize(f, &ctx.state)?;
        progress.add(0, true);
    }

    let ticker = {
        let progress = progress.clone();
        AbortOnDrop(tokio::spawn(async move {
            let mut iv = tokio::time::interval(FRAME);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                iv.tick().await;
                progress.tick();
            }
        }))
    };

    let res = tokio::try_join!(
        chunk_pipeline(ctx, &files, todo, &progress),
        container_pipeline(ctx, cfiles, &progress)
    );
    drop(ticker);
    progress.finish();
    ctx.state.flush();
    match res {
        Ok(_) => Ok(()),
        Err(e) => fatal(format!("{e}\nPartial files kept; rerun the same command to resume.")),
    }
}

async fn chunk_pipeline(ctx: &Ctx, files: &Arc<Vec<BlobFile>>, todo: Vec<(BlobId, u64, Need)>, progress: &Arc<Progress>) -> Result<()> {
    if todo.is_empty() {
        return Ok(());
    }
    // Producer: fetch SAS URLs a few batches ahead of the workers so they never starve.
    let (tx, mut rx) = mpsc::channel::<Result<Work>>(ctx.concurrency * 4);
    let batches: Vec<Vec<(BlobId, u64, Need)>> = {
        let mut v = Vec::new();
        let mut it = todo.into_iter().peekable();
        while it.peek().is_some() {
            v.push(it.by_ref().take(SAS_BATCH).collect());
        }
        v
    };
    let ado = ctx.ado.clone();
    let _producer = AbortOnDrop(tokio::spawn(async move {
        let mut fetched = stream::iter(batches)
            .map(|batch| {
                let ado = ado.clone();
                async move {
                    let ids: Vec<String> = batch.iter().map(|(c, _, _)| c.hex()).collect();
                    ado.sas_urls(&ids).await.map(|sas| (batch, sas))
                }
            })
            .buffered(3);
        while let Some(r) = fetched.next().await {
            match r {
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
                Ok((batch, mut sas)) => {
                    for (cid, size, need) in batch {
                        let url = sas.remove(&cid.hex()).unwrap_or_default();
                        if tx.send(Ok((cid, url, size, need))).await.is_err() {
                            return; // consumer stopped
                        }
                    }
                }
            }
        }
    }));

    let items = stream::unfold(&mut rx, |rx| async move { rx.recv().await.map(|i| (i, rx)) });
    let workers = items
        .map(|item| {
            let (blobs, files, state, progress) = (ctx.blobs.clone(), files.clone(), ctx.state.clone(), progress.clone());
            async move {
                let (cid, url, size, need) = item?;
                let sink: Sink = Arc::new(move |data: &[u8]| write_chunk(&files, &state, &progress, &need, size, data));
                blobs.fetch(cid, url, Some(size), Some(sink)).await.map(|_| ())
            }
        })
        .buffer_unordered(ctx.concurrency);
    let mut workers = std::pin::pin!(workers);
    while let Some(r) = workers.next().await {
        r?;
    }
    Ok(())
}

/// Write verified chunk data at every placement; journal each; finish files that complete.
fn write_chunk(files: &[BlobFile], state: &State, progress: &Progress, need: &Need, size: u64, data: &[u8]) -> Result<()> {
    for &(fi, off) in need {
        let f = &files[fi];
        let ctx = |e: std::io::Error| Fatal(format!("{}: {e}", f.item.rel));
        let h = f.handles().map_err(ctx)?;
        h.data.write_all_at(data, off).map_err(ctx)?;
        (&h.journal).write_all(&off.to_le_bytes()).map_err(ctx)?;
        drop(h);
        let complete = f.remaining.fetch_sub(size, Ordering::AcqRel) == size;
        if complete {
            finalize(f, state).map_err(ctx)?;
        }
        progress.add(size, complete);
    }
    progress.block_done();
    Ok(())
}

async fn container_pipeline(ctx: &Ctx, cfiles: Vec<Item>, progress: &Arc<Progress>) -> Result<()> {
    if cfiles.is_empty() {
        return Ok(());
    }
    let n = ctx.concurrency.min(16);
    let mut st = stream::iter(cfiles)
        .map(|cf| {
            let (ado, state, progress) = (ctx.ado.clone(), ctx.state.clone(), progress.clone());
            async move {
                fetch_container_file(&ado, &cf, &progress).await?;
                state.record(&cf);
                progress.add(0, true);
                progress.block_done();
                Ok::<(), Fatal>(())
            }
        })
        .buffer_unordered(n);
    while let Some(r) = st.next().await {
        r?;
    }
    Ok(())
}

// --- container files ------------------------------------------------------------------------

enum Msg {
    Data(Bytes),
    End,
}

struct Counting {
    out: std::io::BufWriter<std::fs::File>,
    got: Arc<AtomicU64>,
    progress: Arc<Progress>,
}

impl Write for Counting {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.out.write(buf)?;
        self.got.fetch_add(n as u64, Relaxed);
        self.progress.add(n as u64, false);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

enum Out {
    Raw(Counting),
    Gz(GzDecoder<Counting>),
}

impl Out {
    fn push(&mut self, b: &[u8]) -> std::io::Result<()> {
        match self {
            Out::Raw(w) => w.write_all(b),
            Out::Gz(w) => w.write_all(b),
        }
    }

    fn finish(self) -> std::io::Result<()> {
        let mut c = match self {
            Out::Raw(c) => c,
            Out::Gz(g) => g.finish()?,
        };
        c.flush()
    }
}

/// Blocking side of a container-file download: decode (gzip) and write the streamed body to
/// `.part`, then rename it into place. Anything short of an explicit End is an aborted download.
fn sink_task(part: &std::path::Path, final_path: &std::path::Path, size: u64, gz: bool, mut rx: mpsc::Receiver<Msg>, progress: &Arc<Progress>) -> std::result::Result<(), String> {
    let got = Arc::new(AtomicU64::new(0));
    let r = (|| -> std::result::Result<(), String> {
        if let Some(dir) = part.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let file = std::fs::File::create(part).map_err(|e| e.to_string())?;
        let c = Counting { out: std::io::BufWriter::with_capacity(1 << 20, file), got: got.clone(), progress: progress.clone() };
        let mut out = if gz { Out::Gz(GzDecoder::new(c)) } else { Out::Raw(c) };
        loop {
            match rx.blocking_recv() {
                Some(Msg::Data(b)) => out.push(&b).map_err(|e| e.to_string())?,
                Some(Msg::End) => break,
                None => return Err("aborted".into()),
            }
        }
        out.finish().map_err(|e| e.to_string())?;
        let n = got.load(Relaxed);
        if n != size {
            return Err(format!("got {n} bytes, expected {size}"));
        }
        std::fs::rename(part, final_path).map_err(|e| e.to_string())
    })();
    if r.is_err() {
        progress.sub(got.load(Relaxed));
    }
    r
}

/// Marks a download error that retrying cannot fix (the credential was rejected).
const FATAL_AUTH: &str = "\u{1}auth:";

async fn fetch_container_file(ado: &Arc<Ado>, cf: &Item, progress: &Arc<Progress>) -> Result<()> {
    let Kind::Container { url } = &cf.kind else { return fatal("not a container file") };
    let mut last = String::new();
    for attempt in 1..=CHUNK_TRIES {
        match try_container(ado, cf, url, progress).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if let Some(msg) = e.strip_prefix(FATAL_AUTH) {
                    return fatal(msg);
                }
                last = e;
                RETRIES.fetch_add(1, Relaxed);
                if attempt < CHUNK_TRIES {
                    let secs = (0.5 * 2f64.powi(attempt as i32)).min(15.0);
                    tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                }
            }
        }
    }
    fatal(format!("{} failed after {CHUNK_TRIES} tries: {last}", cf.rel))
}

async fn try_container(ado: &Arc<Ado>, cf: &Item, start_url: &str, progress: &Arc<Progress>) -> std::result::Result<(), String> {
    let auth = ado.auth_header();
    let mut url = Url::parse(start_url).map_err(|e| format!("bad content URL: {e}"))?;
    // The content URL comes from the API response: only send the credential if it points at the
    // organization itself (never to another host, and never after a redirect).
    let mut with_token = ado.may_send_credential(&url);
    let mut resp = None;
    for _ in 0..4 {
        let mut rb = ado.http.get(url.clone()).header("Accept-Encoding", "gzip");
        if with_token {
            rb = rb.header("Authorization", &auth);
        }
        let r = rb.send().await.map_err(net_err)?;
        if !matches!(r.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            resp = Some(r);
            break;
        }
        let loc = r.headers().get("Location").and_then(|v| v.to_str().ok()).ok_or("redirect without Location")?.to_string();
        url = url.join(&loc).map_err(|e| e.to_string())?;
        with_token = false; // redirects go to storage; don't leak the credential
    }
    let Some(mut r) = resp else { return Err("too many redirects".into()) };
    let status = r.status().as_u16();
    if status == 203 || status == 401 {
        ado.refresh_auth(&auth).await.map_err(|e| format!("{FATAL_AUTH}{}", e.0))?;
        return Err(format!("HTTP {status} (credential refreshed)"));
    }
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    // The service sends these gzip-compressed regardless; fileLength is the decoded size.
    let gz = r.headers().get("Content-Encoding").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    let (tx, rx) = mpsc::channel::<Msg>(8);
    let (part, final_path, size, prog) = (cf.part(), cf.final_path.clone(), cf.size, progress.clone());
    let writer = tokio::task::spawn_blocking(move || sink_task(&part, &final_path, size, gz, rx, &prog));
    let net: std::result::Result<(), String> = async {
        while let Some(chunk) = r.chunk().await.map_err(net_err)? {
            if tx.send(Msg::Data(chunk)).await.is_err() {
                break; // the writer failed; its error is reported below
            }
        }
        Ok(())
    }
    .await;
    if net.is_ok() {
        let _ = tx.send(Msg::End).await;
    }
    drop(tx);
    let written = writer.await.map_err(|e| format!("writer failed: {e}"))?;
    net?;
    written
}
