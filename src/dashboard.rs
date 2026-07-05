//! # dashboard — the local, read-only, self-contained web server
//!
//! **Why this file exists:** DASHBOARD-SPEC §6 wants `cce dashboard` to serve a
//! local web page that visualizes the metrics aggregate — the two north-stars
//! (savings + retrieval quality) with improving/degrading indicators, plus a
//! recent-searches table and a friendly empty state — over a small JSON API.
//!
//! **What it is / does:** A hand-rolled HTTP/1.1 server bound to `127.0.0.1`
//! (loopback only), serving `GET /` (an HTML page that inlines all CSS/JS and
//! draws its own SVG charts — no external network, CDN, or fonts), `GET
//! /api/metrics` (the §4 aggregate, computed fresh per request), and `GET
//! /api/health`. Every endpoint is READ-ONLY; nothing here mutates state.
//!
//! **Responsibilities:**
//! - Own request routing (`route`), the self-contained page, and the socket loop.
//! - Compute the API body from the live log on each request.
//! - It does NOT aggregate (delegates to `aggregator`) and never writes events.
//!   Binding is loopback-only by construction; a non-loopback bind would need a
//!   token (see SECURITY.md) and is intentionally not offered here.

use crate::aggregator::aggregate;
use crate::metrics::{format_iso, read_log};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A minimal HTTP response.
pub struct HttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

/// Current wall-clock instant as epoch seconds (the dashboard is a live view).
fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Build the `/api/metrics` JSON body: the §4 aggregate plus a non-conformance
/// `generated_ts` wall-clock stamp, computed fresh from the log at `metrics_path`.
pub fn metrics_body(metrics_path: &Path, price: f64) -> String {
    let now = now_secs();
    let log = read_log(metrics_path);
    let agg = aggregate(&log.events, now, price);
    let mut val = serde_json::to_value(&agg).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = val.as_object_mut() {
        obj.insert("generated_ts".to_string(), serde_json::Value::String(format_iso(now)));
    }
    serde_json::to_string_pretty(&val).unwrap_or_else(|_| "{}".to_string())
}

/// Build the `/api/health` JSON body.
fn health_body(metrics_path: &Path) -> String {
    let log = read_log(metrics_path);
    serde_json::json!({
        "status": "ok",
        "events": log.event_count(),
        "skipped": log.skipped,
    })
    .to_string()
}

/// Route a request path to a response (DASHBOARD-SPEC §6). Read-only.
pub fn route(path: &str, metrics_path: &Path, price: f64) -> HttpResponse {
    // Ignore any query string.
    let clean = path.split('?').next().unwrap_or(path);
    match clean {
        "/" => HttpResponse {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: PAGE_HTML.to_string(),
        },
        "/api/metrics" => HttpResponse {
            status: 200,
            content_type: "application/json",
            body: metrics_body(metrics_path, price),
        },
        "/api/health" => HttpResponse {
            status: 200,
            content_type: "application/json",
            body: health_body(metrics_path),
        },
        _ => HttpResponse {
            status: 404,
            content_type: "application/json",
            body: serde_json::json!({"error": "not found"}).to_string(),
        },
    }
}

/// Reason phrase for the small set of statuses we emit.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        _ => "OK",
    }
}

/// Handle one connection: read the request line, route it, write the response.
pub fn handle_connection(
    mut stream: TcpStream,
    metrics_path: &Path,
    price: f64,
) -> std::io::Result<()> {
    handle_connection_with(stream_router(metrics_path, price), &mut stream)
}

/// A router closure resolving a request path to a response.
fn stream_router<'a>(metrics_path: &'a Path, price: f64) -> impl Fn(&str) -> HttpResponse + 'a {
    move |path: &str| route(path, metrics_path, price)
}

