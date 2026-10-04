//! Blob store access: ids, node parsing, xpress decoding, and the connection-grouping fetcher.
//!
//! Blob ids are a 32-byte hash plus one algorithm byte: ...01 = chunk, ...02 = node.
//!   chunk id = SHA-512(chunk bytes)[:32]; node id = SHA-512(serialized node)[:32].
//! A serialized node is b"\0\0" + uint16le(child_count - 1) followed by the children in file
//! order, each either
//!   0x01 + uint56le(size) + hash32   an inner node covering `size` bytes, or
//!   0x00 + uint24le(size) + hash32   a chunk of `size` bytes.
//! Stored blobs may be compressed: "Content-Encoding: xpress" is MS-XCA Plain LZ77.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sha2::{Digest, Sha512};
use tokio::sync::OnceCell;

use crate::ado::Ado;
use crate::progress::RETRIES;
use crate::util::*;

pub const MAX_NODE_SIZE: usize = 1 << 20; // decode cap for blobs of unknown size
pub const CHUNK_TRIES: u32 = 10; // per blob / container file; backoff capped at 15 s
const BLOB_SUFFIX: &str = ".blob.core.windows.net";

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BlobId {
    pub hash: [u8; 32],
    pub alg: u8,
}

impl BlobId {
    pub fn parse(s: &str) -> Result<BlobId> {
        let b = s.as_bytes();
        if b.len() != 66 {
            return fatal(format!("malformed blob id {s:?}"));
        }
        let mut raw = [0u8; 33];
        for (i, pair) in b.chunks(2).enumerate() {
            let (Some(h), Some(l)) = ((pair[0] as char).to_digit(16), (pair[1] as char).to_digit(16)) else {
                return fatal(format!("malformed blob id {s:?}"));
            };
            raw[i] = (h * 16 + l) as u8;
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&raw[..32]);
        Ok(BlobId { hash, alg: raw[32] })
    }

    pub fn with_alg(&self, alg: u8) -> BlobId {
        BlobId { hash: self.hash, alg }
    }

    /// The raw 33 bytes (hash + algorithm), as stored in resume journals.
    pub fn raw(&self) -> [u8; 33] {
        let mut r = [0u8; 33];
        r[..32].copy_from_slice(&self.hash);
        r[32] = self.alg;
        r
    }

    /// Uppercase hex, the form the blob store API uses.
    pub fn hex(&self) -> String {
        self.raw().iter().map(|b| format!("{b:02X}")).collect()
    }
}

pub struct Kid {
    pub is_node: bool,
    pub size: u64,
    pub hash: [u8; 32],
}

pub fn parse_node(data: &[u8]) -> Result<Vec<Kid>> {
    if data.len() < 4 || data[..2] != [0, 0] {
        return fatal("unrecognized node format");
    }
    let le = |b: &[u8]| b.iter().rev().fold(0u64, |a, &x| (a << 8) | x as u64);
    let mut kids = Vec::new();
    let mut o = 4;
    while o < data.len() {
        let (is_node, size_len, total) = match data[o] {
            1 if o + 40 <= data.len() => (true, 7, 40),
            0 if o + 36 <= data.len() => (false, 3, 36),
            t => return fatal(format!("unrecognized node entry type {t} at offset {o}")),
        };
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&data[o + 1 + size_len..o + total]);
        kids.push(Kid { is_node, size: le(&data[o + 1..o + 1 + size_len]), hash });
        o += total;
    }
    if le(&data[2..4]) as i64 != kids.len() as i64 - 1 {
        return fatal("node child count mismatch");
    }
    Ok(kids)
}

