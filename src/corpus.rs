//! # corpus — the read-only corpus-serve bridge (ADR-CORPUS-SERVE; signal-engine Epic #8 · U1.3)
//!
//! **Why this file exists:** signal-engine (the consumer) enriches triage by calling a
//! corpus over plain HTTP — `GET /docs?service=<name>` — but cce (the producer) speaks
//! MCP over stdio and exposes no `/docs` route anywhere (G1, the one seam that carries
//! the product's entire value). Wired together as shipped, nothing errors: the consumer
//! fails open, stamps `corpus_degraded: true`, and triages with zero business context
//! forever. This module closes that seam **at the producer** exactly as
//! [`ADR-CORPUS-SERVE.md`](../../ADR-CORPUS-SERVE.md) decided: a native `cce corpus
//! serve` subcommand — one binary, no middleman — that answers `GET /docs?service=`
//! from the same in-process knowledge retrieval the MCP server uses, with no
//! side-channel.
//!
//! **What it is / does:** A small, read-only, **authenticated** HTTP/1.1 GET surface
//! bound to `127.0.0.1` — the same loopback shape as [`crate::dashboard`], reusing its
//! [`HttpResponse`](crate::dashboard::HttpResponse). Every request must carry
//! `Authorization: Bearer <token>` (constant-time compared) or it is answered `401`; the
//! token is a per-instance secret (R24), so the route is authenticated by construction
//! and refuses to start without one (see `main::cmd_corpus_serve`). `GET /docs?service=`
//! runs [`search_knowledge`](crate::knowledge::search_knowledge) over the store loaded
//! once from `<dir>/.cce/knowledge/` and returns `{"docs":[{"id","title","body"}, …]}` —
//! the exact JSON shape signal-engine's `corpus_client` parses. Retrieval is an
//! in-process call over content already redacted at index time (SPEC-V2.1), so the route
//! adds no new secret-exposure surface.
//!
//! **Responsibilities:**
//! - Own the corpus-serve request parse (request line + `Authorization` header only),
//!   the auth gate, the `/docs` route, the `{"docs":[…]}` body, and the socket loop.
//! - It does NOT rank (that is [`crate::knowledge::retrieval`]) nor decide the token
//!   (that is per-instance config resolved by the CLI).
//! - Loopback-only by construction; TLS and a non-loopback bind are a later ticket
//!   (signal-engine #12/#14) — on the co-located one box (OD2) the hop carries no
//!   network, so bearer auth over loopback is the #11 posture.

use crate::config::KnowledgeConfig;
use crate::dashboard::HttpResponse;
use crate::knowledge::{ingest_default, KnowledgeStore, LoadedKnowledge};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a connection may take to deliver its request head before the server gives up
/// on it and moves on. Mirrors the dashboard's bound (#128): a half-open/idle socket
/// sends no bytes, and connections are served serially, so without this every later
/// request would hang behind it. Loopback clients send immediately, so this is only the
/// idle-stall cap, never a real-request deadline.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap on the request-head bytes read (request line + headers, before the blank
/// line). A legitimate `GET /docs?service=<name>` plus a bearer header is well under 1 KiB;
/// this bounds the allocation a misbehaving/hostile local client can force, so no single
/// connection can make the server read (and buffer) an unbounded head.
const MAX_HEAD_BYTES: u64 = 8 * 1024;

/// A parsed corpus-serve request: only the three fields the route needs. Everything else
/// in the head (Host, Connection, …) is read past and ignored.
struct Request {
    method: String,
    target: String,
    authorization: Option<String>,
}

/// Constant-time byte equality — no early return on the first mismatched byte, so a
/// timing side-channel cannot probe the token one byte at a time. Length is allowed to
/// leak (a token's length is not the secret); content comparison is constant-time.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// True iff the `Authorization` header presents `Bearer <token>` matching `expected`.
/// The scheme keyword is matched case-insensitively (RFC 7235); the token itself is
/// compared byte-exact and constant-time.
fn authorized(authorization: Option<&str>, expected: &str) -> bool {
    let Some(header) = authorization else { return false };
    let header = header.trim();
    let Some((scheme, presented)) = header.split_once(' ') else { return false };
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return false;
    }
    ct_eq(presented.trim().as_bytes(), expected.as_bytes())
}

