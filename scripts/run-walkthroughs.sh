#!/usr/bin/env bash
# Runs the Platform's example walkthroughs (apps/**/examples) so they cannot rot
# unnoticed: CI compiles them (clippy --all-targets) and used to run none, so a
# refusal a walkthrough proves was never executed.
#
# Run it from the Platform root. DATABASE_URL names a PostgreSQL 18 role that
# may create databases: every walkthrough gets its own empty database, dropped
# afterwards, and its own private scratch folder. Nothing here is a secret or a
# product seed. It builds what it runs, one module at a time, and removes each
# module's example programs when they have run (each is a whole linked app).
#
#   scripts/run-walkthroughs.sh               every walkthrough in WALKTHROUGHS
#   scripts/run-walkthroughs.sh visitor p6    only the named ones
#   scripts/run-walkthroughs.sh --check       only check that every example is listed
#
# WALKTHROUGH_GROUP=base|privacy|core|web runs one group (CI runs them side by side).
# WALKTHROUGH_EXTRA holds extra WALKTHROUGHS lines, one per line, for a branch
# whose new walkthrough is not listed yet. WALKTHROUGH_ALLOW_MISSING=1 skips a
# listed example an older checkout does not have. The workflow sets both when
# it is started by hand against another branch with this copy of the script.
#
# A walkthrough passes when every step exits 0. One whose kind is `serve` ends
# by serving the app for a browser: it passes when it has finished its proofs
# and accepts connections, and is then stopped by its own process ID. Failing
# ones are all reported, then the script exits 1. KNOWN_FAILURES lists the
# walkthroughs that fail on main today; see the rule above that list.
#
# To add one: add a line to WALKTHROUGHS. An example that cannot run here goes
# in NOT_RUN with the reason. `--check` (and every run) fails while an example
# is in neither list, so a new example is classified in the change that adds it.
#
# Programs some walkthroughs call, found on PATH unless the variable is set:
#   WALKTHROUGH_CHROMIUM   a sandbox-capable Chromium or Chrome (PDF and privacy exports)
#   WALKTHROUGH_QPDF       qpdf (protected PDFs)
#   WALKTHROUGH_LANGUAGE_MODEL  the pinned fastText file (scripts/provision-language-model.py)
#   pdftotext              poppler-utils (privacy exports are read back)
set -euo pipefail

# WALKTHROUGH_ROOT names the Platform checkout when this file is run from outside it.
root=${WALKTHROUGH_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)}
cd "$root"

