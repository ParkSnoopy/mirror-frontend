use std::{
    collections::HashSet,
    env,
    fmt,
    net::SocketAddr,
    sync::Arc,
    time::SystemTime,
};

use axum::{
    Router,
    body::Body,
    extract::{
        ConnectInfo,
        State,
    },
    http::{
        HeaderMap,
        HeaderName,
        HeaderValue,
        Request,
        Response,
        StatusCode,
        header,
    },
    response::IntoResponse,
    routing::any,
};
use reqwest::redirect::Policy;
use url::Url;

const UPSTREAM_ENV: &str = "MIRROR_UPSTREAM";
const BIND_ENV: &str = "MIRROR_BIND";
const DEBUG_ENV: &str = "DEBUG";

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    upstream: Url,
    debug: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        log(LogCategory::Err, &format!("startup failed: {error}"));
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    load_env_file()?;
    let upstream = env::var(UPSTREAM_ENV)
        .map_err(|_| format!("{UPSTREAM_ENV} is required"))
        .and_then(|value| parse_upstream_host(&value))?;
    let bind = env::var(BIND_ENV).unwrap_or_else(|_| "0.0.0.0:3000".to_owned());
    let bind: SocketAddr = bind
        .parse()
        .map_err(|error| format!("invalid {BIND_ENV}: {error}"))?;
    let debug = parse_debug(env::var(DEBUG_ENV).ok().as_deref())?;
    let state = AppState {
        client: reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(|error| format!("cannot build HTTP client: {error}"))?,
        upstream,
        debug,
    };
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|error| format!("cannot listen on {bind}: {error}"))?;
    if debug {
        log(LogCategory::Info, &format!("listening={bind}"));
    }

    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(|error| format!("server failed: {error}"))
}

fn load_env_file() -> Result<(), String> {
    match dotenvy::from_filename(".env") {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot load .env: {error}")),
    }
}

fn parse_debug(value: Option<&str>) -> Result<bool, String> {
    match value {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err(format!("{DEBUG_ENV} must be true or false")),
    }
}

fn app(state: AppState) -> Router {
    Router::new()
        .fallback(any(proxy))
        .with_state(Arc::new(state))
}