/// Decode one `application/x-www-form-urlencoded` component: `+` → space and `%XX` →
/// byte. This is the exact inverse of the consumer's `URI.encode_www_form_component`
/// (space→`+`, `/`→`%2F`), so `service=check+out%2F2` round-trips to `check out/2`.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Pull the decoded `service` value out of a request target (`/docs?service=<v>&…`).
/// Any other query keys (a future `q=`, U2.1) are read past and ignored, so the contract
/// extends without a break. Returns `None` when `service` is absent or empty.
fn service_param(target: &str) -> Option<String> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == "service" {
                let decoded = percent_decode(v);
                if !decoded.trim().is_empty() {
                    return Some(decoded);
                }
            }
        }
    }
    None
}

/// Char cap for a served doc `body` (U2.2 · G19). The consumer fences every untrusted
/// string with [`Fence::MAX_CHARS`] = 500 by a blunt `text[0, 500]` slice that lands
/// mid-word / mid-sentence (signal-engine `triage/fence.rb`). Fitting the body to the same
/// budget *server-side* — but at a sentence or word boundary — means a real ADR arrives as
/// a readable, incident-ready snippet the consumer passes through untouched, not a
/// truncated stub. Measured in `char`s (Unicode scalars), matching the consumer's
/// character-indexed slice, so a multi-byte scalar is never split.
const BODY_CHAR_CAP: usize = 500;

/// Fit `content` to at most [`BODY_CHAR_CAP`] characters at a sentence or word boundary.
///
/// Returns the content verbatim when it already fits. When it must cut, it prefers to end
/// on the last complete sentence within the cap (a clean snippet needs no marker); failing
/// that — one long unbroken clause — it cuts at the last word boundary and appends a single
/// `…` (U+2026) to mark the elision. The cut is always on a `char` boundary, so a
/// multi-byte scalar is never torn. This is the only place the served body is shaped; the
/// ranking is [`crate::knowledge::retrieval`]'s and is left untouched.
fn body_snippet(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= BODY_CHAR_CAP {
        return content.to_string();
    }
    // Prefer the last complete sentence that fits: end on `.`/`!`/`?` followed by
    // whitespace (so "3.14" or "e.g" mid-number/word is not mistaken for an end). A run of
    // whole sentences reads cleanly and needs no elision marker.
    let within = &chars[..BODY_CHAR_CAP];
    let sentence_end = within.iter().enumerate().rev().find_map(|(i, &c)| {
        let terminator = matches!(c, '.' | '!' | '?');
        let boundary = within.get(i + 1).is_none_or(|n| n.is_whitespace());
        (terminator && boundary).then_some(i + 1)
    });
    if let Some(end) = sentence_end {
        return within[..end].iter().collect::<String>().trim_end().to_string();
    }
    // No sentence terminator in range: cut at the last word boundary and mark the cut.
    // Reserve one char for the `…` so the marked result still fits the cap.
    let budget = BODY_CHAR_CAP - 1;
    let window = &chars[..budget];
    let cut = window.iter().rposition(|c| c.is_whitespace()).unwrap_or(budget);
    let kept: String = window[..cut].iter().collect::<String>().trim_end().to_string();
    format!("{kept}…")
}

/// The `{"docs":[{"id","title","body"}, …]}` body signal-engine's `corpus_client`
/// parses: `id` is the section's stable `chunk_id`, `title` the heading, `body` the
/// section content (already redacted at index time), fitted server-side to the consumer's
/// [`BODY_CHAR_CAP`] at a readable boundary by [`body_snippet`] (U2.2 · G19).
fn docs_body(hits: &[crate::knowledge::KnowledgeHit]) -> String {
    let docs: Vec<serde_json::Value> = hits
        .iter()
        .map(|h| {
            serde_json::json!({
                "id": h.chunk_id,
                "title": h.title,
                "body": body_snippet(&h.content),
            })
        })
        .collect();
    serde_json::json!({ "docs": docs }).to_string()
}

