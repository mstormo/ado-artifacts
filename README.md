# ado-artifacts

Extremely fast, parallel, hash-verified downloader for Azure DevOps pipeline and any build artifacts, even Containers,
written in Rust.

## Usage

```
ado-artifacts 'https://dev.azure.com/contoso/MyProject/_build/results?buildId=1234'   # exact run
ado-artifacts Nightly-Build                             # latest run of a pipeline (name, * wildcards, or numeric ID)
ado-artifacts 219 -a manifest -a wheels -o ~/Downloads  # selected artifacts only
ado-artifacts <target> --list                           # list artifacts and files, download nothing
```


| Option                                            | Meaning                                                                                                                      |
| ------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `-a, --artifact NAME`                             | only this artifact (repeatable; default all)                                                                                 |
| `-o, --out DIR`                                   | output directory (default `.`); files land in `DIR/<artifact>/<path>`                                                        |
| `-j, --concurrency N`                             | max concurrent downloads, at least 1 (default: 128)                                                                          |
| `-q, --quiet`                                     | no progress bar or informational output; warnings, errors and the failed-run report are still printed. Conflicts with `-v`   |
| `-v, --verbose`                                   | prefix the second progress line with `X/Y blocks`                                                                            |
| `-l, --list`                                      | list instead of download                                                                                                     |
| `--force`                                         | re-download files that already exist                                                                                         |
| `--org URL|NAME`, `--project NAME`, `--tenant ID` | organization, project and Azure AD tenant (for the Azure CLI token); env `ADO_ORG`, `ADO_PROJECT`, `ADO_TENANT`; no defaults |
| `--login`, `--logout`                             | save / delete a personal access token (see above); no target                                                                 |


Environment variables: `ADO_ORG`, `ADO_PROJECT`, `ADO_TENANT`, `AZURE_DEVOPS_EXT_PAT`, `ADO_PAT`, `ADO_TOKEN`,
`ADO_ARTIFACTS_HOME`, `AZURE_CONFIG_DIR`, `NO_COLOR`.

Both `dev.azure.com/<org>/<project>/...` and `<org>.visualstudio.com/<project>/...` build URLs are accepted and
any suffixes are ignored. For a pipeline target the latest run by queue time is used, on any branch; if it has not
finished or did not succeed, the failed tasks and their errors are printed and nothing is downloaded.

Exit codes: `0` ok, `1` error, `2` latest run of the pipeline not succeeded (or bad command line), `130` interrupted
(Ctrl-C; partial files and journals are kept, rerun the same command to resume).

## Progress display

Progress goes to stderr. When stderr is a terminal, a two-line display is updated 5 times a second:

```
 75.0% │█████████████▌░░░░░░░│ 212 MB 00:01
       2618/3303 files · 639/851 MB · 117.3 MB/s · 00:04 / ETA 21:46:11
```

With `-v` the second line is prefixed with the block counts:

```
 75.0% │█████████████▌░░░░░░░│ 212 MB 00:01
       5,982/6,667 blocks · 2618/3303 files · 639/851 MB · 117.3 MB/s · 00:04 / ETA 21:46:11
```

- Line 1: percentage (fixed width), the bar, remaining MB, remaining time. The bar represents the total size of all
files of the run, fills smoothly in eighths of a cell and stretches over the rest of the terminal width (the width
is re-read every frame). Bytes already present from an interrupted earlier download count as done, so a resumed run
starts partly filled.
- Line 2 fields in order: blocks done/total (`-v` only); files completed/total; MB done/total (decimal MB);
uncompressed throughput (decoded bytes written per second, moving average over ~2.5 s); elapsed time / ETA as local
wall-clock time (time left is on line 1).
- Colours are used on a terminal unless `NO_COLOR` is set.
- `-v` adds `X/Y blocks`: X is the number of blocks downloaded, verified and written to their destinations of the Y
total.
- `-q` prints none of this (no bar, spinner, `Using run ...`, `Downloading ...`, `Done ...` or `Nothing to download.`).
- When stderr is not a terminal no escape codes are written: the same two lines as plain text (no bar; percentage,
remaining MB and time, then the second line in full, with the block counts with `-v`) are printed every 10 s, plus
a final pair.



## How it works

Pipeline artifacts live in a content-addressed blob store as a Merkle tree per file. The tool reads the artifact
manifest from the blob store, walks every file's node tree (verifying each node's hash, child count and sizes),
de-duplicates chunks, obtains read-only SAS URLs in batches of 2000 ahead of the workers, and fetches the chunks
directly from Azure Blob Storage with parallel requests.

Chunks are decoded (MS-XCA Plain LZ77 "xpress", or identity), verified against their SHA-512 hash and exact size, and
written to file. 

Transient failures (network errors, 5xx, 429, hash/size mismatch) are retried with exponential backoff
(capped at 15 s, 10 tries per blob); 403/404 fetch a fresh SAS URL. Classic build ("Container") artifacts are listed
via the Containers API and streamed file by file (gzip decoded, size checked), up to 16 files at a time. Any fatal
error stops all work and exits 1.

## Install

### Prebuilt binaries