# group|name|module folder|example|prepare|environment|steps|kind
#
# prepare: what the fresh database holds before the first step.
#   none          nothing (the walkthrough migrates, or needs no database)
#   migrate       `bilibi-web migrate`
#   seed          migrate, then `seed-production` (the reference lists, and the Dev instance
#                 role: without it a grant change asks for a second factor nobody enrolled)
#   fixture-seed  migrate, the walkthrough fixture, then seed-production (its tenants get their pick-lists)
#   operator      migrate, the walkthrough fixture, and tenant 1 appointed platform operator
#   geonames      migrate, then `geonames-import` of apps/core/geonames/sample from a local server
#   seed-app      seed, and the app itself running at APP_ORIGIN beside the walkthrough
# environment: space-separated KEY=VALUE. `@/x` is a private folder made for this
#   walkthrough; @chromium, @qpdf and @port (a free local port) are filled in.
#   `-KEY` unsets a variable every walkthrough otherwise gets (APP_ORIGIN).
# steps: the argument lists of the processes to run in order on the one
#   database, separated by `;`. `-` is a run without arguments. `@/x` and @model as above.
# kind: `serve` for a walkthrough that ends by serving the app (see above).
WALKTHROUGHS=(
  'base|agent|apps/base/agent_api|agent-walkthrough|seed|BILIBI_AGENT_DEMO=disposable|-'
  'base|agent-write|apps/base/agent_api|write-walkthrough|seed|BILIBI_AGENT_DEMO=disposable|-'
  'base|artifacts|apps/base/artifacts|artifacts-walkthrough|migrate|BILIBI_DISPOSABLE_ARTIFACTS_DEMO=yes BILIBI_ARTIFACTS_DEMO_ROOT=@/artifacts|-;recover'
  'base|artifacts-html-pdf|apps/base/artifacts|html-pdf-walkthrough|migrate|BILIBI_DISPOSABLE_ARTIFACTS_DEMO=yes BILIBI_ARTIFACTS_DEMO_ROOT=@/artifacts BILIBI_PDF_DEMO_OUT=@/out BILIBI_PDF_CHROMIUM=@chromium|-'
  'base|artifacts-pdf|apps/base/artifacts|pdf-walkthrough|migrate|BILIBI_DISPOSABLE_ARTIFACTS_DEMO=yes BILIBI_ARTIFACTS_DEMO_ROOT=@/artifacts BILIBI_PDF_DEMO_OUT=@/out|fixtures;-'
  'base|artifacts-queued-pdf|apps/base/artifacts|queued-pdf-walkthrough|migrate|BILIBI_DISPOSABLE_ARTIFACTS_DEMO=yes BILIBI_ARTIFACTS_DEMO_ROOT=@/artifacts BILIBI_PDF_DEMO_OUT=@/out BILIBI_PDF_CHROMIUM=@chromium BILIBI_PDF_QPDF=@qpdf BILIBI_PDF_DEMO_PORT=@port|-'
  'base|artifacts-retention|apps/base/artifacts|retention-walkthrough|migrate|BILIBI_DISPOSABLE_ARTIFACTS_DEMO=yes BILIBI_ARTIFACTS_DEMO_ROOT=@/artifacts|-;report'
  'base|audit-actions|apps/base/audit_views|actions-fixture|migrate|BILIBI_DISPOSABLE_AUDIT_FIXTURE=yes|-'
  'base|audit-mutations|apps/base/audit_views|mutations-fixture|migrate|BILIBI_DISPOSABLE_AUDIT_FIXTURE=yes|-'
  'base|audit-history-kinds|apps/base/audit_views|history-kinds|migrate|BILIBI_DISPOSABLE_AUDIT_FIXTURE=yes|-|serve'
  'base|bulk-import|apps/base/bulk_data|import-walkthrough|none|BILIBI_BULK_DATA_DEMO=disposable|-'
  'base|bulk-export|apps/base/bulk_data|export-walkthrough|none|BILIBI_BULK_DATA_DEMO=disposable|-'
  'base|bulk-update|apps/base/bulk_data|update-walkthrough|none|BILIBI_BULK_DATA_DEMO=disposable|-'
  'base|database-pool|apps/base/database|pool-walkthrough|none||-'
  'base|integration|apps/base/integration|integration-walkthrough|migrate|BILIBI_DISPOSABLE_INTEGRATION_DEMO=yes|-'
  'base|language-detection|apps/base/language_detection|language-detection-walkthrough|none|BILIBI_DISPOSABLE_LANGUAGE_WALKTHROUGH=yes|@model'
  'base|mail-inbound|apps/base/mail|inbound-walkthrough|none||@/parsed'
  'base|mail|apps/base/mail|mail-walkthrough|none||@/wire.eml'
  'base|mail-unset-relay|apps/base/mail|unset-relay-walkthrough|migrate|BILIBI_DISPOSABLE_MAIL_DEMO=yes|-'
  'base|media|apps/base/media|media-walkthrough|migrate|BILIBI_DISPOSABLE_MEDIA_DEMO=yes BILIBI_MEDIA_DEMO_ROOT=@/media|-'
  'base|media-attachments|apps/base/media|attachments-walkthrough|migrate|BILIBI_DISPOSABLE_MEDIA_DEMO=yes BILIBI_MEDIA_DEMO_ROOT=@/media|-'
  'base|media-scan|apps/base/media|scan-walkthrough|migrate|BILIBI_DISPOSABLE_MEDIA_DEMO=yes BILIBI_MEDIA_DEMO_ROOT=@/media|-'
  'base|media-scan-no-scanner|apps/base/media|scan-walkthrough|migrate|BILIBI_DISPOSABLE_MEDIA_DEMO=yes BILIBI_MEDIA_DEMO_ROOT=@/media|no-scanner'
  'base|perf|apps/base/perf|perf-walkthrough|migrate|BILIBI_DISPOSABLE_PERF_DEMO=yes|-;inspect'
  'base|queue|apps/base/queue|queue-walkthrough|migrate|BILIBI_DISPOSABLE_QUEUE_DEMO=yes|-;inspect'
  'base|queue-execution|apps/base/queue|execution-walkthrough|none|BILIBI_DISPOSABLE_QUEUE_DEMO=yes|prepare;inspect'
  'base|queue-retention|apps/base/queue|retention-walkthrough|none|BILIBI_DISPOSABLE_QUEUE_DEMO=yes|-'
  'base|schedule|apps/base/schedule|schedule-walkthrough|seed|BILIBI_DISPOSABLE_SCHEDULE_DEMO=yes|dst;timing;review;prepare;coalesce;race 2026-06-03T10:00:00Z;overlap;execute 2026-06-04T12:00:10Z;inspect'
  'base|settings|apps/base/settings|settings-walkthrough|operator|BILIBI_DISPOSABLE_SETTINGS_DEMO=yes|-;inspect'
  'base|workflow-owner|apps/base/workflow|owner|migrate|BILIBI_DISPOSABLE_WORKFLOW_DEMO=yes|init;available 4 1;approve 4 1;history 4 1;proof-delivery 4 2;proof-maintenance 4;approval-init 4;proof-bell 4'

  'privacy|privacy-requests|apps/base/privacy|requests-walkthrough|none|BILIBI_DISPOSABLE_PRIVACY_DEMO=yes BILIBI_PRIVACY_DEMO_ROOT=@/privacy BILIBI_PRIVACY_DEMO_CHROMIUM=@chromium|seed;-'
  'privacy|privacy-failures|apps/base/privacy|failures-walkthrough|none|BILIBI_DISPOSABLE_PRIVACY_DEMO=yes BILIBI_PRIVACY_DEMO_ROOT=@/privacy BILIBI_PRIVACY_DEMO_CHROMIUM=@chromium|seed;-'
  'privacy|privacy-job-ends|apps/base/privacy|job-ends-walkthrough|none|BILIBI_DISPOSABLE_PRIVACY_DEMO=yes BILIBI_PRIVACY_DEMO_ROOT=@/privacy BILIBI_PRIVACY_DEMO_CHROMIUM=@chromium|seed;-'
  'privacy|privacy-repairs|apps/base/privacy|repairs-walkthrough|none|BILIBI_DISPOSABLE_PRIVACY_DEMO=yes BILIBI_PRIVACY_DEMO_ROOT=@/privacy BILIBI_PRIVACY_DEMO_CHROMIUM=@chromium|seed;-'
  'privacy|privacy-holds|apps/base/privacy|holds-walkthrough|none|BILIBI_DISPOSABLE_PRIVACY_DEMO=yes|-;report'
  'privacy|privacy-masking|apps/base/privacy|masking-walkthrough|none|BILIBI_DISPOSABLE_PRIVACY_DEMO=yes|-'
  'privacy|privacy-retention|apps/base/privacy|retention-walkthrough|none|BILIBI_DISPOSABLE_PRIVACY_DEMO=yes|seed;-'
  'privacy|privacy-swept-files|apps/base/privacy|swept-files-walkthrough|none|BILIBI_DISPOSABLE_PRIVACY_DEMO=yes BILIBI_PRIVACY_DEMO_ROOT=@/privacy|seed;-'
  'privacy|consent|apps/core/consent|consent-walkthrough|none|BILIBI_DISPOSABLE_PRIVACY_DEMO=yes BILIBI_PRIVACY_DEMO_ROOT=@/privacy BILIBI_PRIVACY_DEMO_CHROMIUM=@chromium|seed;-'

  'core|address|apps/core/address|address-walkthrough|geonames|BILIBI_ADDRESS_DEMO=disposable|-'
  'core|affiliation|apps/core/affiliation|affiliation-walkthrough|migrate|BILIBI_AFFILIATION_DEMO=disposable|-'
  'core|affiliation-contact-matching|apps/core/affiliation|contact-matching-walkthrough|none|BILIBI_CONTACT_MATCHING=disposable BILIBI_INSTANCE_DIR=@/instance|seed;-'
  'core|affiliation-operations|apps/core/affiliation|k19-operations-walkthrough|migrate|BILIBI_AFFILIATION_DEMO=disposable|-'
  'core|agent-access|apps/core/agent|agent-access-walkthrough|migrate|BILIBI_DISPOSABLE_AGENT_ACCESS_DEMO=yes|-'
  'core|company-caller-reads|apps/core/company|caller-reads-walkthrough|seed|BILIBI_CALLER_READS=disposable|-'
  'core|company-organisation|apps/core/company|k1-organisation-walkthrough|migrate|BILIBI_COMPANY_DEMO=disposable|-'
  'core|company-lifecycle|apps/core/company|k2-lifecycle-walkthrough|migrate|BILIBI_COMPANY_DEMO=disposable|-'
  'core|company-relationships|apps/core/company|k5-relationships-walkthrough|migrate|BILIBI_COMPANY_DEMO=disposable|-'
  'core|company-owner-binders|apps/core/company|k15-owner-binders|seed|BILIBI_COMPANY_DEMO=disposable BILIBI_K15_DEMO_ROOT=@/files|-'
  'core|currency|apps/core/currency|currency-walkthrough|seed|BILIBI_CURRENCY_DEMO=disposable|-'
  'core|document-audit|apps/core/document|audit-coverage|fixture-seed|BILIBI_DOCUMENT_AUDIT=disposable BILIBI_DOCUMENT_AUDIT_ROOT=@/documents|-'
  'core|document-export-rows|apps/core/document|export-rows-proof|fixture-seed|BILIBI_DOCUMENT_EXPORT_PROOF=disposable|-'
  # A latency measurement: run small here, only to prove it still runs.
  'core|document-latency|apps/core/document|list-latency|fixture-seed|BILIBI_DOCUMENT_LATENCY=disposable BILIBI_DOCUMENT_LATENCY_DOCUMENTS=300 BILIBI_DOCUMENT_LATENCY_RUNS=3|-'
  'core|document-pending-merge|apps/core/document|pending-merge-proof|fixture-seed|BILIBI_DOCUMENT_MERGE_PROOF=disposable BILIBI_DOCUMENT_MERGE_ROOT=@/documents|-'
  'core|document-customer-share|apps/core/document|customer-share-walkthrough|fixture-seed|BILIBI_DOCUMENT_CUSTOMER=disposable BILIBI_DOCUMENT_CUSTOMER_ROOT=@/documents|-'
  'core|document-scan-held|apps/core/document|scan-held-proof|fixture-seed|BILIBI_DOCUMENT_SCAN_PROOF=disposable BILIBI_DOCUMENT_SCAN_ROOT=@/documents|-'
  'core|geonames|apps/core/geonames|geonames-walkthrough|none|BILIBI_DISPOSABLE_GEONAMES_DEMO=yes|-'
  'core|geonames-import|apps/core/geonames|geonames-import-walkthrough|migrate|BILIBI_DISPOSABLE_GEONAMES_DEMO=yes|-'
  'core|geonames-postcodes|apps/core/geonames|geonames-postcodes-walkthrough|migrate|BILIBI_DISPOSABLE_GEONAMES_DEMO=yes|-'
  'core|party-contact|apps/core/party|contact-contract|none||-'
  'core|party-default-code|apps/core/party|default-code-contract|none||-'
  'core|person|apps/core/person|person-walkthrough|migrate|BILIBI_PERSON_DEMO=disposable|-'
  'core|person-party-identities|apps/core/person|party-identities|none|BILIBI_PARTY_PROOF=disposable|-'
  'core|person-duplicates|apps/core/person|person-duplicates|none|BILIBI_PERSON_PROOF=disposable|-'
  'core|person-custom-fields|apps/core/person|person-custom-fields|migrate|BILIBI_PERSON_DEMO=disposable|-|serve'
  'core|person-portal|apps/core/person|person-portal|migrate|BILIBI_PERSON_DEMO=disposable|-'
  'core|product|apps/core/product|product-walkthrough|seed|BILIBI_PRODUCT_WALKTHROUGH=disposable|-'
  'core|tenant-suspension|apps/core/tenant|tenant-suspension-walkthrough|none|BILIBI_TENANT_DEMO=disposable|-'
  'core|user-company-accounts|apps/core/user|company-accounts-walkthrough|migrate|BILIBI_COMPANY_ACCOUNTS_DEMO=disposable|-'
  'core|user-credentials|apps/core/user|credentials-walkthrough|migrate|BILIBI_DISPOSABLE_CREDENTIALS_DEMO=yes|-'
  'core|user-database-queries|apps/core/user|database-queries-walkthrough|migrate|BILIBI_DISPOSABLE_QUERY_DEMO=yes|-'
  'core|user-identity|apps/core/user|identity-walkthrough|migrate|BILIBI_IDENTITY_DEMO=disposable|-'
  'core|user-memberships|apps/core/user|memberships-walkthrough|migrate|BILIBI_MEMBERSHIPS_DEMO=disposable|-'
  'core|user-notifications|apps/core/user|notifications-walkthrough|migrate|BILIBI_DISPOSABLE_NOTIFICATIONS_DEMO=yes|-'
  # Installs the fixture and two disabled connections; the protocol itself needs
  # tokens made in a browser (scripts/scim-conformance.py).
  'core|user-scim|apps/core/user|scim-walkthrough|migrate|BILIBI_SCIM_WALKTHROUGH=disposable|-'
  'core|user-sso-agent-review|apps/core/user|sso-agent-review-walkthrough|migrate|BILIBI_SSO_AGENT_REVIEW_WALKTHROUGH=disposable|-'
  'core|user-staff-choices|apps/core/user|staff-choices-walkthrough|migrate|BILIBI_STAFF_CHOICES_DEMO=disposable|-'
  'core|work-calendar|apps/core/work_calendar|work-calendar-walkthrough|none|BILIBI_WORK_CALENDAR_DEMO=disposable|-'
  'core|work-calendar-arithmetic|apps/core/work_calendar|calendar-arithmetic-walkthrough|none|BILIBI_WORK_CALENDAR_DEMO=disposable|-'

  'web|credential-refusals|apps/web|credential-reference|none||--refusals'
  'web|portal-refusals|apps/web|portal-reference|none||--refusals'
  'web|public-endpoint-refusals|apps/web|public-endpoint-reference|none||--refusals'
  'web|scheduled-owner-catalog|apps/web|scheduled-owner-reference|none||catalog'
  'web|optional-capabilities-catalogs|apps/web|optional-capabilities|none||--catalogs'
  'web|optional-capabilities|apps/web|optional-capabilities|seed|BILIBI_OPTIONAL_PROOF=disposable|-'
  # Only the composition with nothing mounted; see NOT_RUN for the mounted ones.
  'web|events-unmounted|apps/web|events-reference|seed||-'
  'web|ports-unmounted|apps/web|ports-reference|none||-'
  'web|grid-catalog|apps/web|g1-grid|seed|BILIBI_GRID_DEMO=disposable|-'
  'web|grid-projection|apps/web|g2-grid|seed|BILIBI_GRID_DEMO=disposable|-'
  'web|grid-summary|apps/web|g5-grid-summary|migrate|BILIBI_GRID_DEMO=disposable|-'
  'web|live-wake|apps/web|live-wake-walkthrough|none|BILIBI_DISPOSABLE_LIVE_WAKE_DEMO=yes|-'
  'web|reports|apps/web|r1-reports|migrate|BILIBI_GRID_DEMO=disposable|-'
  'web|seams|apps/web|p1-seams|migrate||-'
  'web|locale-datetime|apps/web|p4-locale-datetime|migrate|BILIBI_DISPOSABLE_P4_DEMO=yes|-'
  'web|authz|apps/web|p6-authz|seed|BILIBI_AUTHZ_DEMO=disposable|-'
  'web|picklists|apps/web|picklists-reference|none||-'
  'web|records|apps/web|records-reference|none|-APP_ORIGIN|-'
  'web|records-equivalence|apps/web|records-equivalence|none||-'
  'web|role-assignment-bounds|apps/web|role-assignment-bounds|seed|BILIBI_ASSIGNMENT_PROOF=disposable|-'
  'web|system-principal-grants|apps/web|system-principal-grants|seed-app|BILIBI_PRINCIPAL_PROOF=disposable|-'
  'web|visitor|apps/web|visitor-walkthrough|migrate|BILIBI_DISPOSABLE_VISITOR_DEMO=yes BILIBI_MEDIA_DEMO_ROOT=@/media|-'
  'web|agent-edge|apps/web|agent-edge-walkthrough|migrate|BILIBI_DISPOSABLE_EDGE_DEMO=yes|-'
  'web|sign-in-throttle|apps/web|sign-in-throttle-walkthrough|migrate|BILIBI_DISPOSABLE_THROTTLE_DEMO=yes|-'
)