/// Route a parsed request to a response. Auth is checked FIRST, for every path, so an
/// unauthenticated client cannot even probe which routes exist (401 before 404/405).
fn route(
    req: &Request,
    token: &str,
    knowledge: &LoadedKnowledge,
    top_k: usize,
    min_score: f64,
) -> HttpResponse {
    if !authorized(req.authorization.as_deref(), token) {
        return json_response(401, r#"{"error":"unauthorized"}"#);
    }
    if req.method != "GET" {
        return json_response(405, r#"{"error":"method not allowed"}"#);
    }
    let path = req.target.split('?').next().unwrap_or(&req.target);
    if path != "/docs" {
        return json_response(404, r#"{"error":"not found"}"#);
    }
    let Some(service) = service_param(&req.target) else {
        return json_response(400, r#"{"error":"missing service"}"#);
    };
    // The query is the service name itself: knowledge records for a service carry its
    // facet (U1.2), so retrieval over the service term surfaces that service's docs. The
    // optional incident-terms `q=` is a later contract bump (U2.1/#14).
    let hits = knowledge.search(&service, top_k, min_score);
    json_response(200, &docs_body(&hits))
}

fn json_response(status: u16, body: &str) -> HttpResponse {
    HttpResponse { status, content_type: "application/json", body: body.to_string() }
}

/// Reason phrase for the small set of statuses this surface emits.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    }
}

/// Read the request head from `stream`: the request line (method + target) and the
/// `Authorization` header. Stops at the blank line that ends the head; the body (there is
/// none for GET) is not read. Returns `None` on an empty/closed connection.
fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    // Bound the wait for the request head so an idle/half-open socket cannot hang this
    // serially-served server forever (#128). Best-effort: proceed if the platform rejects it.
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    // Cap the total head bytes so no connection can force an unbounded read/allocation
    // (Take yields EOF once the cap is hit, ending the header loop cleanly).
    let mut reader = BufReader::new(&mut *stream).take(MAX_HEAD_BYTES);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut authorization = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_string());
            }
        }
    }
    Ok(Some(Request { method, target, authorization }))
}