/// Read the request line from `stream`, route it via `router`, write the response.
/// Shared by the single-repo and workspace (SPEC-V2.2 §7) dashboards.
pub fn handle_connection_with(
    router: impl Fn(&str) -> HttpResponse,
    stream: &mut TcpStream,
) -> std::io::Result<()> {
    let path = {
        let mut reader = BufReader::new(&mut *stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        // Request line: "METHOD SP PATH SP HTTP/x.y". We only serve GET/read-only.
        line.split_whitespace().nth(1).unwrap_or("/").to_string()
    };
    let resp = router(&path);
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp.status,
        reason(resp.status),
        resp.content_type,
        resp.body.len(),
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(resp.body.as_bytes())?;
    stream.flush()
}

/// Serve connections from `listener`. When `max` is `Some(n)`, stop after `n`
/// connections (used by tests for a clean shutdown); `None` serves forever.
pub fn serve(listener: TcpListener, metrics_path: PathBuf, price: f64, max: Option<usize>) {
    let mut served = 0usize;
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let _ = handle_connection(s, &metrics_path, price);
            }
            Err(_) => break,
        }
        served += 1;
        if let Some(m) = max {
            if served >= m {
                break;
            }
        }
    }
}

/// Bind `127.0.0.1:port` and serve the dashboard forever. Prints the URL.
pub fn run(metrics_path: PathBuf, price: f64, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let addr = listener.local_addr()?;
    println!("cce dashboard: serving http://{addr}/  (loopback only, read-only)");
    println!("metrics log : {}", metrics_path.display());
    println!("press Ctrl-C to stop.");
    serve(listener, metrics_path, price, None);
    Ok(())
}

// --- Workspace dashboard (SPEC-V2.2 §7) ---

/// Build the workspace `/api/metrics` JSON body: the §4 roll-up over every
/// member's events plus a `by_package` breakdown (federation), computed fresh
/// from the members' logs at request time, with a wall-clock `generated_ts`.
pub fn workspace_metrics_body(members: &[crate::federation::MemberMetrics], price: f64) -> String {
    let now = now_secs();
    let mut val = crate::federation::federated_metrics_json(members, now, price);
    if let Some(obj) = val.as_object_mut() {
        obj.insert("generated_ts".to_string(), serde_json::Value::String(format_iso(now)));
    }
    serde_json::to_string_pretty(&val).unwrap_or_else(|_| "{}".to_string())
}

/// Route a request path for the workspace dashboard. Read-only.
pub fn route_workspace(
    path: &str,
    members: &[crate::federation::MemberMetrics],
    price: f64,
) -> HttpResponse {
    let clean = path.split('?').next().unwrap_or(path);
    match clean {
        "/" => HttpResponse {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: PAGE_HTML.to_string(),
        },
        "/api/metrics" => HttpResponse {
            status: 200,
            content_type: "application/json",
            body: workspace_metrics_body(members, price),
        },
        "/api/health" => {
            let events: usize =
                members.iter().map(|m| read_log(&m.metrics_path).event_count()).sum();
            HttpResponse {
                status: 200,
                content_type: "application/json",
                body: serde_json::json!({
                    "status": "ok",
                    "events": events,
                    "members": members.len(),
                })
                .to_string(),
            }
        }
        _ => HttpResponse {
            status: 404,
            content_type: "application/json",
            body: serde_json::json!({"error": "not found"}).to_string(),
        },
    }
}

/// Bind `127.0.0.1:port` and serve the federated workspace dashboard forever.
pub fn run_workspace(
    members: Vec<crate::federation::MemberMetrics>,
    price: f64,
    port: u16,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let addr = listener.local_addr()?;
    println!("cce workspace dashboard: serving http://{addr}/  (loopback only, read-only)");
    println!("members : {}", members.len());
    println!("press Ctrl-C to stop.");
    for stream in listener.incoming().flatten() {
        let mut stream = stream;
        let _ = handle_connection_with(|p| route_workspace(p, &members, price), &mut stream);
    }
    Ok(())
}

