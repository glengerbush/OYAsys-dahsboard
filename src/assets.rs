//! Immutable build-compressed assets: no compression or filesystem I/O per request.
use bytes::Bytes;
use http_body_util::Full;
use hyper::{body::Incoming, Request, Response, StatusCode};

struct Asset {
    alias: &'static str,
    route: &'static str,
    kind: &'static str,
    etag: &'static str,
    raw: &'static [u8],
    gzip: &'static [u8],
}
include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));

pub fn serve(path: &str, request: &Request<Incoming>) -> Option<Response<Full<Bytes>>> {
    let asset = ASSETS
        .iter()
        .find(|asset| path == asset.alias || path == asset.route)?;
    let compressed = accepts_gzip(request);
    let etag = format!(
        "\"{}{}\"",
        asset.etag,
        if compressed { "-gzip" } else { "" }
    );
    let unchanged = request
        .headers()
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate.trim().trim_start_matches("W/");
                candidate == "*" || candidate == etag
            })
        });
    let bytes = if compressed { asset.gzip } else { asset.raw };
    let mut response = Response::new(Full::new(
        if unchanged || request.method() == hyper::Method::HEAD {
            Bytes::new()
        } else {
            Bytes::from_static(bytes)
        },
    ));
    if unchanged {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
    }
    let headers = response.headers_mut();
    headers.insert("content-type", asset.kind.parse().unwrap());
    headers.insert("etag", etag.parse().unwrap());
    headers.insert("vary", "Accept-Encoding".parse().unwrap());
    headers.insert(
        "cache-control",
        if path.starts_with("/assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }
        .parse()
        .unwrap(),
    );
    if compressed {
        headers.insert("content-encoding", "gzip".parse().unwrap());
    }
    if request.method() == hyper::Method::HEAD && !unchanged {
        headers.insert("content-length", bytes.len().to_string().parse().unwrap());
    }
    Some(response)
}

fn accepts_gzip(request: &Request<Incoming>) -> bool {
    let Some(value) = request
        .headers()
        .get("accept-encoding")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let mut wildcard = None;
    for entry in value.split(',') {
        let mut fields = entry.split(';');
        let coding = fields.next().unwrap_or("").trim();
        let quality = fields
            .find_map(|parameter| {
                parameter
                    .trim()
                    .strip_prefix("q=")
                    .map(|q| q.parse::<f32>().unwrap_or(0.0))
            })
            .unwrap_or(1.0);
        if coding.eq_ignore_ascii_case("gzip") {
            return quality > 0.0;
        }
        if coding == "*" {
            wildcard = Some(quality > 0.0);
        }
    }
    wildcard.unwrap_or(false)
}