Each [release](https://github.com/mstormo/ado-artifacts/releases) has archives for:

| Platform                | Archive suffix                   |
| ----------------------- | -------------------------------- |
| Linux x86_64 (static)   | `x86_64-unknown-linux-musl`      |
| Linux arm64 (static)    | `aarch64-unknown-linux-musl`     |
| macOS Apple Silicon     | `aarch64-apple-darwin`           |
| macOS Intel             | `x86_64-apple-darwin`            |

Download the archive for your platform, check it against `SHA256SUMS`, and put the binary on your `PATH`, e.g.:

```
tar -xzf ado-artifacts-v0.1.0-aarch64-apple-darwin.tar.gz
install -m 755 ado-artifacts-v0.1.0-aarch64-apple-darwin/ado-artifacts ~/.local/bin/
```

The macOS binaries are not signed: if macOS refuses to run one downloaded with a browser, remove the quarantine flag
with `xattr -d com.apple.quarantine ~/.local/bin/ado-artifacts`.

### From source

You need a Rust toolchain (install it with [rustup](https://rustup.rs) if `cargo` is not available). From a
checkout of this repository:

```
cargo install --path . --locked
```

This builds an optimized binary and installs it as `~/.cargo/bin/ado-artifacts`. Make sure `~/.cargo/bin` is on
your `PATH` (rustup sets this up by default), then check it with:

```
ado-artifacts --version
```

To update after pulling new changes, run the same `cargo install` command again. To remove it:

```
cargo uninstall ado-artifacts
```



## Setup and authentication

Organization, project and credentials come from your environment and Azure tooling. If you already use the
Azure CLI (`az login`, optionally `az devops configure --defaults organization=... project=...`), the tool
works without any setup. Otherwise run `ado-artifacts --login` once.

### Organization and project

Organization and Project are extracted in the following order (first one wins):

1. a build results URL as the target (it names both)
2. `--org` / `--project` (`--org` takes a full URL such as `https://dev.azure.com/contoso` or
  `https://contoso.visualstudio.com`, or a bare name such as `contoso`; only Azure DevOps Services hosts are
  accepted, since credentials are sent to the organization — Azure DevOps Server / on-premises is not supported)
3. environment `ADO_ORG` / `ADO_PROJECT`
4. the Azure CLI devops defaults in `~/.azure/azuredevops/config` (section `[defaults]`, keys `organization` and
  `project`; `$AZURE_CONFIG_DIR/azuredevops/config` if `AZURE_CONFIG_DIR` is set), as written by
   `az devops configure --defaults organization=https://dev.azure.com/contoso project=MyProject`
5. what `--login` saved (the saved project is only used for the organization it was saved with)

A pipeline name or ID needs both organization and project; a build results URL needs neither. If one is still
unknown the tool exits with code 1 and lists these options.

### Credentials

Candidates are tried in this order and the first one that authenticates against the organization is used
(`GET <org>/_apis/connectionData`); if none works, the tool exits with code 1 and says what it tried and why each
failed.

1. environment `AZURE_DEVOPS_EXT_PAT` (the variable the Azure DevOps CLI extension uses) or `ADO_PAT`: a personal
  access token, sent with HTTP Basic authentication
2. environment `ADO_TOKEN`: an Azure AD access token, sent as a Bearer token
3. the Azure CLI: `az account get-access-token --resource 499b84ac-1321-427f-aa17-267ca6975798` (the Azure DevOps
  application ID), for the current `az login`; `--tenant` / `ADO_TENANT` adds `--tenant <id>`. Without `--tenant`, if
   the organization rejects the token of the default tenant, the tool asks the organization which tenant owns it and
   retries with that one. A missing `az` or a failing command just means this candidate is unavailable. These tokens are renewed automatically on HTTP 401/203.
4. the personal access token saved by `--login`, if it was saved for the same organization

A personal access token cannot be renewed: if it is rejected during a run, the tool stops with
"PAT expired or lacks permissions" and you run `ado-artifacts --login` again. Tokens are never printed or logged.

### `--login` and `--logout`

```
ado-artifacts --login [--org contoso] [--project MyProject]
ado-artifacts --logout
```

`--login` needs no target. It takes the organization from `--org`, `ADO_ORG`, the Azure CLI defaults or an earlier
login (in that order), and asks for it if none is known. It then prints the steps to create a personal access token
for your own account and reads the token you paste:

1. Open `https://dev.azure.com/<org>/_usersSettings/tokens` (for `<org>.visualstudio.com` organizations:
  `https://<org>.visualstudio.com/_usersSettings/tokens`) and sign in.
2. Click "New Token".
3. Name: `ado-artifacts` (any name).
4. Organization: your organization.
5. Expiration: choose a date (at most 1 year).
6. Scopes: select "Custom defined", then enable: Build: Read
7. Click "Create" and copy the token (it is shown only once).
8. Paste it into the prompt.

The token is read without echo when stdin is a terminal; when stdin is not a terminal the first line is used (after
the organization line, if the organization has to be asked), so this works in scripts:
`printf %s "$PAT" | ado-artifacts --login --org contoso`. The token is checked against the organization before
anything is saved; on failure nothing is saved and the exit code is 1. `--project` also saves a default project.

The login is stored in `~/.ado-artifacts/credentials.json` (`$ADO_ARTIFACTS_HOME` replaces the directory; on Windows
the user profile directory is used for `~`). The directory is created with mode 0700 and the file is written
atomically with mode 0600; a warning is printed if the file is readable by others. It holds the organization, the
optional project, the token and the creation time. `--logout` deletes the file.

## License

MIT; see [LICENSE](LICENSE).
