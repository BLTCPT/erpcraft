//! The Day-3 oracle suite: replay the scripted business day from
//! `erpcraft-oracle-day-spec.md` step for step — engine postings through
//! the order-to-cash transitions (T0/T1/T8/T10/T11), the rest through the
//! manual journal path, exactly as a bookkeeper would post them — and
//! assert the ending trial balance equals the hand-computed oracle,
//! per account: $129,975.00 = $129,975.00.

use erpcraft_core::rusqlite::Connection;
use erpcraft_core::{
    accept_quote, create_invoice_from_order, create_order_from_quote, create_quote, entry_count,
    get_entry, get_invoice, issue_invoice, list_accounts, list_customers, list_entries, list_items,
    open_db, post_entry, record_payment, record_sales_return, send_quote, trial_balance,
    JournalEntryView, NewEntry, NewLine, NewQuote, NewQuoteLine, ReturnLine, BRACKET_SKU,
    OPENING_TOTAL_CENTS,
};

const ORACLE_DATE: &str = "2026-10-28";

fn customer_id(conn: &Connection, name: &str) -> i64 {
    list_customers(conn)
        .unwrap()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("customer {name} is seeded"))
        .id
}

fn bracket_id(conn: &Connection) -> i64 {
    list_items(conn)
        .unwrap()
        .into_iter()
        .find(|i| i.sku == BRACKET_SKU)
        .expect("bracket item is seeded")
        .id
}

/// A manual journal entry through the existing post path (T2–T7, T9,
/// T12–T14 of the oracle day are deliberately NOT engine postings).
fn manual(conn: &mut Connection, memo: &str, lines: Vec<(&str, i64, i64)>) -> i64 {
    post_entry(
        conn,
        &NewEntry {
            entry_date: ORACLE_DATE.to_string(),
            memo: memo.to_string(),
            source: "manual".to_string(),
            reverses_entry_id: None,
            lines: lines
                .into_iter()
                .map(|(code, debit, credit)| NewLine {
                    account_code: code.to_string(),
                    debit_cents: debit,
                    credit_cents: credit,
                })
                .collect(),
        },
    )
    .unwrap()
}

/// Quote → order → draft invoice for `qty` brackets; returns the invoice id.
fn draft_invoice(conn: &mut Connection, customer: &str, qty: i64) -> i64 {
    let new_quote = NewQuote {
        customer_id: customer_id(conn, customer),
        lines: vec![NewQuoteLine {
            item_id: bracket_id(conn),
            qty,
            configuration: None,
        }],
    };
    let quote_id = create_quote(conn, &new_quote).unwrap();
    send_quote(conn, quote_id).unwrap();
    accept_quote(conn, quote_id).unwrap();
    let order_id = create_order_from_quote(conn, quote_id).unwrap();
    create_invoice_from_order(conn, order_id).unwrap()
}

/// Assert an entry's lines are exactly (account, debit, credit), in order.
fn assert_lines(entry: &JournalEntryView, expected: &[(&str, i64, i64)]) {
    assert_eq!(
        entry.lines.len(),
        expected.len(),
        "entry {} line count",
        entry.id
    );
    for (line, (code, debit, credit)) in entry.lines.iter().zip(expected.iter()) {
        assert_eq!(line.account_code, *code, "entry {} account", entry.id);
        assert_eq!(line.debit_cents, *debit, "entry {} {code} debit", entry.id);
        assert_eq!(
            line.credit_cents, *credit,
            "entry {} {code} credit",
            entry.id
        );
    }
}

fn entries_with_source(conn: &Connection, source: &str) -> Vec<JournalEntryView> {
    list_entries(conn)
        .unwrap()
        .into_iter()
        .filter(|e| e.source == source)
        .collect()
}

