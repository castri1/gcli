# gcli

A control tower for **Google Cloud** in your terminal: a terminal UI, non-interactive commands
for scripts and AI agents (`--json` everywhere), and an MCP server for Claude. It is to Google
Cloud what [dcli](https://github.com/castri1/dcli) is to AWS: the same shell, the same habits,
the same install, as a separate tool.

Its first job: knowing what each app costs and why, catching a cost spike the day after it
starts, and seeing an app's health, deploys, logs and secrets in one place. See
[DESIGN.md](DESIGN.md) for where it is going.

**Status: M0.** The shell (header, `:` palette, environment switch, toasts, help, sign-in),
the Google Cloud client (Application Default Credentials with a gcloud fallback, retries, page
tokens) and a first **Apps** module that lists an environment's Cloud Run services. Costs,
the full Apps module, Builds and Logs come next (M1–M3).

```
 ◆ gcli   1 Apps                                                    ● prod acme-prod-123456 · us-east1
╭ Cloud Run services · 3 ──────────────────────────────────────────────────────────────────────────╮
│ SERVICE   REVISION           MIN/MAX    UPDATED  BY                                               │
│ admin     admin-00091-8ps      0/100    23h ago  123456789-compute@developer.gserviceaccount.com  │
│ checkout  checkout-00412-x2q     1/4     2d ago  ana@example.com                                  │
│ orders    orders-00046-9sg     0/100    14d ago  ci@acme-prod-123456.iam.gserviceaccount.com      │
╰───────────────────────────────────────────────────────────────────────────────────────────────────╯
 ↑↓ move  / search  y copy URL  o console  r reload               │ : go to  e env  L login  ? help  q quit
```

## Install

macOS and Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/castri1/gcli/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/castri1/gcli/main/install.ps1 | iex
```

Run the same command again to update. `GCLI_VERSION=v0.1.0` installs a specific version and
`GCLI_INSTALL_DIR` changes where it goes (default `~/.local/bin`, or `%LOCALAPPDATA%\gcli\bin`).

With Node.js on any system: `npm install -g @castri1/gcli`.

From source (Rust 1.89+): `cargo install --git https://github.com/castri1/gcli`

You also need the [gcloud CLI](https://cloud.google.com/sdk/docs/install), to sign in.

## Use

```bash
gcli login                 # gcloud auth login --update-adc (a browser opens)
gcli                       # the UI, in the current environment
gcli prod                  # the UI, in that environment
gcli apps [ENV] [--json]   # the environment's Cloud Run services
gcli envs [--json]         # the environments, and which one is current
gcli use [ENV]             # set the current environment
gcli add [NAME] [--project ID] [--region R] [--production]   # add an environment to the config
gcli update [--check]      # update to the latest release (checksum verified)
gcli mcp [--env NAME]      # MCP server on stdio: claude mcp add gcli -- gcli mcp
```

| Where | Keys |
|---|---|
| Everywhere | `:` / `Ctrl+K` go to a module or an environment · `Tab` / `1`-`9` switch module · `e` environment · `L` sign in (suspends the UI and runs gcloud) · `?` help · `q` quit |
| Apps | `↑↓` move · `/` search · `y` copy the URL · `o` open in the Cloud console · `r` reload |

### Credentials

gcli reads Google APIs with an OAuth access token, without an SDK:

1. **Application Default Credentials** (`GOOGLE_APPLICATION_CREDENTIALS`, or
   `~/.config/gcloud/application_default_credentials.json`) of type `authorized_user`: the
   refresh token is exchanged at Google's token endpoint and the access token is kept until it
   is about to expire.
2. **`gcloud auth print-access-token`**, when there is no ADC, when it is another kind of
   credential (a service account key, say), or when Google refuses its refresh token
   (`invalid_grant`, `invalid_rapt`: a new sign-in is needed).

When neither works, the UI says "sign in again (L)" and commands say `hint: run gcli login`.
`gcli login` (and `L`) run `gcloud auth login --update-adc`, which renews both. Tokens are never
printed, logged or put in an error; `GCLI_DEBUG=1` tells which of the two was used.

## Config

`~/.gcli/config.yaml` (or `--config FILE`, or `./gcli.yaml`):

```yaml
environments:
  prod:
    project: acme-prod-123456
    region: us-east1
    production: true
    billing:
      account: 012345-6789AB-CDEF01
      exportTable: acme-billing.billing_export.gcp_billing_export_resource_v1_012345_6789AB_CDEF01
      queryProject: acme-billing         # project that runs (and pays for) the BigQuery jobs
    apps: run                            # how apps are recognized: run (default) or whiteghost
  sandbox:
    project: acme-sandbox
defaults:
  notifications: true
  costs:
    spikeRatio: 2.0      # yesterday vs the 14-day median of the same app
    spikeMinUsd: 1.0     # ignore spikes smaller than this per day
    newAppMinUsd: 2.0    # an app with no history that costs more than this per day
```

Only `project` is required. Without `region`, the active gcloud configuration's (`run/region`,
else `compute/region`); without either, every region. Without `production`, environments whose
name or project says `prod` or `live` count as production.

Without a config file, the active gcloud configuration (its project and region) is the one
environment; `gcli add` saves it. `gcli add` rewrites the file and keeps the previous version as
`config.yaml.bak`; keys gcli does not know are kept, comments are not. The current environment
lives in `~/.gcli/current-env`.

`GCLI_API_ROOT` sends every Google API call to another server, for tests and local fakes:
`https://run.googleapis.com/v2/…` goes to `$GCLI_API_ROOT/run/v2/…`.

## Development

```bash
cargo test                 # unit tests, Google APIs and the token endpoint against a local fake
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo build --release      # target/release/gcli
```

CI runs them on Linux, macOS and Windows. To release: set `version` in `Cargo.toml`, commit,
then tag and push (`git tag v0.1.0 && git push origin v0.1.0`). The release workflow builds
macOS (arm64, x86_64), Linux (x86_64, arm64, static musl) and Windows (x86_64) archives with
checksums, attaches them to a GitHub release and publishes them to npm (`npm/build.mjs`):
`@castri1/gcli` is a small launcher, and npm installs only the `@castri1/gcli-<os>-<cpu>`
package with the binary for the machine.

## License

MIT
