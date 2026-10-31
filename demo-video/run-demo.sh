#!/usr/bin/env bash
# run-demo.sh — the ERPCraft 6-step demo, scripted for recording.
#
# Runs the exact walkthrough from README.md ("The demo script")
# against a fresh database and captures the whole session —
# commands and real output, nothing staged — to demo-transcript.txt
# next to this script. Confirmations at the agent gate are answered
# by piping "y" (shown in the transcript); the gate prints its
# confirmation before anything posts, exactly as interactively.
#
# Usage: ./run-demo.sh [db-path]     (default db: /tmp/erpcraft-demo-video.db)
set -u
cd "$(dirname "$0")/.."
export PATH="$PWD/target/debug:$PATH"
DB="${1:-/tmp/erpcraft-demo-video.db}"
export ERPCRAFT_DB="$DB"
TRANSCRIPT="$PWD/demo-video/demo-transcript.txt"

run() {
    echo
    echo "\$ $*"
    "$@" 2>&1
    echo "[exit $?]"
}

{
    echo "ERPCraft — the 6-step demo, recorded from a fresh database."
    echo "Driver: demo-video/run-demo.sh · Binary: target/debug/erpcraft (cargo build -p erpcraft-cli)"
    rm -f "$DB"
    echo
    echo "# Setup (README): fresh database; the CLI seeds Cascade Precision Parts on first open."
    echo "\$ export ERPCRAFT_DB=$DB; rm -f \$ERPCRAFT_DB"

    echo
    echo "# ── Step 1 · Seed + health, and the opening books ─────────────"
    run erpcraft health
    run erpcraft trial_balance

    echo
    echo "# ── Step 2 · Configured quote: steel + galvanized forces the reinforced mount, \$162.00/unit ──"
    run erpcraft create_quote --json '{"customer_id":1,"lines":[{"item_id":1,"qty":50,"configuration":{"material":"steel","finish":"galvanized"}}]}'
    run erpcraft send_quote 1
    run erpcraft accept_quote 1
    run erpcraft create_order 1
    echo
    echo "# …and the impossible variant refuses, with its reason:"
    run erpcraft evaluate_config --json '{"model_id":1,"configuration":{"material":"aluminum","finish":"galvanized"}}'

    echo
    echo "# ── Step 3 · The agent invoices — propose, confirm, then it posts ──"
    echo
    echo "\$ printf 'y\\n' | erpcraft agent \"Invoice Acme for the 40 units we shipped Tuesday.\""
    printf 'y\n' | erpcraft agent "Invoice Acme for the 40 units we shipped Tuesday." 2>&1
    echo "[exit $?]"

    echo
    echo "# ── Step 4 · MRP check: a live plan, and the shortage it anticipates ──"
    echo
    echo "\$ printf 'y\\n' | erpcraft agent \"Can we build 200 BRK-100 by 2026-11-15?\""
    printf 'y\n' | erpcraft agent "Can we build 200 BRK-100 by 2026-11-15?" 2>&1
    echo "[exit $?]"
    run erpcraft explain_shortage --sku FST-SET --as-of 2026-11-12 --demand-sku BRK-100 --demand-qty 200 --demand-date 2026-11-15

    echo
    echo "# ── Step 5 · Gated release: creates the purchase-order document, moves no money ──"
    echo
    echo "\$ erpcraft trial_balance | totals   (before release)"
    erpcraft trial_balance 2>&1 | grep -E 'total_(debit|credit)_cents'
    echo
    echo "\$ printf 'y\\n' | erpcraft agent \"Release purchase:FST-SET:2026-11-10:110 from the 200 BRK-100 by 2026-11-15 plan.\""
    printf 'y\n' | erpcraft agent "Release purchase:FST-SET:2026-11-10:110 from the 200 BRK-100 by 2026-11-15 plan." 2>&1
    echo "[exit $?]"
    echo
    echo "\$ erpcraft trial_balance | totals   (after release — unchanged: planning never posts)"
    erpcraft trial_balance 2>&1 | grep -E 'total_(debit|credit)_cents'

    echo
    echo "# ── Step 6 · Audit trail: who posted it, why, and the verbatim prompt ──"
    run erpcraft agent --yes "Buy \$3,600 of raw materials on credit."
    run erpcraft audit_trail --source agent
    echo
    echo "# …and the provenance chain of the invoice's revenue entry from step 3:"
    run erpcraft audit_entry 2

    echo
    echo "# End of demo. Final books:"
    run erpcraft trial_balance
} > "$TRANSCRIPT" 2>&1

echo "Transcript written to $TRANSCRIPT"
