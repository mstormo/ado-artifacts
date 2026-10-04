//! Planning: output files, resume journals, the skip-state file, and node-tree mapping.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use futures_util::stream::{self, StreamExt};
use serde_json::Value;

use crate::ado::{Ado, SAS_BATCH};
use crate::blob::{BlobClient, BlobId, parse_node};
use crate::progress::{MAP_CHUNKS, MAP_NODES, MapSpinner, info};
use crate::util::*;

pub const STATE_FILE: &str = ".ado-artifacts.json"; // in the output dir: rel path -> id
const OLD_STATE_FILE: &str = ".ado-fast-artifacts.json"; // earlier name, still read if STATE_FILE is absent

pub enum Kind {
    Blob(BlobId),
    Container { url: String },
}

/// A file of the build to be written under the output directory.
pub struct Item {
    pub rel: String,
    pub final_path: PathBuf,
    pub id: String,
    pub size: u64,
    pub kind: Kind,
}

fn part_path(final_path: &Path, suffix: &str) -> PathBuf {
    let mut s = final_path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

impl Item {
    pub fn part(&self) -> PathBuf {
        part_path(&self.final_path, ".part")
    }

    pub fn journal(&self) -> PathBuf {
        part_path(&self.final_path, ".part.done")
    }
}

fn safe_rel(artifact: &str, path: &str) -> Result<String> {
    let rel = normpath(&format!("{artifact}/{}", path.trim_start_matches('/')));
    if rel.starts_with("..") || rel.starts_with('/') {
        return fatal(format!("refusing unsafe artifact path {}", repr(&rel)));
    }
    Ok(rel)
}

/// Destination of an EmptyDirectory manifest entry.
pub fn empty_dir_path(artifact: &str, path: &str, outdir: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(format!("{outdir}/{}", safe_rel(artifact, path)?)))
}

pub fn blob_item(artifact: &str, path: &str, blob_id: &str, size: u64, outdir: &str) -> Result<Item> {
    let rel = safe_rel(artifact, path)?;
    let id = BlobId::parse(&blob_id.to_ascii_uppercase())?;
    Ok(Item {
        final_path: PathBuf::from(format!("{outdir}/{rel}")),
        id: id.hex(),
        rel,
        size,
        kind: Kind::Blob(id),
    })
}

/// List the files of a Container artifact (resource.data is '#/<containerId>/<folder>').
pub async fn container_files(ado: &Ado, art: &Value, outdir: &str) -> Result<Vec<Item>> {
    let data = art["resource"]["data"].as_str().unwrap_or("");
    let mut it = data.splitn(3, '/');
    let (_, Some(cid), Some(folder)) = (it.next(), it.next(), it.next()) else {
        return fatal(format!("unexpected container artifact data {}", repr(data)));
    };
    let url = format!(
        "{}/_apis/resources/Containers/{cid}?itemPath={}&isShallow=false&api-version=5.0-preview",
        ado.org,
        quote(folder)
    );
    let listing = ado.request(&url, None).await?;
    let name = art["name"].as_str().unwrap_or("");
    let mut out = Vec::new();
    for i in listing.get("value").and_then(Value::as_array).cloned().unwrap_or_default() {
        if i.get("itemType").and_then(Value::as_str) != Some("file") {
            continue;
        }
        let path = i.get("path").and_then(Value::as_str).unwrap_or("");
        let rel = safe_rel(name, &relpath(path, folder))?;
        let size = match i.get("fileLength") {
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            Some(Value::String(s)) => s.parse().unwrap_or(0),
            _ => 0,
        };
        let cid_item = match i.get("containerId") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => cid.to_string(),
        };
        out.push(Item {
            final_path: PathBuf::from(format!("{outdir}/{rel}")),
            id: format!("container:{cid_item}:{path}"),
            rel,
            size,
            kind: Kind::Container { url: i.get("contentLocation").and_then(Value::as_str).unwrap_or("").to_string() },
        });
    }
    Ok(out)
}

// --- state file -----------------------------------------------------------------------------

/// Remembers which build file each output path holds, so reruns skip only truly current files.
pub struct State {
    path: PathBuf,
    inner: Mutex<StateInner>,
}

struct StateInner {
    ids: BTreeMap<String, String>,
    last_flush: Option<Instant>,
}

/// JSON formatter that escapes non-ASCII like Python's json.dump (ensure_ascii).
struct AsciiFormatter;

