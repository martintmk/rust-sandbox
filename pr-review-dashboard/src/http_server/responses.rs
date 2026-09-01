// Licensed under the MIT License.

//! Response-building helpers. Every response is funneled through [`build`], so
//! [`apply_security_headers`] cannot be forgotten on any response path, including error paths.

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode, header};
use serde::Serialize;

use super::security::apply_security_headers;

/// The response body type produced by every handler: an in-memory byte buffer, since no route
/// streams a response.
pub(super) type Body = Full<Bytes>;

fn build(status: StatusCode, content_type: &'static str, body: Bytes) -> Response<Body> {
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, header::HeaderValue::from_static(content_type));
    apply_security_headers(response.headers_mut());
    response
}

pub(super) fn html(status: StatusCode, body: String) -> Response<Body> {
    build(status, "text/html; charset=utf-8", Bytes::from(body))
}

pub(super) fn json(status: StatusCode, value: &impl Serialize) -> Response<Body> {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{\"error\":\"serialization failed\"}".to_vec());
    build(status, "application/json", Bytes::from(bytes))
}

pub(super) fn plain(status: StatusCode, body: &'static str) -> Response<Body> {
    build(status, "text/plain; charset=utf-8", Bytes::from_static(body.as_bytes()))
}

/// Serves one of the embedded static assets ([`super::assets`]) with an explicit content type.
pub(super) fn static_asset(content_type: &'static str, body: &'static str) -> Response<Body> {
    build(StatusCode::OK, content_type, Bytes::from_static(body.as_bytes()))
}

/// A `303 See Other` redirect used by the no-JavaScript form fallback: after a mutating action is
/// enqueued via a full-page form submit, the browser is redirected back to a same-origin path
/// (always built from validated numeric identifiers, never from client-supplied text) so it lands
/// on a rendered page instead of the JSON action response.
pub(super) fn see_other(location: &str) -> Response<Body> {
    let mut response = build(
        StatusCode::SEE_OTHER,
        "text/plain; charset=utf-8",
        Bytes::from_static(b"redirecting"),
    );
    if let Ok(value) = header::HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

pub(super) fn not_found() -> Response<Body> {
    plain(StatusCode::NOT_FOUND, "not found")
}

pub(super) fn bad_request(message: &'static str) -> Response<Body> {
    plain(StatusCode::BAD_REQUEST, message)
}

pub(super) fn payload_too_large() -> Response<Body> {
    plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
}

pub(super) fn uri_too_long() -> Response<Body> {
    plain(StatusCode::URI_TOO_LONG, "request target too long")
}

pub(super) fn forbidden(message: &'static str) -> Response<Body> {
    plain(StatusCode::FORBIDDEN, message)
}