/// The entire page: HTML + inlined CSS + inlined JS that fetches `/api/metrics`
/// and draws hand-rolled SVG charts. No external network, CDN, or fonts.
const PAGE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>CCE Dashboard</title>
<style>
  :root { color-scheme: light dark; --bg:#f6f7f9; --card:#fff; --ink:#1a1d21; --muted:#5b6470;
          --line:#e2e6ea; --up:#1a8a4a; --down:#c23b3b; --flat:#8a7a1a; --accent:#3556d4; }
  @media (prefers-color-scheme: dark) {
    :root { --bg:#14171c; --card:#1d2128; --ink:#e8ebef; --muted:#9aa4b1; --line:#2b313a;
            --up:#41c07a; --down:#e06666; --flat:#d4bd55; --accent:#6d86f0; }
  }
  * { box-sizing:border-box; }
  body { margin:0; background:var(--bg); color:var(--ink);
         font:15px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif; }
  header { padding:22px 24px 8px; }
  h1 { margin:0; font-size:20px; }
  .sub { color:var(--muted); font-size:13px; margin-top:2px; }
  main { padding:12px 24px 40px; max-width:1080px; }
  .cards { display:grid; grid-template-columns:repeat(auto-fit,minmax(160px,1fr)); gap:12px; margin:12px 0 20px; }
  .card { background:var(--card); border:1px solid var(--line); border-radius:10px; padding:14px 16px; }
  .card .k { color:var(--muted); font-size:12px; text-transform:uppercase; letter-spacing:.04em; }
  .card .v { font-size:26px; font-weight:650; margin-top:4px; }
  section { background:var(--card); border:1px solid var(--line); border-radius:12px; padding:18px 20px; margin:16px 0; }
  section h2 { margin:0 0 4px; font-size:15px; }
  section .hint { color:var(--muted); font-size:12px; margin-bottom:12px; }
  .delta { display:flex; align-items:baseline; gap:12px; margin:6px 0 14px; flex-wrap:wrap; }
  .delta .big { font-size:30px; font-weight:700; }
  .delta .tag { font-weight:650; font-size:14px; padding:2px 10px; border-radius:999px; border:1px solid var(--line); }
  .up { color:var(--up); } .down { color:var(--down); } .flat { color:var(--flat); }
  .cmp { color:var(--muted); font-size:13px; }
  .charts { display:grid; grid-template-columns:repeat(auto-fit,minmax(280px,1fr)); gap:16px; }
  .chart h3 { font-size:12px; color:var(--muted); margin:0 0 6px; font-weight:600; text-transform:uppercase; letter-spacing:.03em; }
  svg { width:100%; height:auto; display:block; }
  table { width:100%; border-collapse:collapse; font-size:13px; }
  th,td { text-align:left; padding:8px 10px; border-bottom:1px solid var(--line); }
  th { color:var(--muted); font-weight:600; font-size:11px; text-transform:uppercase; letter-spacing:.03em; }
  td.num { text-align:right; font-variant-numeric:tabular-nums; }
  .pill { font-size:11px; padding:1px 8px; border-radius:999px; border:1px solid var(--line); }
  .pill.helpful { color:var(--up); } .pill.not_helpful { color:var(--down); } .pill.none { color:var(--muted); }
  .empty { text-align:center; padding:56px 20px; color:var(--muted); }
  .empty code { background:var(--card); border:1px solid var(--line); border-radius:6px; padding:2px 6px; }
  .axis { fill:var(--muted); font-size:10px; }
  footer { color:var(--muted); font-size:12px; padding:0 24px 28px; }
</style>
</head>
<body>
<header>
  <h1>CCE Dashboard</h1>
  <div class="sub" id="sub">loading metrics…</div>
</header>
<main id="main"></main>
<footer id="footer"></footer>
<script>
const $ = (t, a={}, kids=[]) => { const e=document.createElementNS(t.startsWith('svg:')?'http://www.w3.org/2000/svg':'http://www.w3.org/1999/xhtml', t.replace('svg:','')); for(const k in a){ if(k==='text') e.textContent=a[k]; else e.setAttribute(k,a[k]); } (Array.isArray(kids)?kids:[kids]).forEach(c=>c&&e.appendChild(typeof c==='string'?document.createTextNode(c):c)); return e; };
const int = n => (n||0).toLocaleString('en-US');
const pct = r => (r==null? '—' : (r*100).toFixed(1)+'%');
const usd = n => '$'+(n||0).toFixed(2);
const sc  = n => (n==null? '—' : (+n).toFixed(3));
function arrow(dir){ if(dir==='up') return {sym:'↑', word:'improving', cls:'up'}; if(dir==='down') return {sym:'↓', word:'degrading', cls:'down'}; return {sym:'→', word:'flat', cls:'flat'}; }

function bars(points, key, opts={}){
  const W=300,H=120,pad=22, n=points.length;
  const max = Math.max(1e-9, ...points.map(p=>+p[key]||0));
  const bw = n? (W-pad*2)/n : 0;
  const svg = $('svg:svg',{viewBox:`0 0 ${W} ${H}`,role:'img'});
  svg.appendChild($('svg:line',{x1:pad,y1:H-pad,x2:W-pad,y2:H-pad,stroke:'var(--line)'}));
  points.forEach((p,i)=>{
    const v=+p[key]||0, h=(H-pad*2)*(v/max);
    svg.appendChild($('svg:rect',{x:(pad+i*bw+bw*0.15).toFixed(1), y:(H-pad-h).toFixed(1), width:(bw*0.7).toFixed(1), height:h.toFixed(1), rx:2, fill:opts.color||'var(--accent)'}));
  });
  if(n){ svg.appendChild($('svg:text',{x:pad,y:H-6,class:'axis',text:points[0].date.slice(5)}));
         svg.appendChild($('svg:text',{x:W-pad,y:H-6,'text-anchor':'end',class:'axis',text:points[n-1].date.slice(5)})); }
  svg.appendChild($('svg:text',{x:pad,y:12,class:'axis',text:(opts.fmt?opts.fmt(max):max)}));
  return svg;
}
function line(points, key, opts={}){
  const W=300,H=120,pad=22, n=points.length;
  const max = Math.max(1e-9, ...points.map(p=>+p[key]||0));
  const svg = $('svg:svg',{viewBox:`0 0 ${W} ${H}`,role:'img'});
  svg.appendChild($('svg:line',{x1:pad,y1:H-pad,x2:W-pad,y2:H-pad,stroke:'var(--line)'}));
  const x = i => n<=1? pad : pad+(W-pad*2)*(i/(n-1));
  const y = v => H-pad-(H-pad*2)*(v/max);
  let d=''; points.forEach((p,i)=>{ d+=(i?'L':'M')+x(i).toFixed(1)+' '+y(+p[key]||0).toFixed(1)+' '; });
  if(n) svg.appendChild($('svg:path',{d, fill:'none', stroke:opts.color||'var(--accent)','stroke-width':2}));
  points.forEach((p,i)=> svg.appendChild($('svg:circle',{cx:x(i).toFixed(1),cy:y(+p[key]||0).toFixed(1),r:2.5,fill:opts.color||'var(--accent)'})));
  svg.appendChild($('svg:text',{x:pad,y:12,class:'axis',text:(opts.fmt?opts.fmt(max):max)}));
  if(n){ svg.appendChild($('svg:text',{x:pad,y:H-6,class:'axis',text:points[0].date.slice(5)}));
         svg.appendChild($('svg:text',{x:W-pad,y:H-6,'text-anchor':'end',class:'axis',text:points[n-1].date.slice(5)})); }
  return svg;
}
function chart(title, node){ return $('div',{class:'chart'},[$('h3',{text:title}), node]); }

function deltaBlock(valueText, cmpText, dir){
  const a=arrow(dir);
  return $('div',{class:'delta'},[
    $('span',{class:'big',text:valueText}),
    $('span',{class:'tag '+a.cls,text:a.sym+' '+a.word}),
    $('span',{class:'cmp',text:cmpText})
  ]);
}

function render(m){
  const sub=document.getElementById('sub'), main=document.getElementById('main'), foot=document.getElementById('footer');
  const t=m.totals;
  const noData = (t.searches+t.indexes+t.feedback)===0;
  sub.textContent = noData ? 'no data yet' : ('generated '+(m.generated_ts||''));
  main.innerHTML=''; foot.textContent='schema '+m.schema+' · read-only · loopback only';
  if(noData){
    main.appendChild($('div',{class:'empty'},[
      $('p',{text:'No metrics yet.'}),
      $('p',{},[document.createTextNode('Run '), $('code',{text:'cce index <dir>'}), document.createTextNode(' and '), $('code',{text:'cce search <query>'}), document.createTextNode(' to start collecting data, then refresh.')])
    ]));
    return;
  }
  const cards=$('div',{class:'cards'});
  [['Tokens saved',int(t.tokens_saved)],['Est. $ saved',usd(t.cost_saved_usd)],['Searches',int(t.searches)],['Helpful rate',pct(t.helpful_rate)]]
    .forEach(([k,v])=> cards.appendChild($('div',{class:'card'},[$('div',{class:'k',text:k}),$('div',{class:'v',text:v})])));
  main.appendChild(cards);

  const daily=m.series.daily||[];
  // North-star A: savings
  const sv=m.north_star.savings;
  const secA=$('section',{},[
    $('h2',{text:'North-star A · Token & cost savings'}),
    $('div',{class:'hint',text:'mean savings ratio, current 7 days vs prior 7 days'}),
    deltaBlock(pct(sv.current.mean_savings_ratio), 'current '+pct(sv.current.mean_savings_ratio)+' vs prior '+pct(sv.prior.mean_savings_ratio)+'  (Δ '+(sv.delta_ratio>=0?'+':'')+sv.delta_ratio.toFixed(6)+')', sv.direction),
    $('div',{class:'charts'},[
      chart('Tokens saved / day', bars(daily,'tokens_saved',{fmt:int})),
      chart('Mean savings ratio / day', line(daily,'mean_savings_ratio',{fmt:v=>v.toFixed(2)}))
    ])
  ]);
  main.appendChild(secA);

  // North-star B: quality
  const q=m.north_star.quality;
  const secB=$('section',{},[
    $('h2',{text:'North-star B · Retrieval quality'}),
    $('div',{class:'hint',text:'mean top score, current 7 days vs prior 7 days'}),
    deltaBlock(sc(q.current.mean_top_score), 'current '+sc(q.current.mean_top_score)+' vs prior '+sc(q.prior.mean_top_score)+'  (Δ '+(q.delta_top_score>=0?'+':'')+q.delta_top_score.toFixed(6)+')', q.direction),
    $('div',{class:'charts'},[
      chart('Mean top score / day', line(daily,'mean_top_score',{color:'var(--up)',fmt:v=>v.toFixed(2)})),
      chart('Empty rate / day', line(daily,'empty_rate',{color:'var(--down)',fmt:v=>v.toFixed(2)})),
      chart('Helpful / day', bars(daily,'helpful',{color:'var(--up)',fmt:int})),
      chart('Not helpful / day', bars(daily,'not_helpful',{color:'var(--down)',fmt:int}))
    ])
  ]);
  main.appendChild(secB);

  // Recent searches
  const rows=(m.recent_searches||[]).map(r=>$('tr',{},[
    $('td',{},[$('span',{title:r.ts,text:(r.ts||'').replace('T',' ').replace('Z','')})]),
    $('td',{text:r.query||''}),
    $('td',{class:'num',text:int(r.result_count)}),
    $('td',{class:'num',text:int(r.tokens_saved)}),
    $('td',{class:'num',text:pct(r.savings_ratio)}),
    $('td',{class:'num',text:sc(r.top_score)}),
    $('td',{},[$('span',{class:'pill '+r.feedback,text:r.feedback})])
  ]));
  const tbl=$('table',{},[
    $('thead',{},[$('tr',{},['Time','Query','Results','Saved','Ratio','Top','Feedback'].map(h=>$('th',{text:h})))]),
    $('tbody',{},rows.length?rows:[$('tr',{},[$('td',{colspan:7,text:'no searches yet'})])])
  ]);
  main.appendChild($('section',{},[$('h2',{text:'Recent searches'}),$('div',{class:'hint',text:'newest first'}),tbl]));
}

fetch('/api/metrics').then(r=>r.json()).then(render).catch(e=>{
  document.getElementById('sub').textContent='failed to load metrics: '+e;
});
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_metrics() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test/fixture/base/metrics_sample.jsonl"
        ))
    }

    #[test]
    fn root_serves_self_contained_html() {
        let resp = route("/", &fixture_metrics(), 3.00);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "text/html; charset=utf-8");
        assert!(resp.body.contains("<title>CCE Dashboard</title>"));
        // Inlines its own CSS + JS and draws its own SVG.
        assert!(resp.body.contains("<style>"));
        assert!(resp.body.contains("<script>"));
        assert!(resp.body.contains("svg:svg"));
        // Self-contained: no external resource loads (no CDN, no remote assets).
        // (The only "http" strings are XML namespace identifiers, never fetched.)
        assert!(!resp.body.contains("<link"));
        assert!(!resp.body.contains("src="));
        assert!(!resp.body.to_lowercase().contains("cdn"));
        assert!(!resp.body.contains("@import"));
    }

    #[test]
    fn health_reports_event_and_skipped_counts() {
        let resp = route("/api/health", &fixture_metrics(), 3.00);
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["events"], 7); // 1 index + 4 search + 2 feedback
        assert_eq!(v["skipped"], 0);
    }

    #[test]
    fn workspace_routes_serve_rollup_health_and_page() {
        // Two members, one search event each; the workspace routes must serve the
        // shared page, a federated `/api/metrics` with `by_package`, a summed
        // `/api/health`, and 404 for anything else (SPEC-V2.2 §7).
        let tmp = tempfile::tempdir().unwrap();
        let mut members = Vec::new();
        for (name, tokens) in [("app", 1000u64), ("billing", 3000u64)] {
            let p = tmp.path().join(format!("{name}.jsonl"));
            std::fs::write(
                &p,
                format!(
                    "{{\"schema\":\"cce.metrics/v1\",\"event\":\"search\",\"ts\":\"2026-07-04T10:00:00Z\",\"id\":\"{name}00000000\",\"query\":\"q\",\"result_count\":1,\"tokens_saved\":{tokens},\"savings_ratio\":0.5,\"top_score\":0.9,\"empty\":false,\"low_confidence\":false}}\n"
                ),
            )
            .unwrap();
            members.push(crate::federation::MemberMetrics {
                name: name.to_string(),
                package: name.to_string(),
                metrics_path: p,
            });
        }

        let page = route_workspace("/", &members, 3.00);
        assert!(page.body.contains("<title>CCE Dashboard</title>"));

        let health = route_workspace("/api/health", &members, 3.00);
        let hv: serde_json::Value = serde_json::from_str(&health.body).unwrap();
        assert_eq!(hv["events"], 2);
        assert_eq!(hv["members"], 2);

        let metrics = route_workspace("/api/metrics", &members, 3.00);
        let mv: serde_json::Value = serde_json::from_str(&metrics.body).unwrap();
        assert_eq!(mv["totals"]["tokens_saved"], 4000);
        assert!(mv.get("generated_ts").is_some());
        assert_eq!(mv["by_package"].as_array().unwrap().len(), 2);

        assert_eq!(route_workspace("/nope", &members, 3.00).status, 404);
    }

    #[test]
    fn metrics_endpoint_is_aggregate_plus_generated_ts() {
        let resp = route("/api/metrics", &fixture_metrics(), 3.00);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "application/json");
        let v: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(v["schema"], "cce.metrics/v1");
        assert!(v.get("generated_ts").is_some());
        assert_eq!(v["totals"]["searches"], 4);
        assert_eq!(v["totals"]["tokens_saved"], 53000);
    }

    #[test]
    fn unknown_path_is_404() {
        let resp = route("/nope", &fixture_metrics(), 3.00);
        assert_eq!(resp.status, 404);
        let v: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert!(v.get("error").is_some());
    }

    #[test]
    fn query_string_is_ignored() {
        let resp = route("/api/health?x=1", &fixture_metrics(), 3.00);
        assert_eq!(resp.status, 200);
    }

    #[test]
    fn missing_log_yields_empty_but_valid_metrics() {
        let resp = route("/api/metrics", Path::new("/no/such/metrics.jsonl"), 3.00);
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(v["totals"]["searches"], 0);
    }
}
