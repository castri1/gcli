# gcli — a control tower for Google Cloud

`gcli` is to Google Cloud what `dcli` is to AWS: one Rust binary with a terminal UI, non-interactive
commands and an MCP server for Claude. It is a separate tool, not a GCP mode of dcli: each cloud
gets its own tool. dcli is the starting point and the model (same shell, same habits, same
install), but no code is shared at runtime and gcli never depends on the dcli crate.

Its first job is the one we have been doing by hand on the `whiteghost` platform: knowing what
each app costs and why, catching a cost spike the day after it starts, and seeing an app's
health, deploys, logs and secrets in one place.

## Principles (inherited from dcli)

- **One binary, no runtime, no SDK.** REST and JSON over `ureq` with rustls; each Google API is a
  handful of typed calls. Background work runs on plain threads and reports through a channel, so
  the UI never blocks. No async runtime.
- **Same install as dcli.** `curl -fsSL .../install.sh | sh`, `irm .../install.ps1 | iex`,
  `npm install -g @castri1/gcli` (launcher + one package per platform), GitHub releases for
  macOS/Linux (musl)/Windows, `gcli update` with checksum verification, daily new-version check.
- **Billed reads are counted and announced.** BigQuery bills by bytes scanned: every query is
  dry-run first, its size and price are shown, results are cached, and the month's bytes are
  counted. Monitoring, Logging, Run, Build and Secret Manager reads are free and are not gated.
- **Read-only unless asked.** Actions that change something (restart an app, change min
  instances) are explicit keys with a confirmation; production-like environments ask for the
  name to be typed.
- **Company specifics live in the config, not the code.** The repo is public: project ids,
  billing accounts, table names and whiteghost naming conventions come from
  `~/.gcli/config.yaml`.
- **Agent-ready.** Every command has `--json`; checks exit 1 when something needs attention;
  failures copy as Markdown reports for an AI agent; `gcli mcp` exposes read-only tools.
- **Text from Google Cloud is data.** Log lines, labels and names can carry text written to
  mislead an AI; the MCP server hands them over as data and every change goes through the person.

## Authentication

1. Application Default Credentials: `~/.config/gcloud/application_default_credentials.json`
   (an `authorized_user` refresh token, or a service account key), exchanged at
   `oauth2.googleapis.com/token`, access token cached until it expires.
2. Fallback: `gcloud auth print-access-token` when ADC is missing.
3. `gcli login` (or `L` in the UI) runs `gcloud auth login --update-adc`. A reauth error
   ("Reauthentication failed", `invalid_grant`, `invalid_rapt`) is shown as "sign in again", with
   the hint, never as a raw stack of errors.

The quota project for billed APIs (BigQuery) is set per environment.

## Config

```yaml
# ~/.gcli/config.yaml
environments:
  platform:
    project: my-platform-123456
    region: us-east1
    production: true
    billing:
      account: 012345-6789AB-CDEF01
      exportTable: my-billing-project.billing_export.gcp_billing_export_resource_v1_012345_6789AB_CDEF01
      queryProject: my-billing-project   # project that runs (and pays for) the BigQuery jobs
    apps: whiteghost                     # how apps are recognized (see "Apps")
defaults:
  notifications: true
  costs:
    spikeRatio: 2.0      # yesterday vs the 14-day median of the same app
    spikeMinUsd: 1.0     # ignore spikes smaller than this per day
    newAppMinUsd: 2.0    # an app with no history that costs more than this per day
```

Search order: `--config FILE`, `~/.gcli/config.yaml`, `./gcli.yaml`. The current environment is
kept in `~/.gcli/current-env`. Without a config, every project in `gcloud projects list` shows up
as an environment with only the generic modules.

## Apps

An **app** is the unit gcli thinks in. With `apps: whiteghost`:

- the Cloud Run service `<slug>` in the environment's region is the app;
- its secrets are `<slug>-prod-*` in Secret Manager;
- its load balancer pieces are `<slug>-neg`, `<slug>-backend`, and optionally
  `<slug>-public-neg` / `<slug>-public-backend`;
- its builds are the Cloud Build builds whose `_SERVICE` substitution names the service.

