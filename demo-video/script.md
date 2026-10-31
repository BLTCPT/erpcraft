# ERPCraft demo video — narration script

- **Target runtime:** ~2.5 minutes (390 words at a natural pace)
- **Voice:** single narrator, calm and confident; this file was also
  synthesized as `narration.mp3` (see below)
- **Picture:** terminal running `demo-video/run-demo.sh`; the screen
  shows `demo-video/demo-transcript.txt` — every number the narration
  cites is on screen when it is spoken.
- **How to re-record:** the fenced block below is the exact spoken
  text (also saved as `narration.txt`, one `Narrator:` turn per
  paragraph, ready for `tts synthesize-script`). Numbers and
  initialisms are written as they should sound.

## Spoken text

```text
Narrator: This is ERP Craft — a free, open-source enterprise resource planning system, written in Rust, and A I native from day one. Our company is Cascade Precision Parts, a small bracket manufacturer in Oregon. Its opening books balance at one hundred twenty thousand five hundred dollars on each side. And that balance is not the application code's opinion: the double-entry rule lives in the database itself, so no path in — not the A P I, not the command line, not the agent — can post an entry that doesn't balance.
Narrator: Step two: a configured quote. Fifty steel brackets, galvanized finish. The configurator — its dependency rules modeled on S A P variant configuration — forces the reinforced mount, because a named constraint says orders over forty units need it, and it rolls the chosen components into the price: one hundred sixty-two dollars a unit. Then I ask for the impossible version, galvanized on aluminum, and it refuses in plain language: galvanized finish is available only on steel brackets.
Narrator: Step three: the agent. I say — invoice Acme for the forty units we shipped Tuesday. The agent proposes the posting, rendered from the ledger itself, never from model prose: five thousand dollars of receivables and revenue, two thousand seven hundred twenty of cost of goods sold. Total debits equal total credits, seven thousand seven hundred twenty dollars. And nothing posts until I confirm. That is the contract: agents propose, humans confirm, then it posts.
Narrator: Step four: planning. Can we build two hundred brackets by November fifteenth? M R P here is dynamic: the plan is computed live from open orders, stock, and bills of material — never stored. Demand is two hundred fifty; fifty are already covered; the plan builds two hundred and nets the components it needs. Move the calendar forward, and the same plan turns into a warning: the fastener purchase had to start on November seventh. The shortage is explained before it happens.
Narrator: Step five: I release that fastener purchase, through the same confirm gate. It creates the purchase order document — and the trial balance does not move. Planning never posts. Finally, the audit trail: every entry says who posted it, which document it belongs to, why it posted — and for agent postings, the exact words I used. Provable books. An agent on a leash. A plan that explains itself. That is ERP Craft.
```

## Cue sheet

| Beat | Narration | On screen (transcript section) |
| --- | --- | --- |
| 1 · ~0:00 | Intro — provable books | Step 1: `health`, `trial_balance` ($120,500.00/side) |
| 2 · ~0:35 | Configured quote + refusal | Step 2: quote at $162.00/unit, forced reinforced mount; `evaluate_config` refusal (exit 1) |
| 3 · ~1:05 | Agent invoice behind the gate | Step 3: confirmation text, $7,720.00 = $7,720.00, `Confirm? [y/N]` |
| 4 · ~1:40 | Dynamic MRP + shortage | Step 4: agent's plan answer; `explain_shortage` (start was Nov 7) |
| 5 · ~2:05 | Gated release + audit trail | Step 5: totals identical before/after; Step 6: `audit_trail --source agent`, `audit_entry 2` chain |

## Production notes

- Record the terminal at a readable font size; the transcript file is
  the fallback for any beat that scrolls too fast — pause on the
  confirmation screen (beat 3) and the audit summary (beat 5).
- The refusal in beat 2 prints to stderr in a real terminal; when
  recording interactively it appears inline, in red in most themes.
  That is the honest behavior — do not stage it as a dialog box.
- End card suggestion: repo URL — github.com/BLTCPT/erpcraft —
  over the final trial balance.
