//! Render a scenario-suite run as a self-contained HTML report — the human-facing
//! artifact an auditor attaches to a finding ("here's what happens under each
//! edge case"). No external assets, opens offline.

use crate::replay::ScenarioOutcome;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the outcomes of a suite run to a standalone HTML document.
pub fn render_html(title: &str, outcomes: &[ScenarioOutcome]) -> String {
    let passed = outcomes.iter().filter(|o| o.pass).count();
    let total = outcomes.len();
    let all_pass = passed == total;

    let cards: String = outcomes
        .iter()
        .map(|o| {
            let (mark, cls) = if o.pass { ("PASS", "pass") } else { ("FAIL", "fail") };
            let got = if o.actual.success {
                "succeeded".to_string()
            } else {
                format!("reverted — {}", esc(o.actual.error.as_deref().unwrap_or("error")))
            };
            let asserts: String = o
                .asserts
                .iter()
                .map(|a| {
                    let m = if a.pass { "✓" } else { "✗" };
                    let ac = if a.pass { "pass" } else { "fail" };
                    format!(
                        "<li class=\"{ac}\">{m} {}</li>",
                        esc(&a.description)
                    )
                })
                .collect();
            let asserts = if asserts.is_empty() {
                String::new()
            } else {
                format!("<ul class=\"asserts\">{asserts}</ul>")
            };
            let logs = if o.actual.logs.is_empty() {
                String::new()
            } else {
                let body = o
                    .actual
                    .logs
                    .iter()
                    .map(|l| esc(l))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("<details><summary>program logs ({} lines)</summary><pre>{body}</pre></details>", o.actual.logs.len())
            };
            format!(
                r#"<div class="card {cls}">
  <div class="head">
    <span class="badge {cls}">{mark}</span>
    <span class="name">{name}</span>
    <span class="cu">{cu} CU</span>
  </div>
  <div class="meta">expected: <b>{expect}</b> &nbsp;·&nbsp; got: {got}</div>
  {asserts}
  {logs}
</div>"#,
                name = esc(&o.name),
                expect = esc(&o.expect),
                cu = o.actual.compute_units,
            )
        })
        .collect();

    let summary_cls = if all_pass { "pass" } else { "fail" };
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>svmscope report — {title}</title>
<style>
  :root {{ --bg:#0a0b0f; --panel:#13151d; --line:#1e2632; --text:#e8edf4; --dim:#9aa3b4;
    --muted:#5f6878; --green:#35d07f; --red:#ff5c6c; --mono:ui-monospace,Menlo,monospace; }}
  * {{ box-sizing:border-box; }}
  body {{ margin:0; background:var(--bg); color:var(--text); font-family:-apple-system,Segoe UI,sans-serif;
    line-height:1.5; padding:32px 20px 60px; }}
  main {{ max-width:820px; margin:0 auto; }}
  h1 {{ font-size:20px; margin:0 0 4px; }}
  .sub {{ color:var(--muted); font-family:var(--mono); font-size:12px; word-break:break-all; margin-bottom:18px; }}
  .summary {{ display:inline-flex; align-items:center; gap:10px; font-size:22px; font-weight:750;
    padding:10px 18px; border-radius:12px; margin-bottom:22px; }}
  .summary.pass {{ background:rgba(53,208,127,.12); color:var(--green); }}
  .summary.fail {{ background:rgba(255,92,108,.12); color:var(--red); }}
  .card {{ background:var(--panel); border:1px solid var(--line); border-left:3px solid var(--muted);
    border-radius:12px; padding:14px 16px; margin:10px 0; }}
  .card.pass {{ border-left-color:var(--green); }}
  .card.fail {{ border-left-color:var(--red); }}
  .head {{ display:flex; align-items:center; gap:12px; }}
  .badge {{ font-size:11px; font-weight:700; padding:3px 9px; border-radius:6px; }}
  .badge.pass {{ background:rgba(53,208,127,.15); color:var(--green); }}
  .badge.fail {{ background:rgba(255,92,108,.15); color:var(--red); }}
  .name {{ font-weight:650; flex:1; }}
  .cu {{ color:var(--muted); font-family:var(--mono); font-size:12px; }}
  .meta {{ color:var(--dim); font-size:13px; margin-top:8px; }}
  .asserts {{ list-style:none; padding:0; margin:10px 0 0; font-family:var(--mono); font-size:12.5px; }}
  .asserts li {{ padding:2px 0; }}
  .asserts li.pass {{ color:var(--green); }}
  .asserts li.fail {{ color:var(--red); }}
  details {{ margin-top:10px; }}
  summary {{ cursor:pointer; color:var(--dim); font-size:12.5px; }}
  pre {{ background:#0c0e14; border:1px solid var(--line); border-radius:8px; padding:10px 12px;
    overflow:auto; max-height:320px; font-family:var(--mono); font-size:11.5px; color:var(--dim); }}
  footer {{ color:var(--muted); font-size:12px; margin-top:26px; }}
</style></head>
<body><main>
  <h1>svmscope scenario report</h1>
  <div class="sub">{title}</div>
  <div class="summary {summary_cls}">{passed} / {total} passed</div>
  {cards}
  <footer>Generated by svmscope · deterministic replay of real Solana programs.</footer>
</main></body></html>"#,
        title = esc(title),
    )
}