# Examples no line above runs, each with the reason. `module folder|example|reason`.
NOT_RUN=(
  # They only serve the app, with a reference page, for a person to use in a browser.
  'apps/core/document|upload-resume-proof|serves the New document dialog for a browser upload'
  'apps/core/person|person-contact-panel|serves a reference page only'
  'apps/web|dashboard-reference|serves a reference page only'
  'apps/web|download-reference|serves a reference page only'
  'apps/web|flex-reference|serves a reference page only'
  'apps/web|g3-lenses|serves a reference page only'
  'apps/web|g4-page-columns|serves a reference page only'
  'apps/web|list-reference|serves a reference page only'
  'apps/web|live-reference|serves a reference page only'
  'apps/web|schedule-reference|serves a reference page only'
  'apps/web|settings-editor-reference|serves a reference page only'
  'apps/web|softphone-walkthrough|serves a reference page only'
  'apps/web|u5-inline|serves a reference page only'
  'apps/web|w7-workflow-reference|serves a reference page only'
  # They need something a person or another run supplies.
  'apps/core/user|sso-walkthrough|needs a local Keycloak and the browser steps of apps/core/user/sso.md'
  'apps/web|delivery-walkthrough|needs an open customer invitation made in a running instance'
  'apps/web|import-results-walkthrough|its case file is private and not in the repository'
  'apps/web|owner-agent-standing|needs OAuth tokens a person obtains from the running /api/v1 edge'
  'apps/web|step-up-reference|needs the cookie of a signed-in browser session'
  # It needs a fixture container mounted and the workspace rebuilt for each
  # composition (fixtures/containers). So do the mounted compositions of
  # events-reference and ports-reference, whose unmounted runs are above.
  'apps/web|session-ended-walkthrough|needs fixtures/containers/session_listener mounted'
)