#[test]
fn oracle_day_replays_to_the_hand_computed_trial_balance() {
    let mut conn = open_db(":memory:").unwrap();

    // -- Opening state: $120,500.00 per side, one opening entry. --------
    let tb = trial_balance(&conn).unwrap();
    assert_eq!(tb.total_debit_cents, OPENING_TOTAL_CENTS);
    assert_eq!(tb.total_credit_cents, OPENING_TOTAL_CENTS);
    assert_eq!(entry_count(&conn).unwrap(), 1);

    // -- T0: quote Acme for 40 brackets — NO posting. -------------------
    let acme_invoice = draft_invoice(&mut conn, "Acme", 40);
    assert_eq!(entry_count(&conn).unwrap(), 1, "T0/T-order post nothing");

    // -- T1: ship + invoice Acme (engine: revenue + COGS at issuance). --
    issue_invoice(&mut conn, acme_invoice).unwrap();
    let acme = get_invoice(&conn, acme_invoice).unwrap().unwrap();
    assert_eq!(acme.total_cents, 500_000);
    let t1 = entries_with_source(&conn, &format!("invoice:{acme_invoice}"));
    assert_eq!(t1.len(), 2, "issuance posts exactly two entries");
    let t1_revenue = t1
        .iter()
        .find(|e| Some(e.id) == acme.gl_entry_id)
        .expect("the revenue entry is the one recorded on the invoice");
    assert_lines(t1_revenue, &[("1100", 500_000, 0), ("4000", 0, 500_000)]);
    let t1_cogs = t1
        .iter()
        .find(|e| Some(e.id) == acme.cogs_entry_id)
        .expect("the COGS entry is the one recorded on the invoice");
    assert_lines(t1_cogs, &[("5000", 272_000, 0), ("1220", 0, 272_000)]);

    // -- T2: Beta pays $6,200.00 against opening AR (manual). -----------
    manual(
        &mut conn,
        "T2 Beta pays against opening AR",
        vec![("1000", 620_000, 0), ("1100", 0, 620_000)],
    );
    // -- T3: raw materials on credit (manual). --------------------------
    // SPEC DISCREPANCY, resolved in favor of the oracle's ending state:
    // the spec's T3 narrative says $3,800.00, but its own hand-computed
    // ending trial balance (RM $9,300.00, AP $9,200.00, totals
    // $129,975.00) and both cross-checks (net assets $116,616.00,
    // liabilities $35,700.00) are consistent ONLY with $3,600.00:
    // RM 8,600 + 3,600 − 2,900 = 9,300 and AP 9,700 + 3,600 − 4,100 =
    // 9,200, whereas $3,800 leaves both accounts $200 high and the day
    // at $130,175. The ending state is the acceptance oracle (DAY3_PREP,
    // AGENTS.md), so the replay uses $3,600.00.
    manual(
        &mut conn,
        "T3 raw materials on credit",
        vec![("1200", 360_000, 0), ("2000", 0, 360_000)],
    );
    // -- T4: pay a supplier (manual). -----------------------------------
    manual(
        &mut conn,
        "T4 pay supplier",
        vec![("2000", 410_000, 0), ("1000", 0, 410_000)],
    );
    // -- T5: issue materials to production (manual). --------------------
    manual(
        &mut conn,
        "T5 materials to WIP",
        vec![("1210", 290_000, 0), ("1200", 0, 290_000)],
    );
    // -- T6: direct labor capitalized to WIP (manual). ------------------
    manual(
        &mut conn,
        "T6 direct labor",
        vec![("1210", 180_000, 0), ("1000", 0, 180_000)],
    );
    // -- T7: complete production — 50 brackets to Finished Goods. -------
    manual(
        &mut conn,
        "T7 production completed",
        vec![("1220", 410_000, 0), ("1210", 0, 410_000)],
    );

    // -- T8: second sale — ship + invoice Delta, 25 brackets (engine). --
    let delta_invoice = draft_invoice(&mut conn, "Delta", 25);
    issue_invoice(&mut conn, delta_invoice).unwrap();
    let delta = get_invoice(&conn, delta_invoice).unwrap().unwrap();
    assert_eq!(delta.total_cents, 312_500);
    let t8_revenue_id = delta.gl_entry_id.unwrap();
    let t8_cogs_id = delta.cogs_entry_id.unwrap();
    assert_lines(
        &get_entry(&conn, t8_revenue_id).unwrap().unwrap(),
        &[("1100", 312_500, 0), ("4000", 0, 312_500)],
    );
    assert_lines(
        &get_entry(&conn, t8_cogs_id).unwrap().unwrap(),
        &[("5000", 170_000, 0), ("1220", 0, 170_000)],
    );
    // Snapshot T8's entries so T11 can prove they are byte-unchanged.
    let t8_revenue_before =
        serde_json::to_value(get_entry(&conn, t8_revenue_id).unwrap().unwrap()).unwrap();
    let t8_cogs_before =
        serde_json::to_value(get_entry(&conn, t8_cogs_id).unwrap().unwrap()).unwrap();

    // -- T9: Epsilon deposit — a liability, NOT revenue (manual). -------
    manual(
        &mut conn,
        "T9 Epsilon customer deposit",
        vec![("1000", 150_000, 0), ("2200", 0, 150_000)],
    );

    // -- T10: Acme pays the T1 invoice in full (engine). ----------------
    let payment_id = record_payment(&mut conn, acme_invoice, 500_000).unwrap();
    let t10 = entries_with_source(&conn, &format!("payment:{payment_id}"));
    assert_eq!(t10.len(), 1);
    assert_lines(&t10[0], &[("1000", 500_000, 0), ("1100", 0, 500_000)]);
    assert_eq!(
        get_invoice(&conn, acme_invoice).unwrap().unwrap().status,
        "paid"
    );

    // -- T11: Delta returns 2 defective brackets (correcting entries). --
    let bracket = bracket_id(&conn);
    let (ret_revenue_id, ret_cogs_id) = record_sales_return(
        &mut conn,
        delta_invoice,
        &[ReturnLine {
            item_id: bracket,
            qty: 2,
        }],
    )
    .unwrap();
    let ret_revenue = get_entry(&conn, ret_revenue_id).unwrap().unwrap();
    assert_eq!(ret_revenue.source, format!("return:{delta_invoice}"));
    assert_eq!(
        ret_revenue.reverses_entry_id,
        Some(t8_revenue_id),
        "the revenue reversal points at T8's revenue entry"
    );
    assert_lines(&ret_revenue, &[("4010", 25_000, 0), ("1100", 0, 25_000)]);
    let ret_cogs = get_entry(&conn, ret_cogs_id).unwrap().unwrap();
    assert_eq!(ret_cogs.source, format!("return:{delta_invoice}"));
    assert_eq!(
        ret_cogs.reverses_entry_id,
        Some(t8_cogs_id),
        "the COGS reversal points at T8's COGS entry"
    );
    assert_lines(&ret_cogs, &[("1220", 13_600, 0), ("5000", 0, 13_600)]);
    // T8's originals are byte-unchanged — corrections never edit.
    assert_eq!(
        serde_json::to_value(get_entry(&conn, t8_revenue_id).unwrap().unwrap()).unwrap(),
        t8_revenue_before
    );
    assert_eq!(
        serde_json::to_value(get_entry(&conn, t8_cogs_id).unwrap().unwrap()).unwrap(),
        t8_cogs_before
    );

    // -- T12–T14: rent, interest, depreciation (manual). ----------------
    manual(
        &mut conn,
        "T12 shop rent",
        vec![("6000", 120_000, 0), ("1000", 0, 120_000)],
    );
    manual(
        &mut conn,
        "T13 loan interest",
        vec![("6010", 12_500, 0), ("1000", 0, 12_500)],
    );
    manual(
        &mut conn,
        "T14 depreciation",
        vec![("6020", 35_000, 0), ("1310", 0, 35_000)],
    );

    // 17 oracle-day entries + the opening entry.
    assert_eq!(entry_count(&conn).unwrap(), 18);

    // -- Ending trial balance: the oracle, PER ACCOUNT. -----------------
    let tb = trial_balance(&conn).unwrap();
    let expected: &[(&str, i64, i64)] = &[
        ("1000", 5_367_500, 0), // Cash $53,675.00
        ("1100", 907_500, 0),   // AR $9,075.00
        ("1120", 0, 0),         // Prepaid Insurance — stays zero
        ("1200", 930_000, 0),   // Raw Materials $9,300.00
        ("1210", 60_000, 0),    // WIP $600.00 (the partial job)
        ("1220", 911_600, 0),   // Finished Goods $9,116.00
        ("1300", 4_200_000, 0), // Equipment $42,000.00
        ("1310", 0, 715_000),   // Accum. Depreciation $7,150.00
        ("2000", 0, 920_000),   // AP $9,200.00
        ("2100", 0, 0),         // Sales Tax Payable — stays zero
        ("2200", 0, 150_000),   // Customer Deposits $1,500.00
        ("2300", 0, 2_500_000), // Loan Payable $25,000.00
        ("2400", 0, 0),         // Wages Payable — stays zero
        ("3000", 0, 6_500_000), // Owner's Equity $65,000.00
        ("3100", 0, 1_400_000), // Retained Earnings $14,000.00
        ("4000", 0, 812_500),   // Sales Revenue $8,125.00
        ("4010", 25_000, 0),    // Sales Returns $250.00
        ("5000", 428_400, 0),   // COGS $4,284.00
        ("6000", 120_000, 0),   // Rent $1,200.00
        ("6010", 12_500, 0),    // Interest $125.00
        ("6020", 35_000, 0),    // Depreciation $350.00
        ("6030", 0, 0),         // Supplies — stays zero
    ];
    assert_eq!(tb.rows.len(), expected.len());
    for (row, (code, debit, credit)) in tb.rows.iter().zip(expected.iter()) {
        assert_eq!(row.code, *code);
        assert_eq!(row.debit_cents, *debit, "account {code} debit");
        assert_eq!(row.credit_cents, *credit, "account {code} credit");
    }
    assert_eq!(tb.total_debit_cents, 12_997_500); // $129,975.00
    assert_eq!(tb.total_credit_cents, 12_997_500);

    // -- Cross-checks from the oracle spec. ------------------------------
    let accounts = list_accounts(&conn).unwrap();
    let balance = |code: &str| {
        accounts
            .iter()
            .find(|a| a.code == code)
            .unwrap()
            .balance_cents
    };
    // Net income: revenue − returns − COGS − rent − interest − depreciation.
    let net_income = 812_500 - 25_000 - 428_400 - 120_000 - 12_500 - 35_000;
    assert_eq!(net_income, 191_600); // $1,916.00
                                     // The same net income falls out of the account balances (signed,
                                     // debits − credits): revenue is a credit balance, the rest debits.
    let net_income_from_ledger = -(balance("4000")
        + balance("4010")
        + balance("5000")
        + balance("6000")
        + balance("6010")
        + balance("6020"));
    assert_eq!(net_income_from_ledger, net_income);

    // Accounting equation: net assets = liabilities + equity + net income.
    let net_assets: i64 = accounts
        .iter()
        .filter(|a| a.account_type == "asset" || a.account_type == "contra_asset")
        .map(|a| a.balance_cents)
        .sum();
    let liabilities: i64 = -accounts
        .iter()
        .filter(|a| a.account_type == "liability")
        .map(|a| a.balance_cents)
        .sum::<i64>();
    let equity: i64 = -accounts
        .iter()
        .filter(|a| a.account_type == "equity")
        .map(|a| a.balance_cents)
        .sum::<i64>();
    assert_eq!(net_assets, 11_661_600); // $116,616.00
    assert_eq!(liabilities, 3_570_000); // $35,700.00
    assert_eq!(equity, 7_900_000); // $79,000.00
    assert_eq!(net_assets, liabilities + equity + net_income);
}