/// MS-XCA Plain LZ77. Stops at `out_len` or when the input runs out (returning what was
/// produced so far); `Err` if the data is malformed.
pub fn xpress_lz77(inp: &[u8], out_len: usize) -> std::result::Result<Vec<u8>, &'static str> {
    let n = inp.len();
    let mut out: Vec<u8> = Vec::with_capacity(out_len);
    let (mut ip, mut flags, mut nflags) = (0usize, 0u32, 0u32);
    let mut half: Option<usize> = None;
    while out.len() < out_len {
        if nflags == 0 {
            if ip + 4 > n {
                break;
            }
            flags = u32::from_le_bytes([inp[ip], inp[ip + 1], inp[ip + 2], inp[ip + 3]]);
            ip += 4;
            nflags = 32;
        }
        nflags -= 1;
        if (flags >> nflags) & 1 == 0 {
            if ip >= n {
                break;
            }
            out.push(inp[ip]);
            ip += 1;
            continue;
        }
        if ip + 2 > n {
            break;
        }
        let mv = inp[ip] as usize | (inp[ip + 1] as usize) << 8;
        ip += 2;
        let (mut len, off) = (mv & 7, (mv >> 3) + 1);
        if len == 7 {
            match half.take() {
                None => {
                    if ip >= n {
                        return Err("truncated");
                    }
                    len = (inp[ip] & 15) as usize;
                    half = Some(ip);
                    ip += 1;
                }
                Some(h) => len = (inp[h] >> 4) as usize,
            }
            if len == 15 {
                if ip >= n {
                    return Err("truncated");
                }
                len = inp[ip] as usize;
                ip += 1;
                if len == 255 {
                    if ip + 2 > n {
                        return Err("truncated");
                    }
                    len = inp[ip] as usize | (inp[ip + 1] as usize) << 8;
                    ip += 2;
                    if len == 0 {
                        if ip + 4 > n {
                            return Err("truncated");
                        }
                        len = u32::from_le_bytes([inp[ip], inp[ip + 1], inp[ip + 2], inp[ip + 3]]) as usize;
                        ip += 4;
                    }
                    if len < 15 + 7 {
                        return Err("bad length");
                    }
                    len -= 15 + 7;
                }
                len += 15;
            }
            len += 7;
        }
        len += 3;
        let op = out.len();
        if off > op || len > out_len - op {
            return Err("bad match");
        }
        let start = op - off;
        if off >= len {
            out.extend_from_within(start..start + len);
        } else {
            for k in 0..len {
                let b = out[start + k];
                out.push(b);
            }
        }
    }
    Ok(out)
}

// --- DNS: one IP per host, remembered, so connections can be grouped per IP ------------------

#[derive(Default)]
struct DnsCache {
    map: Mutex<HashMap<String, Arc<OnceCell<IpAddr>>>>,
}

impl DnsCache {
    async fn ip(&self, host: &str) -> std::io::Result<IpAddr> {
        let cell = {
            let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
            m.entry(host.to_string()).or_default().clone()
        };
        cell.get_or_try_init(|| async {
            tokio::net::lookup_host((host, 443))
                .await?
                .next()
                .map(|a| a.ip())
                .ok_or_else(|| std::io::Error::other(format!("no address for {host}")))
        })
        .await
        .copied()
    }
}

struct CachedResolver(Arc<DnsCache>);

impl Resolve for CachedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let cache = self.0.clone();
        Box::pin(async move {
            let ip = cache.ip(name.as_str()).await?;
            Ok(Box::new(std::iter::once(SocketAddr::new(ip, 0))) as Addrs)
        })
    }
}

/// Called with the verified, decoded bytes of a chunk (on a blocking thread).
pub type Sink = Arc<dyn Fn(&[u8]) -> Result<()> + Send + Sync>;

enum Attempt {
    Retry(String),
    Fatal(Fatal),
}

/// Fetches blobs from SAS URLs over pooled keep-alive connections, decodes and verifies them.
///
/// Chunks are spread over ~100 storage accounts (<x>vsblobprodcus<n>.blob.core.windows.net) that
/// resolve to a handful of front-end IPs. A connection per account quickly runs into connection
/// refusals, so every request goes to one canonical host name per IP (the first account seen for
/// it) and names the real account in the Host header (the front ends serve every account, and the
/// TLS cert is *.blob.core.windows.net). The pool is keyed by that canonical host, so the number
/// of connections follows the request concurrency, not concurrency x accounts.
pub struct BlobClient {
    ado: Arc<Ado>,
    http: reqwest::Client,
    dns: Arc<DnsCache>,
    canon: Mutex<HashMap<IpAddr, String>>,
}