async fn proxy(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Result<Response<Body>, ProxyError> {
    let client_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect_info| connect_info.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let path = request.uri().path().to_owned();
    let result = async {
        let public_url = request_public_url(&request)?;
        let target = target_url(&state.upstream, request.uri());
        let (parts, body) = request.into_parts();
        let mut headers = filtered_headers(&parts.headers, true);
        rewrite_request_headers(&mut headers, &public_url, &state.upstream);
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );

        let upstream_response = state
            .client
            .request(parts.method, target)
            .headers(headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await
            .map_err(|_| ProxyError::Upstream)?;

        build_response(upstream_response, &state.upstream, &public_url).await
    }
    .await;

    if state.debug {
        let status = match &result {
            Ok(response) => response.status(),
            Err(error) => error.status(),
        };
        log(
            LogCategory::for_status(status),
            &format!("ip={client_ip} path={path} status={status}"),
        );
    }
    result
}

async fn build_response(
    upstream_response: reqwest::Response,
    upstream: &Url,
    public_url: &Url,
) -> Result<Response<Body>, ProxyError> {
    let status = upstream_response.status();
    let source_headers = upstream_response.headers().clone();
    let rewrite_body =
        is_rewritable(&source_headers) && !source_headers.contains_key(header::CONTENT_ENCODING);
    let mut headers = filtered_headers(&source_headers, rewrite_body);
    rewrite_response_headers(&mut headers, upstream, public_url);

    let body = if rewrite_body {
        let bytes = upstream_response
            .bytes()
            .await
            .map_err(|_| ProxyError::Upstream)?;
        Body::from(rewrite_bytes(&bytes, upstream, public_url))
    } else {
        Body::from_stream(upstream_response.bytes_stream())
    };

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

fn parse_http_url(value: &str, name: &str) -> Result<Url, String> {
    let value = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let mut url = Url::parse(&value).map_err(|error| format!("invalid {name}: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("{name} must be an HTTP or HTTPS URL"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{name} must not contain credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!("{name} must not contain a query or fragment"));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn parse_upstream_host(value: &str) -> Result<Url, String> {
    let error = || {
        format!("{UPSTREAM_ENV} must be a hostname without scheme, port, path, query, or fragment")
    };
    if value.is_empty() || value.trim() != value || value.contains(['/', ':', '?', '#', '@']) {
        return Err(error());
    }
    let url = Url::parse(&format!("https://{value}")).map_err(|_| error())?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(error());
    }
    Ok(url)
}

fn target_url(upstream: &Url, request_uri: &axum::http::Uri) -> Url {
    let mut target = upstream.clone();
    let base = upstream.path().trim_end_matches('/');
    let path = request_uri.path();
    target.set_path(&format!("{base}{path}"));
    target.set_query(request_uri.query());
    target
}

fn request_public_url(request: &Request<Body>) -> Result<Url, ProxyError> {
    let headers = request.headers();
    let host = first_forwarded_value(headers, "x-forwarded-host")
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
        })
        .ok_or(ProxyError::BadRequest("missing or invalid Host header"))?;
    let scheme = first_forwarded_value(headers, "x-forwarded-proto").unwrap_or("http");
    if !matches!(scheme, "http" | "https") {
        return Err(ProxyError::BadRequest("invalid X-Forwarded-Proto header"));
    }
    parse_http_url(&format!("{scheme}://{host}"), "Host")
        .map_err(|_| ProxyError::BadRequest("invalid Host header"))
}

fn first_forwarded_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn filtered_headers(headers: &HeaderMap, content_changed: bool) -> HeaderMap {
    let mut blocked = hop_by_hop_headers(headers);
    blocked.insert(header::HOST);
    if content_changed {
        blocked.insert(header::CONTENT_LENGTH);
    }

    let mut result = HeaderMap::new();
    for (name, value) in headers {
        if !blocked.contains(name) {
            result.append(name, value.clone());
        }
    }
    result
}

fn hop_by_hop_headers(headers: &HeaderMap) -> HashSet<HeaderName> {
    let mut names = HashSet::from([
        header::CONNECTION,
        HeaderName::from_static("keep-alive"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ]);
    if let Some(connection) = headers
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
    {
        for name in connection.split(',').map(str::trim) {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                names.insert(name);
            }
        }
    }
    names
}

fn rewrite_request_headers(headers: &mut HeaderMap, public_url: &Url, upstream: &Url) {
    for name in [header::ORIGIN, header::REFERER] {
        rewrite_header(headers, name, public_url, upstream);
    }
}

fn rewrite_response_headers(headers: &mut HeaderMap, upstream: &Url, public_url: &Url) {
    for name in [
        header::LOCATION,
        header::CONTENT_LOCATION,
        header::LINK,
        header::REFRESH,
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::CONTENT_SECURITY_POLICY,
    ] {
        rewrite_header(headers, name, upstream, public_url);
    }
    rewrite_cookies(headers, upstream);
}

fn rewrite_header(headers: &mut HeaderMap, name: HeaderName, from: &Url, to: &Url) {
    let values: Vec<_> = headers
        .get_all(&name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|value| rewrite_text(value, from, to))
        .filter_map(|value| HeaderValue::from_str(&value).ok())
        .collect();
    if values.is_empty() {
        return;
    }
    headers.remove(&name);
    for value in values {
        headers.append(&name, value);
    }
}

fn rewrite_cookies(headers: &mut HeaderMap, upstream: &Url) {
    let Some(host) = upstream.host_str() else {
        return;
    };
    let values: Vec<_> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|cookie| {
            cookie
                .split(';')
                .filter(|part| {
                    let part = part.trim();
                    !part
                        .strip_prefix("Domain=")
                        .or_else(|| part.strip_prefix("domain="))
                        .is_some_and(|domain| {
                            domain.trim_start_matches('.').eq_ignore_ascii_case(host)
                        })
                })
                .collect::<Vec<_>>()
                .join(";")
        })
        .filter_map(|value| HeaderValue::from_str(&value).ok())
        .collect();
    if values.is_empty() {
        return;
    }
    headers.remove(header::SET_COOKIE);
    for value in values {
        headers.append(header::SET_COOKIE, value);
    }
}