/// Write an `HttpResponse` back over the socket. A `401` also carries a
/// `WWW-Authenticate: Bearer` challenge (RFC 7235). `Connection: close` — one request per
/// connection, matching the consumer's `Net::HTTP` one-shot GET and the dashboard.
fn write_response(stream: &mut TcpStream, resp: &HttpResponse) -> std::io::Result<()> {
    let challenge = if resp.status == 401 {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
        resp.status,
        reason(resp.status),
        resp.content_type,
        resp.body.len(),
        challenge,
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(resp.body.as_bytes())?;
    stream.flush()
}

/// Handle one connection: read the request head, route it (auth-gated), write the reply.
fn handle_connection(
    stream: &mut TcpStream,
    token: &str,
    knowledge: &LoadedKnowledge,
    top_k: usize,
    min_score: f64,
) -> std::io::Result<()> {
    let Some(req) = read_request(stream)? else { return Ok(()) };
    let resp = route(&req, token, knowledge, top_k, min_score);
    write_response(stream, &resp)
}

/// Serve connections from `listener` (loopback). When `max` is `Some(n)`, stop after `n`
/// connections (used by tests for a clean shutdown); `None` serves forever. The store is
/// loaded once (freshness is the deploy's job — the nightly `.cck` pull + restart, OD2).
pub fn serve(
    listener: TcpListener,
    knowledge: LoadedKnowledge,
    token: String,
    top_k: usize,
    min_score: f64,
    max: Option<usize>,
) {
    let mut served = 0usize;
    for stream in listener.incoming() {
        match stream {
            Ok(mut s) => {
                let _ = handle_connection(&mut s, &token, &knowledge, top_k, min_score);
            }
            // A transient accept error (a client RST between SYN and accept, momentary fd
            // pressure on the shared one box) must not tear the whole bridge down —
            // that would silently revert the consumer to zero-context triage, the exact
            // failure the ADR exists to prevent. Skip this connection, keep serving.
            Err(_) => continue,
        }
        served += 1;
        if let Some(m) = max {
            if served >= m {
                break;
            }
        }
    }
}

/// Load the active knowledge store under `root` for serving. A store that was never
/// created — no `current` pointer — is NOT an error: it is the "no evidence" state
/// before the first nightly pull, so the bridge serves `{"docs":[]}` (the consumer
/// stamps `corpus_degraded: false`, no context). Any OTHER failure is a present-but-
/// broken store and fails loud: a dangling `current` pointer whose snapshot is missing
/// (a half-written deploy) is NOT silently degraded to empty — that would re-introduce
/// the exact silent zero-context failure the ADR exists to kill — nor is a corrupt
/// (`InvalidData`) snapshot. Only the genuine no-pointer case is the empty path.
fn load_store(root: &Path) -> std::io::Result<KnowledgeStore> {
    // The empty path is keyed on the `current` pointer's ABSENCE specifically, not on any
    // NotFound (a dangling pointer's missing snapshot is also NotFound, but it is a broken
    // store, not an un-provisioned one).
    if !KnowledgeStore::current_pointer_path(root).exists() {
        return Ok(ingest_default(&[], b""));
    }
    KnowledgeStore::load_current(root)
}

/// Bind `127.0.0.1:port` and serve the corpus bridge forever. Prints the URL and store
/// path (never the token). `port` 0 binds an ephemeral port (printed). `top_k` bounds the
/// docs returned; `min_score` and any per-root knowledge config come from `<root>/.cce/config`.
pub fn run(root: PathBuf, port: u16, token: String, top_k: usize) -> std::io::Result<()> {
    let min_score = KnowledgeConfig::load(&root).min_score;
    let store = load_store(&root)?;
    let chunks = store.chunks.len();
    let knowledge = LoadedKnowledge::new(store);
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let addr = listener.local_addr()?;
    println!("cce corpus serve: serving http://{addr}/docs?service=…  (loopback only, read-only, bearer-authenticated)");
    println!("knowledge store : {} ({chunks} chunks)", KnowledgeStore::dir(&root).display());
    println!("press Ctrl-C to stop.");
    serve(listener, knowledge, token, top_k, min_score, None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_KNOWLEDGE_MIN_SCORE as MIN;
    use crate::knowledge::parse_ndjson;

    /// A two-record synthetic knowledge store (no real names): a "checkout" service note
    /// and an unrelated one. `search_knowledge` over "checkout" surfaces the first.
    fn loaded() -> LoadedKnowledge {
        let feed = format!(
            "{}\n{}\n",
            serde_json::json!({
                "id": "kn:checkout",
                "title": "Checkout service runbook",
                "body": "## Restart\n\nThe checkout service restarts cleanly; drain connections first.",
                "source": "handbook",
                "updated_at": "2026-02-01T10:00:00Z",
                "labels": ["checkout"],
            }),
            serde_json::json!({
                "id": "kn:other",
                "title": "Data retention window",
                "body": "## Rule\n\nPurge inactive account records after ninety days.",
                "source": "handbook",
                "updated_at": "2026-01-01T00:00:00Z",
                "labels": ["privacy"],
            }),
        );
        let records = parse_ndjson(&feed).unwrap();
        LoadedKnowledge::new(ingest_default(&records, feed.as_bytes()))
    }

    fn req(method: &str, target: &str, auth: Option<&str>) -> Request {
        Request {
            method: method.to_string(),
            target: target.to_string(),
            authorization: auth.map(|a| a.to_string()),
        }
    }

    const TOK: &str = "s3cret-token";

    #[test]
    fn missing_auth_is_401_before_any_route() {
        let k = loaded();
        // Even a well-formed /docs request with no Authorization header is 401.
        let r = route(&req("GET", "/docs?service=checkout", None), TOK, &k, 10, MIN);
        assert_eq!(r.status, 401);
        assert!(r.body.contains("unauthorized"));
        // And an unknown path is 401 too (auth before 404) — no route probing.
        let r = route(&req("GET", "/secret", None), TOK, &k, 10, MIN);
        assert_eq!(r.status, 401);
    }

    #[test]
    fn wrong_token_is_401() {
        let k = loaded();
        let r = route(&req("GET", "/docs?service=checkout", Some("Bearer nope")), TOK, &k, 10, MIN);
        assert_eq!(r.status, 401);
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_token_is_not() {
        let k = loaded();
        // Scheme keyword case-insensitive (RFC 7235)...
        let r = route(
            &req("GET", "/docs?service=checkout", Some(&format!("bearer {TOK}"))),
            TOK,
            &k,
            10,
            MIN,
        );
        assert_eq!(r.status, 200);
        // ...but a token that differs only in case is rejected.
        let r = route(
            &req("GET", "/docs?service=checkout", Some("Bearer S3CRET-TOKEN")),
            TOK,
            &k,
            10,
            MIN,
        );
        assert_eq!(r.status, 401);
    }

    #[test]
    fn authenticated_docs_returns_matching_service_docs() {
        let k = loaded();
        let r = route(
            &req("GET", "/docs?service=checkout", Some(&format!("Bearer {TOK}"))),
            TOK,
            &k,
            10,
            MIN,
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let docs = v["docs"].as_array().unwrap();
        assert!(!docs.is_empty(), "checkout must surface at least one doc: {}", r.body);
        // The consumer's contract: each doc has a truthy id (the 16-hex chunk_id) and a
        // title/body. The top hit is the checkout runbook, not the retention note.
        let id = docs[0]["id"].as_str().unwrap();
        assert_eq!(id.len(), 16, "id is the 16-hex chunk_id: {id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(docs[0]["title"], "Checkout service runbook");
        assert!(docs[0]["body"].as_str().unwrap().contains("checkout service restarts"));
    }

    #[test]
    fn docs_without_service_is_400() {
        let k = loaded();
        let r = route(&req("GET", "/docs", Some(&format!("Bearer {TOK}"))), TOK, &k, 10, MIN);
        assert_eq!(r.status, 400);
        // Empty service value is also "missing".
        let r =
            route(&req("GET", "/docs?service=", Some(&format!("Bearer {TOK}"))), TOK, &k, 10, MIN);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn authenticated_non_get_is_405_unknown_path_is_404() {
        let k = loaded();
        let r = route(
            &req("POST", "/docs?service=checkout", Some(&format!("Bearer {TOK}"))),
            TOK,
            &k,
            10,
            MIN,
        );
        assert_eq!(r.status, 405);
        let r = route(&req("GET", "/nope", Some(&format!("Bearer {TOK}"))), TOK, &k, 10, MIN);
        assert_eq!(r.status, 404);
    }

    #[test]
    fn unmatched_service_returns_empty_docs_not_an_error() {
        let k = loaded();
        let r = route(
            &req("GET", "/docs?service=nonexistent-xyz", Some(&format!("Bearer {TOK}"))),
            TOK,
            &k,
            10,
            MIN,
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["docs"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn service_param_url_decodes_form_component() {
        // The consumer encodes with URI.encode_www_form_component: space→`+`, `/`→`%2F`.
        assert_eq!(service_param("/docs?service=check+out%2F2").as_deref(), Some("check out/2"));
        assert_eq!(service_param("/docs?service=api%2Dgateway").as_deref(), Some("api-gateway"));
        // A later q= param is ignored, not an error (forward-compatible with U2.1).
        assert_eq!(service_param("/docs?service=checkout&q=timeout").as_deref(), Some("checkout"));
        assert_eq!(service_param("/docs").as_deref(), None);
    }

    #[test]
    fn percent_decode_handles_multibyte_and_malformed_tails() {
        // A multibyte UTF-8 codepoint round-trips (é = %C3%A9).
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        // Malformed escapes are emitted literally, never panic: a truncated `%2`/`%` at
        // the very end and a non-hex `%ZZ` all pass through unchanged.
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%"), "a%");
        assert_eq!(percent_decode("a%ZZb"), "a%ZZb");
        // A well-formed escape at the very end still decodes (bounds check is not off-by-one).
        assert_eq!(percent_decode("a%2F"), "a/");
    }

    #[test]
    fn top_k_caps_the_returned_docs() {
        // Three records that all share the query token "runbook"; top_k=2 must truncate.
        let feed = (1..=3)
            .map(|i| {
                serde_json::json!({
                    "id": format!("kn:{i}"),
                    "title": format!("Service {i} runbook"),
                    "body": format!("## Restart\n\nService {i} runbook: restart cleanly."),
                    "source": "handbook",
                    "updated_at": format!("2026-02-0{i}T10:00:00Z"),
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let records = parse_ndjson(&feed).unwrap();
        let k = LoadedKnowledge::new(ingest_default(&records, feed.as_bytes()));
        let r = route(
            &req("GET", "/docs?service=runbook", Some(&format!("Bearer {TOK}"))),
            TOK,
            &k,
            2,
            MIN,
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["docs"].as_array().unwrap().len(), 2, "top_k=2 caps the set: {}", r.body);
    }

    #[test]
    fn ct_eq_matches_only_identical_bytes() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"x"));
    }

    #[test]
    fn short_body_passes_through_unchanged() {
        // A body already within the cap is served verbatim — no cap, no elision marker.
        let short = "## Restart\n\nThe checkout service restarts cleanly; drain connections first.";
        assert!(short.chars().count() <= BODY_CHAR_CAP);
        assert_eq!(body_snippet(short), short);
    }

    #[test]
    fn long_body_is_capped_at_500_chars_on_a_sentence_boundary() {
        // A multi-sentence body over the cap: the snippet stays ≤500 chars AND ends on a
        // whole sentence (a real ADR round-trips as a useful snippet, not a mid-sentence
        // cut). U2.2 — G19.
        let sentence = "The gateway drains in-flight requests before it exits. ";
        let body = sentence.repeat(20); // ~1100 chars, every sentence ends in `. `
        assert!(body.chars().count() > BODY_CHAR_CAP);
        let snip = body_snippet(&body);
        assert!(snip.chars().count() <= BODY_CHAR_CAP, "capped: {} chars", snip.chars().count());
        assert!(snip.chars().count() < body.chars().count(), "actually truncated");
        // Ends on a complete sentence, not mid-word: last char is a sentence terminator.
        let last = snip.chars().next_back().unwrap();
        assert!(matches!(last, '.' | '!' | '?'), "ends on a whole sentence, got {last:?}: {snip}");
        // And it is a genuine prefix of the source — we never rewrite the text, only cut it.
        assert!(body.starts_with(&snip), "snippet is a prefix of the source");
    }

    #[test]
    fn long_unbroken_sentence_cuts_at_a_word_boundary_with_ellipsis() {
        // No sentence terminator within the cap (one very long clause): fall back to the
        // last WORD boundary and mark the elision with U+2026 — never split a word.
        let body = "alpha beta gamma delta epsilon ".repeat(30); // ~930 chars, no `.`/`!`/`?`
        assert!(body.chars().count() > BODY_CHAR_CAP);
        let snip = body_snippet(&body);
        assert!(snip.chars().count() <= BODY_CHAR_CAP, "capped: {}", snip.chars().count());
        assert!(snip.ends_with('…'), "mid-sentence cut is marked with an ellipsis: {snip}");
        // Strip the marker: the remainder is a prefix of the source that ends exactly at a
        // word boundary — the source char immediately after it is whitespace.
        let kept = snip.strip_suffix('…').unwrap();
        assert!(body.starts_with(kept), "kept text is a prefix of the source");
        let next = body.chars().nth(kept.chars().count()).unwrap();
        assert!(next.is_whitespace(), "cut lands on a word boundary, next char is {next:?}");
    }

    #[test]
    fn body_snippet_never_splits_a_multibyte_char() {
        // A cap that would fall inside a multi-byte scalar must not panic or emit a
        // replacement char — the cut is on a char boundary by construction.
        let body = "é".repeat(BODY_CHAR_CAP + 50); // every char is 2 bytes
        let snip = body_snippet(&body);
        assert!(snip.chars().count() <= BODY_CHAR_CAP);
        assert!(!snip.contains('\u{FFFD}'), "no replacement char from a torn scalar");
    }

    #[test]
    fn served_docs_cap_every_body_and_keep_ranking() {
        // End-to-end through the route: a knowledge record whose section exceeds the cap
        // is served as a ≤500-char boundary-cut snippet, most-relevant-first ordering
        // preserved. This is the U2.2 acceptance: ≤500-char bodies + ranked, ADR-shaped.
        let long_adr = "The gateway rejects a request when its upstream pool is saturated. \
             Operators restart it with a rolling drain so no in-flight call is dropped. \
             The runbook pins the drain timeout to thirty seconds. \
             Escalate to the platform on-call if the pool stays saturated after a restart. \
             A saturated pool almost always traces back to a slow downstream dependency. \
             Check the dependency's latency dashboard before assuming the gateway is at fault. \
             Never bounce the gateway twice in a row without reading the drain log in between. \
             The last two incidents were both a misconfigured connection ceiling, not load."
            .to_string();
        assert!(long_adr.chars().count() > BODY_CHAR_CAP, "fixture must exceed the cap");
        let feed = format!(
            "{}\n{}\n",
            serde_json::json!({
                "id": "kn:gateway",
                "title": "Gateway saturation runbook",
                "body": format!("## Saturation\n\n{long_adr}"),
                "source": "handbook",
                "updated_at": "2026-03-01T10:00:00Z",
                "labels": ["gateway"],
            }),
            serde_json::json!({
                "id": "kn:unrelated",
                "title": "Data retention window",
                "body": "## Rule\n\nPurge inactive account records after ninety days.",
                "source": "handbook",
                "updated_at": "2026-01-01T00:00:00Z",
                "labels": ["privacy"],
            }),
        );
        let records = parse_ndjson(&feed).unwrap();
        let k = LoadedKnowledge::new(ingest_default(&records, feed.as_bytes()));
        let r = route(
            &req("GET", "/docs?service=gateway", Some(&format!("Bearer {TOK}"))),
            TOK,
            &k,
            10,
            MIN,
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let docs = v["docs"].as_array().unwrap();
        assert!(!docs.is_empty(), "gateway must surface a doc: {}", r.body);
        // Most-relevant-first: the gateway runbook outranks the unrelated retention note.
        assert_eq!(docs[0]["title"], "Gateway saturation runbook");
        // Every served body respects the cap and ends cleanly (whole sentence or ellipsis).
        for d in docs {
            let body = d["body"].as_str().unwrap();
            assert!(body.chars().count() <= BODY_CHAR_CAP, "body ≤{BODY_CHAR_CAP}: {body}");
            let last = body.chars().next_back().unwrap();
            assert!(
                matches!(last, '.' | '!' | '?' | '…'),
                "served body ends on a boundary, not mid-word: {body:?}"
            );
        }
        // The capped gateway body is still a useful snippet: it carries the opening of the
        // ADR verbatim (the incident-ready lede), not a blank or a mid-sentence stub.
        let top_body = docs[0]["body"].as_str().unwrap();
        assert!(
            top_body.contains("The gateway rejects a request when its upstream pool is saturated."),
            "snippet keeps the ADR's opening sentence intact: {top_body}"
        );
    }

    #[test]
    fn empty_store_serves_empty_docs() {
        let k = LoadedKnowledge::new(ingest_default(&[], b""));
        let r = route(
            &req("GET", "/docs?service=checkout", Some(&format!("Bearer {TOK}"))),
            TOK,
            &k,
            10,
            MIN,
        );
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["docs"].as_array().unwrap().len(), 0);
    }
}