impl BlobClient {
    pub fn new(ado: Arc<Ado>, concurrency: usize) -> Result<BlobClient> {
        let dns = Arc::new(DnsCache::default());
        let http = reqwest::Client::builder()
            .http1_only() // the Host-header trick needs HTTP/1.1 (HTTP/2 would send :authority)
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(CachedResolver(dns.clone())))
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host((concurrency / 8).max(4))
            .pool_idle_timeout(Duration::from_secs(30))
            .tcp_nodelay(true)
            .build()
            .map_err(|e| Fatal(format!("could not build HTTP client: {}", net_err(e))))?;
        Ok(BlobClient { ado, http, dns, canon: Mutex::new(HashMap::new()) })
    }

    async fn get(&self, url: &str) -> std::result::Result<(u16, Option<String>, Bytes), String> {
        let mut u = Url::parse(url).map_err(|e| format!("bad blob URL: {e}"))?;
        let host = u.host_str().unwrap_or("").to_string();
        let host_header = match u.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.clone(),
        };
        if host.ends_with(BLOB_SUFFIX) {
            let ip = self.dns.ip(&host).await.map_err(|e| format!("resolving {host}: {e}"))?;
            let canon = {
                let mut m = self.canon.lock().unwrap_or_else(|e| e.into_inner());
                m.entry(ip).or_insert_with(|| host.clone()).clone()
            };
            u.set_host(Some(&canon)).map_err(|e| e.to_string())?;
        }
        let r = self
            .http
            .get(u)
            .header("Host", host_header)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .map_err(net_err)?;
        let status = r.status().as_u16();
        let enc = r.headers().get("Content-Encoding").and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = r.bytes().await.map_err(net_err)?;
        Ok((status, enc, body))
    }

    /// Return the verified content of a blob. `size` is required for chunks, None for nodes.
    /// `sink`, if given, is called once with the verified chunk data (on a blocking thread).
    pub async fn fetch(&self, id: BlobId, mut url: String, size: Option<u64>, sink: Option<Sink>) -> Result<Bytes> {
        let mut last = String::new();
        for attempt in 1..=CHUNK_TRIES {
            match self.attempt(id, &mut url, size, &sink).await {
                Ok(data) => return Ok(data),
                Err(Attempt::Fatal(e)) => return Err(e),
                Err(Attempt::Retry(e)) => {
                    last = e;
                    RETRIES.fetch_add(1, Relaxed);
                    if attempt < CHUNK_TRIES {
                        let secs = (0.5 * 2f64.powi(attempt as i32)).min(15.0);
                        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                    }
                }
            }
        }
        fatal(format!("blob {} failed after {CHUNK_TRIES} tries: {last}", id.hex()))
    }

    async fn attempt(
        &self,
        id: BlobId,
        url: &mut String,
        size: Option<u64>,
        sink: &Option<Sink>,
    ) -> std::result::Result<Bytes, Attempt> {
        let (status, enc, body) = self.get(url).await.map_err(Attempt::Retry)?;
        if status == 403 || status == 404 {
            // SAS expired or rotated: get a fresh URL
            let hex = id.hex();
            let mut m = self.ado.sas_urls(std::slice::from_ref(&hex)).await.map_err(Attempt::Fatal)?;
            if let Some(u) = m.remove(&hex) {
                *url = u;
            }
            return Err(Attempt::Retry(format!("HTTP {status}")));
        }
        if status != 200 {
            return Err(Attempt::Retry(format!("HTTP {status}")));
        }
        let xpress = match enc.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("xpress") => true,
            None | Some("") | Some("none") | Some("identity") => false,
            Some(other) => {
                return Err(Attempt::Fatal(Fatal(format!(
                    "unsupported Content-Encoding {} on blob {}",
                    repr(other),
                    id.hex()
                ))));
            }
        };
        let sink = sink.clone();
        tokio::task::spawn_blocking(move || -> std::result::Result<Bytes, Attempt> {
            let data = if xpress {
                let cap = size.map_or(MAX_NODE_SIZE, |s| s as usize);
                Bytes::from(xpress_lz77(&body, cap).map_err(|e| Attempt::Retry(format!("malformed xpress data: {e}")))?)
            } else {
                body
            };
            if let Some(s) = size
                && data.len() as u64 != s
            {
                return Err(Attempt::Retry(format!("size {} != {s}", data.len())));
            }
            if Sha512::digest(&data)[..32] != id.hash {
                return Err(Attempt::Retry("content hash mismatch".into()));
            }
            if let Some(sink) = sink {
                sink(&data).map_err(Attempt::Fatal)?;
            }
            Ok(data)
        })
        .await
        .map_err(|e| Attempt::Fatal(Fatal(format!("worker failed: {e}"))))?
    }

    /// Return a whole (small) blob, e.g. a manifest, assembling it if it is a node tree.
    pub async fn read_all(&self, root: BlobId) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut stack = vec![(root, None::<String>)];
        while let Some((id, known_url)) = stack.pop() {
            let url = match known_url {
                Some(u) => u,
                None => {
                    let hex = id.hex();
                    self.ado.sas_urls(std::slice::from_ref(&hex)).await?.remove(&hex).unwrap_or_default()
                }
            };
            let data = self.fetch(id, url, None, None).await?;
            if id.alg == 1 {
                out.extend_from_slice(&data);
                continue;
            }
            let kids = parse_node(&data)?;
            let ids: Vec<BlobId> = kids.iter().map(|k| BlobId { hash: k.hash, alg: if k.is_node { 2 } else { 1 } }).collect();
            let mut urls = HashMap::new();
            for batch in ids.chunks(crate::ado::SAS_BATCH) {
                let hexes: Vec<String> = batch.iter().map(BlobId::hex).collect();
                urls.extend(self.ado.sas_urls(&hexes).await?);
            }
            for kid in ids.into_iter().rev() {
                let u = urls.get(&kid.hex()).cloned();
                stack.push((kid, u));
            }
        }
        Ok(out)
    }
}