impl serde_json::ser::Formatter for AsciiFormatter {
    fn write_string_fragment<W: ?Sized + Write>(&mut self, w: &mut W, fragment: &str) -> std::io::Result<()> {
        let mut buf = [0u16; 2];
        for c in fragment.chars() {
            if c.is_ascii() {
                w.write_all(&[c as u8])?;
            } else {
                for u in c.encode_utf16(&mut buf) {
                    write!(w, "\\u{u:04x}")?;
                }
            }
        }
        Ok(())
    }
}

fn json_string(s: &str) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, AsciiFormatter);
    serde::Serialize::serialize(s, &mut ser).ok();
    String::from_utf8(out).unwrap_or_default()
}

impl State {
    pub fn load(outdir: &str) -> State {
        let path = PathBuf::from(format!("{outdir}/{STATE_FILE}"));
        let read_from = if path.exists() { path.clone() } else { PathBuf::from(format!("{outdir}/{OLD_STATE_FILE}")) };
        let ids = fs::read_to_string(&read_from)
            .ok()
            .and_then(|t| serde_json::from_str::<BTreeMap<String, String>>(&t).ok())
            .unwrap_or_default();
        State { path, inner: Mutex::new(StateInner { ids, last_flush: None }) }
    }

    pub fn is_current(&self, f: &Item) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.ids.get(&f.rel) == Some(&f.id)
            && fs::metadata(&f.final_path).is_ok_and(|m| m.is_file() && m.len() == f.size)
    }

    pub fn record(&self, f: &Item) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.ids.insert(f.rel.clone(), f.id.clone());
        if inner.last_flush.is_none_or(|t| t.elapsed().as_secs_f64() > 2.0) {
            self.flush_locked(&mut inner);
        }
    }

    pub fn flush(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.flush_locked(&mut inner);
    }

    fn flush_locked(&self, inner: &mut StateInner) {
        let mut text = String::from("{");
        for (i, (k, v)) in inner.ids.iter().enumerate() {
            text.push_str(if i == 0 { "\n" } else { ",\n" });
            text.push_str(&format!("{}: {}", json_string(k), json_string(v)));
        }
        text.push_str(if inner.ids.is_empty() { "}" } else { "\n}" });
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let tmp = part_path(&self.path, &format!(".{}.tmp", std::process::id()));
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, &self.path);
        }
        inner.last_flush = Some(Instant::now());
    }
}

// --- pipeline-artifact output files ---------------------------------------------------------

pub struct Handles {
    pub data: File,
    pub journal: File,
}

/// A pipeline-artifact file being assembled from chunks.
pub struct BlobFile {
    pub item: Item,
    pub blob_id: BlobId,
    pub remaining: AtomicU64,
    pub done_offsets: HashSet<u64>,
    handles: Mutex<Option<Arc<Handles>>>,
}

impl BlobFile {
    pub fn new(item: Item) -> BlobFile {
        let Kind::Blob(blob_id) = item.kind else { unreachable!("BlobFile needs a blob item") };
        let remaining = AtomicU64::new(item.size);
        BlobFile { item, blob_id, remaining, done_offsets: HashSet::new(), handles: Mutex::new(None) }
    }

    /// The (lazily opened) .part and journal handles. Opened on demand so that only files with
    /// chunks in flight hold file descriptors.
    pub fn handles(&self) -> std::io::Result<Arc<Handles>> {
        let mut g = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = g.as_ref() {
            return Ok(h.clone());
        }
        let data = OpenOptions::new().write(true).open(self.item.part())?;
        let journal = OpenOptions::new().append(true).open(self.item.journal())?;
        let h = Arc::new(Handles { data, journal });
        *g = Some(h.clone());
        Ok(h)
    }

