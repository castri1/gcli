# gcli

A control tower for **Google Cloud** in your terminal: a terminal UI, non-interactive commands
for scripts and AI agents (`--json` everywhere), and an MCP server for Claude. It is to Google
Cloud what [dcli](https://github.com/castri1/dcli) is to AWS: the same shell, the same habits,
the same install, as a separate tool.

Its first job: knowing what each app costs and why, catching a cost spike the day after it
starts, and seeing an app's health, deploys, logs and secrets in one place. See
[DESIGN.md](DESIGN.md) for where it is going.

**Status: M1, M2 and M3.** The shell (header, `:` palette, environment switch, toasts, help,
sign-in), the Google Cloud client (Application Default Credentials with a gcloud fallback, retries,
page tokens), **Apps** (health from Cloud Monitoring, held requests, revisions, secrets and their
versions, restart, min instances), **Costs** (the billing export: month to date, budgets, services
and SKUs, apps, spikes), **Builds** (per app, step timeline, failed logs, which apps' last deploy
failed) and **Logs** (what an app wrote: search, follow, the distinct errors).

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
gcli apps [ENV] [--sort cost|requests|errors|name] [--json]   # apps and their last 24 h
gcli apps [ENV] --held     # only apps holding requests at their timeout (exit 1 if any)
gcli app APP [ENV] [--json | --report]   # one app: health, revisions, secrets (never values)
gcli apps restart APP [ENV] [--yes]      # new revision, same config: re-reads `latest` secrets
gcli apps min APP N [ENV] [--yes]        # service-level min instances (no new revision)
gcli builds [APP] [ENV] [--failed] [--limit N] [--json]   # recent builds, and whose last deploy failed
gcli builds [ENV] --check [--json]       # exit 1: an app's last deploy failed; 2: could not read
gcli build ID [ENV] [--log | --report] [--json]   # steps, the failing step, the log around the first error
gcli logs APP [ENV] [--severity LEVEL | --errors] [--search TEXT] [--since 15m|2h|3d] [--limit N]
                    [--requests] [--filter EXPR] [--follow] [--json]   # what an app wrote
gcli logs APP [ENV] --errors --group [--since DURATION] [--json]       # the distinct errors, counted
gcli logs [ENV] --filter EXPR [...]   # anything that isn't an app, by a Cloud Logging filter
gcli costs [ENV] [--by service|app|sku] [--month YYYY-MM] [--days N] [--refresh] [--json]
gcli costs [ENV] --check [--date YYYY-MM-DD] [--notify]   # exit 1: a spike; 2: could not read fresh costs
gcli envs [--json]         # the environments, and which one is current
gcli use [ENV]             # set the current environment
gcli add [NAME] [--project ID] [--region R] [--production]   # add an environment to the config
gcli update [--check]      # update to the latest release (checksum verified)
gcli mcp [--env NAME]      # MCP server on stdio: claude mcp add gcli -- gcli mcp
```

| Where | Keys |
|---|---|
| Everywhere | `:` / `Ctrl+K` go to a module or an environment · `Tab` / `1`-`9` switch module · `e` environment · `L` sign in (suspends the UI and runs gcloud) · `?` help · `q` quit |
| Apps | `↑↓` move · `Enter` open · `/` search · `s` sort · `l` its logs · `R` restart · `m` min instances · `y` copy the URL · `o` open in the Cloud console · `r` reload |
| An app | `←→` Health / Revisions / Secrets · `w` 24 h / 6 h · `l` its logs · `c` copy a report for an AI agent · `b` its builds · `R` · `m` · `r` refresh · `Esc` back |
| Builds | `↑↓` move · `Enter` open · `/` search · `f` failed only · `a` this app only · `y` copy the id · `o` open in the Cloud console · `r` reload |
| A build | `↑↓` scroll the log · `z` log full screen (`/` search, `e`/`E` errors, `y` copy) · `c` copy a report for an AI agent · `r` refresh · `Esc` back |
| Logs | `s` severity · `w` time window · `f` follow on/off · `t` the requests log · `a` another app · `r` read again · `/` search · `n` `N` next, previous · `e` `E` next, previous error · `y` copy every line · `↑↓` `g` `G` move · `Esc` `q` back |
| Costs | `←→` Overview / By service / By app / Spikes · `↑↓` move · `Enter` a service's SKUs · `r` read again |

### A daily cost check

`gcli costs ENV --check --notify` raises a desktop notification when something spiked and when it
could not tell (signed out, say); a quiet day stays quiet. To run it every morning, schedule it
with cron on Linux, Task Scheduler on Windows, or a launchd agent on macOS, which also runs a job
missed while the Mac slept. Save this as `~/Library/LaunchAgents/com.gcli.costs-check.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.gcli.costs-check</string>
  <key>ProgramArguments</key><array>
    <string>/opt/homebrew/bin/gcli</string><string>costs</string><string>ENV</string>
    <string>--check</string><string>--notify</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
  <key>StartCalendarInterval</key><dict><key>Hour</key><integer>8</integer><key>Minute</key><integer>0</integer></dict>
</dict></plist>
```

and load it with `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.gcli.costs-check.plist`.
Costs are read by UTC day and the export runs up to 12 hours behind, so run it once that day is
complete.

### Apps

An app is a Cloud Run service of the environment's region. With `apps: whiteghost` its secrets
are also the `<slug>-prod-*` ones in Secret Manager. The list shows each app's latest ready
revision and its age, traffic split, min/max instances, CPU allocation (`req`: only during
requests, `always`: always on), and its last 24 h from Cloud Monitoring (one call per metric for
every app): requests, 4xx and 5xx shares, billed instance hours and p99 latency. When
`gcli costs` has written `~/.gcli/costs/<env>/apps.json`, this month's and yesterday's cost too.

**Held** marks an app whose requests run until the request timeout: SSE streams, long polls or
slow crons that Cloud Run cuts at the timeout, each keeping an instance billed the whole time.
Either its p99 latency is at least 90% of the timeout, or, in a busy app whose fast requests hide
them from the p99, at least 10 requests ran that long and they take a fifth or more of its billed
instance time. The NOTES column counts requests near the timeout even when they are not enough
to call the app held.

An app's **Secrets** tab lists its secrets with their versions (enabled, disabled, destroyed,
with dates), which are bound to the service (variable ← version) and which are not, and flags a
variable bound to `latest` whose newest version came after the running revision: Cloud Run
reads it only when an instance starts, so it is *not loaded yet: restart to pick it up*. Values
are never read.

Changes are explicit and confirmed; in a production environment you type the app's name (the
commands refuse without `--yes` outside a terminal). `R` / `gcli apps restart` sends the service
back unchanged but for a `gcli-restart` template label, so Cloud Run makes a new revision with
the same configuration; `m` / `gcli apps min` sets the service-level minimum, which applies at
once without a revision (the revision's own minimum still applies when higher).

### Builds

A build belongs to an app when one of its substitutions names the Cloud Run service: `_SERVICE`
with `apps: whiteghost`; `_SERVICE_NAME` (what Cloud Run's continuous deployment writes), then
`_SERVICE`, with `apps: run`. Builds are read from the environment's region; an environment without
one reads Cloud Build's `global` builds, since Cloud Build has no "every region".

**Last deploy failed** marks an app whose latest finished build failed, with no later build that
succeeded (one that was cancelled or is still running does not count). A deploy command can claim
success after a failed build, so this reads the builds and not what the command said. The notes say
what serves instead: *still running* a revision older than the failed build; or *last build failed*
when a revision came after it (the build failed past its deploy step, or someone deployed another
way, or restarted the app). A name in a failed build with no Cloud Run service (an app since
deleted) is not counted. The check reads the newest 3,000 failed builds and asks, for each app with
one, what came after; `gcli app` and an app's detail in the UI say so too (`b` there shows its builds).

`gcli builds ENV --check` exits 1 when some app's last deploy failed and 2 when it could not read,
for cron and CI, in the same way as `gcli costs --check`.

A failed build opens its log at the first line of the failing step that says an error (names of
files and packages like `error-handler.ts`, and warnings, do not count), or at the step's last line
when none does. Log text comes from Cloud Logging, whose retention (30 days by default) is how far
back a build's log can be read. Reports and the MCP server hand on the lines around the first error
and the end of the step, marked as data, with what looks like a credential replaced (a best effort; `--log` and `gcli logs` show the text as it is, to whoever can already read it). Substitutions other than the ones
gcli reads, and steps' arguments and environment, are never shown or asked for.

**MCP.** `list_builds` and `build_failure` read the same, read-only.

### Logs

`gcli logs APP` shows what the app wrote in the last hour (stdout, stderr and anything else, not
the log of requests served; `--requests` shows that one), newest first: time, severity, message.
It reads Cloud Logging entries of the Cloud Run service in the environment's region.

- **`--severity LEVEL`** is that severity or worse, **`--errors`** is ERROR or worse.
- **`--search TEXT`** finds text in any field of an entry, in any case, like the Logs Explorer's
  search box. A word that every entry's labels hold (the app's name, the region, the project) is
  looked for only in the message and the request's URL.
- **`--filter EXPR`** takes a whole [Cloud Logging filter](https://cloud.google.com/logging/docs/view/logging-query-language),
  and-ed with the other options, for anything that isn't an app. `--since` still applies, unless
  the filter speaks of `timestamp` itself and no `--since` is given.
- **`--follow`** shows the last entries, then what arrives, oldest first, until Ctrl-C. It asks
  every 5 seconds (Cloud Logging allows about 60 reads a minute per project), goes 60 seconds back
  each time for entries that arrive late, and shows none twice. With `--json`, one object per line.
- **`--errors --group`** lists the distinct errors, most frequent first: each message's first
  line with its numbers, ids, hex and email addresses replaced by `#`, how many there were and
  when the last one was. It counts the newest 1000 entries unless `--limit` says otherwise, and
  says when there were more.

Text output cuts a line at 2000 characters; `--json` cuts nothing. What is said about the read
goes to stderr, so a pipe gets only the log.

**Logs** in the UI asks for an app (`l` in Apps goes straight to the app's logs), and shows the
newest 1000 entries in the full-screen viewer, where the mouse selects text. Following shows
them oldest first and polls only while the module is on screen.

**MCP.** `search_logs` (app, severity, search, since, at most 200 entries, long messages cut) and
`error_summary` (the patterns and counts, never the messages themselves) are read-only and limited
by `gcli mcp --env`. Log text is handed over as data, and only what a search needs of each entry:
no payloads, labels or traces, as logs may hold personal data.

### Costs

Costs reads the detailed billing export (`billing.exportTable`, the resource-level one) in
BigQuery: the environment's project per UTC day, service, app (Cloud Run service) and SKU, and
the billing account's months for budgets and credits. Gross leaves credits out, net counts them.

- **Days are UTC.** The export runs about 4 hours behind (Cloud Storage and Cloud Scheduler
  about 12): today is partial, and comparisons, forecasts and spikes stop at the last complete
  day.
- **Apps.** Only Cloud Run rows name an app (`resource.name`, the service). Cloud SQL, Secret
  Manager, Artifact Registry, Cloud Build, the load balancer and the rest are shown as shared,
  per service, never spread over the apps.
- **Spikes.** An app spikes on a complete day when it costs more than `spikeRatio` × the median
  of the 14 days before (the days it ran) and more than `spikeMinUsd`; with no cost in those 14
  days, more than `newAppMinUsd`. Shared services are checked as a whole the same way.
- **Budgets.** The Budget API returns no spend: each budget's is summed from the export with its
  own filters (projects, services, credits). It needs the Cloud Billing Budget API enabled in the
  environment's project and `roles/billing.viewer` on the billing account.
- **Billed reads.** BigQuery bills the bytes scanned (queries read only the export days they
  need). A read is dry-run first and priced ("≈ 900 MB, US$ 0.0051"); it runs on its own when
  what was read is older than `autoRefreshHours` and the price is under `autoRefreshMaxUsd`,
  otherwise it asks (`--refresh` reads anyway). Every query is counted in
  `~/.gcli/costs/ledger.json`. What was read is kept in `~/.gcli/costs/<env>/`, with
  `apps.json` (each app's month to date, last complete day, 14-day median, spike) for other tools.
- **MCP.** `costs_summary` and `cost_spikes` answer from what was kept and never query BigQuery;
  when it is missing or older than a day they ask for `gcli costs <env> --refresh`.

Reading needs `roles/bigquery.jobUser` on `billing.queryProject` and `roles/bigquery.dataViewer`
on the export's dataset.

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
    autoRefreshHours: 6  # Costs reads the export again when what it read is older than this…
    autoRefreshMaxUsd: 0.05   # …and the dry run prices the read under this; otherwise it asks
    bigqueryUsdPerTib: 6.25   # BigQuery's on-demand price, to price a read before it runs
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