fn is_rewritable(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| {
            mime.starts_with("text/")
                || matches!(
                    mime.trim(),
                    "application/javascript"
                        | "application/json"
                        | "application/manifest+json"
                        | "application/xhtml+xml"
                        | "application/xml"
                        | "image/svg+xml"
                )
        })
}

fn rewrite_bytes(bytes: &[u8], from: &Url, to: &Url) -> Vec<u8> {
    match std::str::from_utf8(bytes) {
        Ok(text) => rewrite_text(text, from, to).into_bytes(),
        Err(_) => bytes.to_vec(),
    }
}

fn rewrite_text(value: &str, from: &Url, to: &Url) -> String {
    let to_origin = to.origin().ascii_serialization();
    let from_authority = authority(from);
    let to_authority = authority(to);

    let mut rewritten = value.to_owned();
    for scheme in ["https", "http", "ftp", "rsync"] {
        rewritten = rewritten.replace(&format!("{scheme}://{from_authority}"), &to_origin);
    }
    rewritten.replace(&format!("//{from_authority}"), &format!("//{to_authority}"))
}

fn authority(url: &Url) -> String {
    match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    }
}

#[derive(Debug)]
enum ProxyError {
    BadRequest(&'static str),
    Upstream,
}

impl ProxyError {
    fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Upstream => StatusCode::BAD_GATEWAY,
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message).into_response(),
            Self::Upstream => (StatusCode::BAD_GATEWAY, "upstream request failed").into_response(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogCategory {
    Info,
    Warn,
    Err,
}

impl LogCategory {
    fn for_status(status: StatusCode) -> Self {
        if status.is_server_error() {
            Self::Err
        } else if status.is_client_error() {
            Self::Warn
        } else {
            Self::Info
        }
    }
}

impl fmt::Display for LogCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Err => "ERR",
        })
    }
}