    pub fn close(&self) {
        *self.handles.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Create .part files and load resume journals (written offsets) where they match.
pub fn open_outputs(files: &mut [BlobFile]) -> Result<()> {
    for f in files {
        if let Some(dir) = f.item.final_path.parent() {
            fs::create_dir_all(dir)?;
        }
        let (part, journal) = (f.item.part(), f.item.journal());
        let header = f.blob_id.raw();
        if part.exists() && journal.exists() && fs::metadata(&part)?.len() == f.item.size {
            let mut data = Vec::new();
            File::open(&journal)?.read_to_end(&mut data)?;
            if data.len() >= header.len() && data[..header.len()] == header {
                let body = &data[header.len()..];
                f.done_offsets = body
                    .chunks_exact(8)
                    .map(|c| u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
                    .collect();
            }
        }
        if f.done_offsets.is_empty() {
            for p in [&part, &journal] {
                if p.exists() {
                    fs::remove_file(p)?;
                }
            }
        }
        let data = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o644).open(&part)?;
        data.set_len(f.item.size)?;
        let new_journal = !journal.exists();
        let mut j = OpenOptions::new().append(true).create(true).mode(0o644).open(&journal)?;
        if new_journal {
            j.write_all(&header)?;
        }
    }
    Ok(())
}

pub fn finalize(f: &BlobFile, state: &State) -> std::io::Result<()> {
    f.close();
    fs::rename(f.item.part(), &f.item.final_path)?;
    fs::remove_file(f.item.journal())?;
    state.record(&f.item);
    Ok(())
}

// --- node-tree mapping ----------------------------------------------------------------------

pub type ChunkMap = HashMap<BlobId, (u64, Vec<(usize, u64)>)>;

/// Walk every file's node tree; return {chunk_id: (size, [(file index, offset), ...])}.
pub async fn map_chunks(files: &[BlobFile], ado: &Arc<Ado>, blobs: &Arc<BlobClient>, concurrency: usize) -> Result<ChunkMap> {
    let spinner = MapSpinner::start();
    let mut chunks: ChunkMap = HashMap::new();
    fn add_chunk(chunks: &mut ChunkMap, cid: BlobId, size: u64, fi: usize, off: u64) {
        chunks.entry(cid).or_insert_with(|| (size, Vec::new())).1.push((fi, off));
    }
    // (node hash, file index, base offset, size)
    let mut level: Vec<([u8; 32], usize, u64, u64)> = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        if f.item.size == 0 {
            continue;
        }
        match f.blob_id.alg {
            1 => add_chunk(&mut chunks, f.blob_id.with_alg(1), f.item.size, fi, 0),
            2 => level.push((f.blob_id.hash, fi, 0, f.item.size)),
            a => return fatal(format!("{}: unknown blob algorithm {}", f.item.rel, repr(&format!("{a:02X}")))),
        }
    }
    MAP_CHUNKS.store(chunks.len() as u64, std::sync::atomic::Ordering::Relaxed);
    while !level.is_empty() {
        let mut ids: Vec<BlobId> = level.iter().map(|l| BlobId { hash: l.0, alg: 2 }).collect();
        ids.sort_by_key(|i| i.hex());
        ids.dedup();
        let batches: Vec<Vec<String>> = ids.chunks(SAS_BATCH).map(|c| c.iter().map(BlobId::hex).collect()).collect();
        let mut sas: HashMap<String, String> = HashMap::new();
        let mut st = stream::iter(batches)
            .map(|b| {
                let ado = ado.clone();
                async move { ado.sas_urls(&b).await }
            })
            .buffer_unordered(concurrency);
        while let Some(r) = st.next().await {
            sas.extend(r?);
        }
        drop(st);
        let fetched: Vec<Result<(BlobId, bytes::Bytes)>> = stream::iter(ids.iter().copied())
            .map(|id| {
                let blobs = blobs.clone();
                let url = sas.get(&id.hex()).cloned().unwrap_or_default();
                async move {
                    let d = blobs.fetch(id, url, None, None).await?;
                    MAP_NODES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok((id, d))
                }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;
        let mut content: HashMap<[u8; 32], bytes::Bytes> = HashMap::with_capacity(ids.len());
        for r in fetched {
            let (id, d) = r?;
            content.insert(id.hash, d);
        }
        let mut next = Vec::new();
        for (h, fi, base, size) in level {
            let kids = parse_node(&content[&h])?;
            if kids.iter().map(|k| k.size).sum::<u64>() != size {
                return fatal(format!("{}: node {} sizes do not add up", files[fi].item.rel, &BlobId { hash: h, alg: 2 }.hex()[..64]));
            }
            let mut off = base;
            for k in kids {
                if k.is_node {
                    next.push((k.hash, fi, off, k.size));
                } else {
                    add_chunk(&mut chunks, BlobId { hash: k.hash, alg: 1 }, k.size, fi, off);
                }
                off += k.size;
            }
        }
        level = next;
        MAP_CHUNKS.store(chunks.len() as u64, std::sync::atomic::Ordering::Relaxed);
    }
    drop(spinner);
    if !files.is_empty() {
        info(&format!("Mapping files: done, {} unique chunks", thousands(&chunks.len().to_string())));
    }
    Ok(chunks)
}