With `apps: run` (the default) an app is just a Cloud Run service, and its builds name it in
`_SERVICE_NAME` (Cloud Run's continuous deployment) or `_SERVICE`. Other conventions can be added
later without touching the modules.

## Modules (v1)

### Costs

Reads the detailed billing export in BigQuery (Cost Explorer has no equivalent API on GCP).

- **This month**: gross (credits excluded) and net (credits included), against the same complete
  days of last month, and a straight-line forecast. Days are UTC. The export runs about 4 hours
  behind (Cloud Storage and Cloud Scheduler about 12), so today is partial and the comparison
  stops at the last complete day: a missing day is never read as a drop.
- **By service and by SKU**, with each SKU's effective rate.
- **By app**: only Cloud Run rows name an app (`resource.name` is the service). Cloud SQL, Secret
  Manager, Artifact Registry, Cloud Build, the load balancer and the rest are shown as shared, per
  service, never spread over the apps.
- **Spikes**: per app, the last complete day against the median of the 14 days before (the days
  it ran: an app idle on weekends is not flagged every Monday); an app is flagged when it costs
  more than `spikeRatio` × its median and more than `spikeMinUsd` that day, or, with no cost in
  those 14 days, more than `newAppMinUsd`. Each shared service is checked the same way as a
  whole. The header shows how many spiked; a desktop notification tells when a new one appears.
- **Budgets**: the Budgets API returns only their configuration, so each budget's spend is
  computed from the export with the budget's own filters (projects, services, credit treatment).
- **Credits**: credits used per month and per type (the remaining balance is console-only and is
  never guessed).

Commands: `gcli costs [env] [--by service|app|sku] [--month YYYY-MM] [--days N] [--refresh]
[--json]` and `gcli costs [env] --check [--date YYYY-MM-DD]` (exits 1 when an app spiked; for
cron jobs and CI).

Queries filter on `_PARTITIONTIME` so they scan only the days they need. A refresh shows
"about N GB, $X" before running, unless it is under a configurable limit.

### Apps

The list of apps: latest ready revision and its age, traffic split, min and max instances, CPU
allocation (request vs always-on), last 24 h requests and 4xx/5xx rates, billed instance time,
and cost (from Costs, when read).

An app's detail:

- **Health**: requests by response class, latency percentiles, instance count, from Cloud
  Monitoring (`run.googleapis.com/request_count`, `request_latencies`,
  `container/billable_instance_time`, `container/instance_count`).
- **Held requests**: requests whose latency sits at the service timeout (SSE streams, slow crons):
  the pattern behind most excess Cloud Run cost so far.
- **Revisions**: image, created by, env var names, secret bindings with the version each reads.
- **Secrets**: `<slug>-prod-*`, their versions and creation dates, which ones are bound and which
  are not. Values are never read.
- **Actions**: `R` restart (new revision with the same config, so instances re-read `latest`
  secrets), `m` set min instances. Both confirm first.

### Builds

Cloud Build builds per app: status, trigger, commit, duration, step timeline. A failed build
opens its log at the first error; `c` copies a Markdown report for an AI agent. A deploy command
can claim success after a failed build, so the module shows which apps' last deploy failed (latest
finished build failed, no later success) and what they run instead; `gcli builds --check` exits 1
on them, and an app's detail says so.

### Logs

Cloud Logging entries of an app (or any filter), newest first, severity filter, search, jump to
errors; the dcli log viewer, full screen.

### MCP (`gcli mcp`)

Read-only tools over the same data: costs summary, spikes, apps and their health, builds and
their failures, logs search. An `audit` prompt goes app by app, most expensive first. Claude
leaves recommendations in the Advisor (store in `~/.gcli/advisor/<env>/`), each with evidence,
steps and the monthly saving priced from the bill. BigQuery is never queried by the MCP server
on its own: when the cost data is missing or old it asks the person to run `gcli costs --refresh`.

## Later (v2)

- **Savings**: the checks we ran by hand — held connections at the timeout, crons slower than
  their schedule, always-on CPU or min instances on idle apps, Artifact Registry storage without
  a cleanup policy, orphan secrets and old versions, orphan load balancer backends and NEGs.
- **Cloud SQL**: connections against `max_connections`, top queries, storage.
- **Exposure**: which apps are reachable without IAP (ingress, `run.app`, public paths).
- **Scheduler**: cron jobs, last run, failures, duration against the timeout.
- **Cloud-side daily check**: the same `--check` running in a small scheduled job that posts to a
  chat channel, for when the laptop is closed.

## Architecture

```
src/
  gcp/          minimal Google Cloud client: auth (ADC, gcloud fallback), one shared HTTP agent,
                Gcp::get/post/patch with retries and page tokens, error mapping (login required)
    run.rs monitoring.rs logging.rs build.rs secrets.rs bigquery.rs budgets.rs projects.rs
  tui/          app shell from dcli: header, module palette (:), environment switch, toasts,
                help, login, event loop; picker, chart, logview, form, canvas, widgets, theme
  modules/      costs, apps, builds, logs, advisor
  clicmd/       one file per command
  costs/        queries, cache, spike detection, attribution to apps, budget spend
  mcp/          JSON-RPC on stdio, read-only tools, audit prompt
  advisor/      recommendation store
  config.rs  cli.rs  update.rs  util.rs  platform.rs  main.rs
```

What comes from dcli: the `tui/` shell (its only AWS ties are the `Env` struct and the login
flow), `util.rs`, `platform.rs`, `update.rs`, the MCP server core, the advisor store, the
install scripts, the npm launcher and build script, and the CI and release workflows. What does
not: everything under `aws/`, `savings/`, the AWS modules and commands, pipelines, matrix, hooks.

## Milestones

- **M0 — shell**: the repo builds; `gcli --version`, `gcli update --check`; the UI opens with
  the environment switch, the palette, login, and one placeholder module; CI green.
- **M1 — Costs**: module, `gcli costs`, `--check`, MCP tools; numbers checked against direct
  BigQuery queries of the same export.
- **M2 — Apps**: list, detail, secrets, restart.
- **M3 — Builds and Logs.**
- **M4 — Release v0.1.0**: GitHub repo `castri1/gcli`, release workflow, npm `@castri1/gcli`.