fn log(category: LogCategory, message: &str) {
    let timestamp = humantime::format_rfc3339_seconds(SystemTime::now());
    eprintln!("{timestamp} {category:<4} {message}");
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn parses_debug_setting() {
        assert!(!parse_debug(None).unwrap());
        assert!(!parse_debug(Some("false")).unwrap());
        assert!(parse_debug(Some("true")).unwrap());
        assert!(parse_debug(Some("TRUE")).is_err());
        assert!(parse_debug(Some("1")).is_err());
    }

    #[test]
    fn categorizes_request_status() {
        assert_eq!(LogCategory::for_status(StatusCode::OK), LogCategory::Info);
        assert_eq!(
            LogCategory::for_status(StatusCode::NOT_FOUND),
            LogCategory::Warn
        );
        assert_eq!(
            LogCategory::for_status(StatusCode::BAD_GATEWAY),
            LogCategory::Err
        );
        assert_eq!(LogCategory::Info.to_string(), "INFO");
        assert_eq!(LogCategory::Warn.to_string(), "WARN");
        assert_eq!(LogCategory::Err.to_string(), "ERR");
        assert_eq!(format!("{:<4}", LogCategory::Info), "INFO");
        assert_eq!(format!("{:<4}", LogCategory::Warn), "WARN");
        assert_eq!(format!("{:<4}", LogCategory::Err), "ERR ");
    }

    #[test]
    fn parses_clean_upstream_hostname() {
        let upstream = parse_upstream_host("upstream.example").unwrap();
        let uri = "/linux/file.iso?download=1".parse().unwrap();

        assert_eq!(upstream.as_str(), "https://upstream.example/");
        assert_eq!(
            target_url(&upstream, &uri).as_str(),
            "https://upstream.example/linux/file.iso?download=1"
        );
    }

    #[test]
    fn rejects_upstream_values_that_are_not_clean_hostnames() {
        for value in [
            "https://mirror.example.com",
            "http://mirror.example.com/",
            "https://mirror.example.com/",
            "mirror.example.com/",
            "mirror.example.com:443",
            "mirror.example.com/path",
            "mirror.example.com?query",
            "user@mirror.example.com",
        ] {
            assert!(parse_upstream_host(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn removes_hop_by_hop_headers_named_by_connection() {
        let headers = HeaderMap::from_iter([
            (
                header::CONNECTION,
                HeaderValue::from_static("keep-alive, x-private"),
            ),
            (
                HeaderName::from_static("x-private"),
                HeaderValue::from_static("secret"),
            ),
            (
                HeaderName::from_static("x-public"),
                HeaderValue::from_static("visible"),
            ),
        ]);

        let filtered = filtered_headers(&headers, false);

        assert!(!filtered.contains_key(header::CONNECTION));
        assert!(!filtered.contains_key("x-private"));
        assert_eq!(filtered["x-public"], "visible");
    }

    #[test]
    fn rewrites_upstream_urls_in_text() {
        let upstream = Url::parse("https://upstream.example/").unwrap();
        let public = Url::parse("https://mirror.example:8443/").unwrap();

        assert_eq!(
            rewrite_text(
                "https://upstream.example/a //upstream.example/b ftp://upstream.example/c rsync://upstream.example/d",
                &upstream,
                &public
            ),
            "https://mirror.example:8443/a //mirror.example:8443/b https://mirror.example:8443/c https://mirror.example:8443/d"
        );
    }

    #[tokio::test]
    async fn missing_host_is_rejected() {
        let state = AppState {
            client: reqwest::Client::new(),
            upstream: Url::parse("https://example.com/").unwrap(),
            debug: false,
        };
        let request = Request::builder().uri("/").body(Body::empty()).unwrap();

        let response = app(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            "missing or invalid Host header"
        );
    }

    #[test]
    fn derives_public_url_from_forwarded_request_headers() {
        let request = Request::builder()
            .uri("/")
            .header(header::HOST, "internal:3000")
            .header("x-forwarded-host", "one.example, internal:3000")
            .header("x-forwarded-proto", "https, http")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            request_public_url(&request).unwrap().as_str(),
            "https://one.example/"
        );
    }

    #[tokio::test]
    async fn relays_method_path_query_body_and_rewrites_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream_origin = format!("http://{address}");
        let body_origin = upstream_origin.clone();
        let upstream_app = Router::new().fallback(any(move |request: Request<Body>| {
            let body_origin = body_origin.clone();
            async move {
                let method = request.method().clone();
                let uri = request.uri().clone();
                let body = to_bytes(request.into_body(), 1024).await.unwrap();
                (
                    [(header::CONTENT_TYPE, "text/html")],
                    format!(
                        "{method} {uri} {} <a href=\"{body_origin}/asset\">asset</a>",
                        String::from_utf8(body.to_vec()).unwrap()
                    ),
                )
            }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream_app).await.unwrap();
        });
        let state = AppState {
            client: reqwest::Client::builder()
                .redirect(Policy::none())
                .build()
                .unwrap(),
            upstream: Url::parse(&format!("{upstream_origin}/base/")).unwrap(),
            debug: false,
        };
        let request = Request::builder()
            .method("POST")
            .uri("/nested?q=1")
            .header(header::HOST, "mirror.example")
            .body(Body::from("payload"))
            .unwrap();

        let response = app(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 4096).await.unwrap(),
            "POST /base/nested?q=1 payload <a href=\"http://mirror.example/asset\">asset</a>"
        );
        server.abort();
    }
}