# Walkthroughs that fail on main today, each with the issue that tracks it:
# `name|issue number`. A listed walkthrough still runs. When it fails it is
# reported (a warning, and its output) and does not fail the run. When it
# passes, the run fails and says to remove its line, so the change that fixes
# a walkthrough deletes its line here. A failure that is not listed fails the
# run. Never add a line to get a pull request through: fix the walkthrough, or
# the code it found broken.
KNOWN_FAILURES=(
)

if [[ -n ${WALKTHROUGH_EXTRA:-} ]]; then
  while IFS= read -r line; do
    [[ -n ${line// /} ]] && WALKTHROUGHS+=("$line")
  done <<<"$WALKTHROUGH_EXTRA"
fi

# Every example under apps/ is in exactly one of the two lists, and every
# listed example exists.
check_lists() {
  local found listed status=0
  found=$(
    find apps -path '*/examples/*.rs' | while IFS= read -r file; do
      folder=${file%/examples/*}
      rest=${file#*/examples/}
      case "$rest" in
        */main.rs) echo "$folder|${rest%/main.rs}" ;;
        */*) ;; # a module of an example, not an example
        *) echo "$folder|${rest%.rs}" ;;
      esac
    done | sort -u
  )
  listed=$(
    {
      for entry in "${WALKTHROUGHS[@]}"; do
        IFS='|' read -r _ _ folder example _ <<<"$entry"
        echo "$folder|$example"
      done
      for entry in "${NOT_RUN[@]}"; do
        IFS='|' read -r folder example _ <<<"$entry"
        echo "$folder|$example"
      done
    } | sort -u
  )
  while IFS= read -r missing; do
    [[ -z $missing ]] && continue
    echo "::error::${missing%%|*}/examples/${missing#*|} is in neither WALKTHROUGHS nor NOT_RUN of scripts/run-walkthroughs.sh" >&2
    status=1
  done < <(comm -23 <(echo "$found") <(echo "$listed"))
  while IFS= read -r stale; do
    [[ -z $stale ]] && continue
    [[ -n ${WALKTHROUGH_ALLOW_MISSING:-} ]] && continue
    echo "::error::scripts/run-walkthroughs.sh lists ${stale%%|*}/examples/${stale#*|}, which does not exist" >&2
    status=1
  done < <(comm -13 <(echo "$found") <(echo "$listed"))
  for entry in "${KNOWN_FAILURES[@]}"; do
    name=${entry%%|*}
    for run in "${WALKTHROUGHS[@]}"; do
      IFS='|' read -r _ run_name _ <<<"$run"
      [[ $run_name == "$name" ]] && continue 2
    done
    echo "::error::KNOWN_FAILURES of scripts/run-walkthroughs.sh names $name, which is not a walkthrough" >&2
    status=1
  done
  return $status
}

# The issue of a known failure, or nothing.
known_issue() {
  local entry
  for entry in "${KNOWN_FAILURES[@]}"; do
    [[ ${entry%%|*} == "$1" ]] && echo "${entry#*|}" && return 0
  done
  return 0
}

if [[ ${1:-} == --check ]]; then
  check_lists
  echo "Every example is listed: ${#WALKTHROUGHS[@]} runs, ${#NOT_RUN[@]} examples not run"
  exit 0
fi
check_lists

: "${DATABASE_URL:?DATABASE_URL must name a PostgreSQL 18 role that may create databases}"
target=${CARGO_TARGET_DIR:-$root/target}
timeout_seconds=${WALKTHROUGH_TIMEOUT:-900}
base_url=${DATABASE_URL%/*}
admin_url="$base_url/postgres"
work=$(mktemp -d)
logs=${WALKTHROUGH_LOGS:-$work/logs}
mkdir -p "$logs"
app="$target/debug/bilibi-web"
fixture="$target/debug/bilibi-walkthrough-fixture"
chromium=${WALKTHROUGH_CHROMIUM:-$(command -v chromium || command -v chromium-browser || command -v google-chrome || true)}
qpdf=${WALKTHROUGH_QPDF:-$(command -v qpdf || true)}
model=${WALKTHROUGH_LANGUAGE_MODEL:-}

step() { printf '\n== %s\n' "$*"; }

free_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
PY
}

package_of() {
  sed -n 's/^name *= *"\(.*\)"/\1/p' "$1/Cargo.toml" | head -n 1
}

# The entries this run covers, in list order.
selected=("$@")
entries=()
for entry in "${WALKTHROUGHS[@]}"; do
  IFS='|' read -r group name folder example _ <<<"$entry"
  if [[ ${#selected[@]} -gt 0 ]]; then
    [[ " ${selected[*]} " == *" $name "* ]] || continue
  fi
  if [[ -n ${WALKTHROUGH_GROUP:-} && $group != "$WALKTHROUGH_GROUP" ]]; then
    continue
  fi
  if [[ ! -d $folder/examples ]] || { [[ ! -f $folder/examples/$example.rs ]] && [[ ! -f $folder/examples/$example/main.rs ]]; }; then
    # Only reachable with WALKTHROUGH_ALLOW_MISSING: check_lists refused it otherwise.
    echo "SKIP $name: $folder/examples/$example is not in this checkout"
    continue
  fi
  entries+=("$entry")
done
if [[ ${#entries[@]} -eq 0 ]]; then
  if [[ -n ${WALKTHROUGH_GROUP:-} ]]; then
    echo "No selected walkthrough is in the $WALKTHROUGH_GROUP group"
    exit 0
  fi
  echo 'No walkthrough matches' >&2
  exit 1
fi

# records-equivalence proves a pinned composition, which the build compiles in.
# Pinning refuses a checkout with uncommitted changes; the other walkthroughs
# do not need the pin, so that is not a reason to run none of them.
step 'Pin the composition and build the app and the fixture'
cargo metadata --format-version 1 >/dev/null
python3 scripts/discover-modules.py --pin ||
  echo '::warning::The composition is not pinned, so records-equivalence will fail'
cargo build --locked -p bilibi-web -p bilibi-walkthrough-fixture --bins

# Serves apps/core/geonames/sample in the layout `geonames-import` downloads.
geonames_server() {
  local folder=$1 port=$2
  mkdir -p "$folder/dump" "$folder/zip"
  cp apps/core/geonames/sample/countryInfo.txt apps/core/geonames/sample/admin1CodesASCII.txt "$folder/dump/"
  (cd apps/core/geonames/sample && zip -q "$folder/dump/cities15000.zip" cities15000.txt && zip -q "$folder/zip/MY.zip" MY.txt)
  python3 -m http.server "$port" --bind 127.0.0.1 --directory "$folder" >/dev/null 2>&1 &
  helper=$!
  for _ in $(seq 1 50); do
    (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && return 0
    sleep 0.2
  done
  echo 'The local GeoNames server never answered' >&2
  return 1
}

# Runs one walkthrough. Sets $verdict to pass, fail, timeout or setup.
run_one() {
  local name=$1 example=$2 prepare=$3 environment=$4 steps=$5 kind=$6
  local database="walkthrough_${name//[^a-z0-9]/_}_$$"
  local dir="$work/$name" log="$logs/$name.log"
  local port pair key value argv pid deadline
  helper=''
  verdict=''
  mkdir -m 700 "$dir"
  port=$(free_port)
  psql "$admin_url" -v ON_ERROR_STOP=1 -qc "CREATE DATABASE $database"
  (
    # Everything a step sees. The keys are random per walkthrough and guard
    # nothing: the database and folders are dropped when it ends.
    export DATABASE_URL="$base_url/$database"
    export APP_ORIGIN="http://127.0.0.1:$port"
    export BILIBI_ENV=development
    export BILIBI_MAIL_DIR="$dir/mail"
    BILIBI_SETTINGS_KEY=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
    BILIBI_USER_SIGNING_KEY=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
    BILIBI_MEDIA_SIGNING_KEY=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
    export BILIBI_SETTINGS_KEY BILIBI_USER_SIGNING_KEY BILIBI_MEDIA_SIGNING_KEY
    mkdir -m 700 "$dir/mail" "$dir/cwd"
    [[ $kind == serve ]] && export BIND_ADDR="127.0.0.1:$port"
    for pair in $environment; do
      if [[ $pair == -* ]]; then
        unset "${pair#-}"
        continue
      fi
      key=${pair%%=*}
      value=${pair#*=}
      case "$value" in
        @chromium) value=$chromium ;;
        @qpdf) value=$qpdf ;;
        @port) value=$(free_port) ;;
        @/*) value="$dir/${value#@/}" && mkdir -m 700 "$value" ;;
      esac
      export "$key=$value"
    done
    # A walkthrough that keeps files beside itself gets a folder of its own.
    cd "$dir/cwd"

    echo "-- prepare: $prepare"
    case "$prepare" in
      none) ;;
      migrate) "$app" migrate ;;
      seed | seed-app) "$app" migrate && "$app" seed-production ;;
      fixture-seed) "$app" migrate && "$fixture" && "$app" seed-production ;;
      operator)
        "$app" migrate && "$fixture" &&
          psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -qc \
            'INSERT INTO platform_operator(singleton,tenant_id) VALUES(true,1) ON CONFLICT DO NOTHING'
        ;;
      geonames)
        geonames_port=$(free_port)
        "$app" migrate &&
          (cd "$root" && geonames_server "$dir/geonames" "$geonames_port" && echo "$helper" >"$dir/helper.pid") &&
          "$app" geonames-import --base-url "http://127.0.0.1:$geonames_port/dump" \
            --postcodes-url "http://127.0.0.1:$geonames_port/zip" --postcodes MY --cache-dir "$dir/geonames-cache"
        ;;
      *) echo "Unknown prepare '$prepare'" >&2 && exit 2 ;;
    esac || exit 97
    if [[ $prepare == seed-app ]]; then
      BIND_ADDR="127.0.0.1:$port" "$app" run &
      echo $! >"$dir/helper.pid"
      for _ in $(seq 1 60); do
        (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break
        sleep 1
      done
    fi

    IFS=';' read -ra runs <<<"$steps"
    last=$((${#runs[@]} - 1))
    for index in "${!runs[@]}"; do
      argv=()
      for word in ${runs[$index]}; do
        case "$word" in
          -) ;;
          @model) argv+=("$model") ;;
          @/*) argv+=("$dir/${word#@/}") ;;
          *) argv+=("$word") ;;
        esac
      done
      echo "-- step: $example ${argv[*]}"
      if [[ $kind == serve && $index == "$last" ]]; then
        exec "$target/debug/examples/$example" "${argv[@]}"
      fi
      "$target/debug/examples/$example" "${argv[@]}" || exit $?
    done
  ) >"$log" 2>&1 &
  pid=$!
  deadline=$((SECONDS + timeout_seconds))
  while ((SECONDS < deadline)); do
    if ! kill -0 "$pid" 2>/dev/null; then
      if wait "$pid"; then
        verdict=pass
      elif [[ $? == 97 ]]; then
        verdict=setup
      else
        verdict=fail
      fi
      break
    fi
    # A serving walkthrough binds only after its proofs passed.
    if [[ $kind == serve ]] && (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
      verdict=pass
      break
    fi
    sleep 1
  done
  [[ -z $verdict ]] && verdict=timeout
  # Stop only what this walkthrough started, by process ID.
  if kill -0 "$pid" 2>/dev/null; then
    children=$(ps -o pid= --ppid "$pid" || true)
    # shellcheck disable=SC2086 # one process ID per word
    kill "$pid" $children 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  if [[ -s $dir/helper.pid ]]; then
    kill "$(cat "$dir/helper.pid")" 2>/dev/null || true
  fi
  psql "$admin_url" -qc "DROP DATABASE IF EXISTS $database WITH (FORCE)" || true
  rm -rf "$dir"
}

failed=()
known=()
fixed=()
passed=0
summary=${GITHUB_STEP_SUMMARY:-/dev/null}
printf '| Walkthrough | Result | Seconds |\n| --- | --- | --- |\n' >>"$summary"

# One module at a time: build its examples, run them, remove them.
folders=()
for entry in "${entries[@]}"; do
  IFS='|' read -r _ _ folder _ <<<"$entry"
  [[ " ${folders[*]} " == *" $folder "* ]] || folders+=("$folder")
done
for folder in "${folders[@]}"; do
  package=$(package_of "$folder")
  examples=()
  for entry in "${entries[@]}"; do
    IFS='|' read -r _ _ entry_folder example _ <<<"$entry"
    [[ $entry_folder == "$folder" ]] || continue
    [[ " ${examples[*]} " == *" --example $example "* ]] || examples+=(--example "$example")
  done
  step "Build the walkthroughs of $folder ($package)"
  cargo build --locked -p "$package" "${examples[@]}"
  for entry in "${entries[@]}"; do
    IFS='|' read -r _ name entry_folder example prepare environment steps kind <<<"$entry"
    [[ $entry_folder == "$folder" ]] || continue
    step "Walkthrough: $name ($folder/examples/$example)"
    if [[ $steps == *@model* && ! -f $model ]]; then
      # The model is a 131 MB download from its publisher; its absence says
      # nothing about this repository.
      echo "::warning title=Walkthrough $name skipped::the pinned language model is not provisioned (WALKTHROUGH_LANGUAGE_MODEL)"
      printf '| %s | skipped: no language model | 0 |\n' "$name" >>"$summary"
      continue
    fi
    started=$SECONDS
    run_one "$name" "$example" "$prepare" "$environment" "${steps:--}" "${kind:-}"
    elapsed=$((SECONDS - started))
    issue=$(known_issue "$name")
    if [[ $verdict == pass && -n $issue ]]; then
      echo "::error title=Walkthrough $name passes::remove its line from KNOWN_FAILURES in scripts/run-walkthroughs.sh (issue #$issue)"
      echo "PASS $name (${elapsed}s), but it is listed as a known failure: remove it from the known list"
      fixed+=("$name")
      printf '| %s | **passes: remove it from the known list (#%s)** | %s |\n' "$name" "$issue" "$elapsed" >>"$summary"
    elif [[ $verdict == pass ]]; then
      echo "PASS $name (${elapsed}s)"
      passed=$((passed + 1))
      printf '| %s | pass | %s |\n' "$name" "$elapsed" >>"$summary"
    elif [[ -n $issue ]]; then
      echo "::warning title=Walkthrough $name still fails::known failure, issue #$issue ($folder/examples/$example)"
      echo "KNOWN FAILURE $name ($verdict, ${elapsed}s), issue #$issue; last output:"
      tail -n 20 "$logs/$name.log"
      known+=("$name")
      printf '| %s | known failure (#%s) | %s |\n' "$name" "$issue" "$elapsed" >>"$summary"
    else
      case "$verdict" in
        setup) reason='its database could not be prepared' ;;
        timeout) reason="it did not finish in ${timeout_seconds}s" ;;
        *) reason='a step exited unsuccessfully' ;;
      esac
      echo "::error title=Walkthrough $name failed::$folder/examples/$example: $reason"
      echo "FAIL $name ($verdict, ${elapsed}s); last output:"
      tail -n 60 "$logs/$name.log"
      failed+=("$name")
      printf '| %s | **%s** | %s |\n' "$name" "$verdict" "$elapsed" >>"$summary"
    fi
  done
  if [[ -z ${WALKTHROUGH_KEEP:-} ]]; then
    for flag in "${examples[@]}"; do
      [[ $flag == --example ]] && continue
      rm -f "$target/debug/examples/$flag" "$target/debug/examples/${flag//-/_}"-*
    done
  fi
done

printf '\n%s passed' "$passed"
[[ ${#known[@]} -gt 0 ]] && printf '; known failures, still failing: %s' "${known[*]}"
printf '\n'
status=0
if [[ ${#fixed[@]} -gt 0 ]]; then
  printf 'Listed as known failures but passing; remove from KNOWN_FAILURES: %s\n' "${fixed[*]}" >&2
  status=1
fi
if [[ ${#failed[@]} -gt 0 ]]; then
  printf 'Failed walkthroughs: %s\nLogs: %s\n' "${failed[*]}" "$logs" >&2
  status=1
fi
[[ $status == 0 ]] && rm -rf "$work"
exit $status
