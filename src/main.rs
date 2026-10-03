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
const SHADOW_DOMAIN_ENV: &str = "SHADOW_DOMAIN";
const BIND_ENV: &str = "MIRROR_BIND";
const DEBUG_ENV: &str = "DEBUG";
const GOOGLE_FAIL_ENV: &str = "GOOGLE_FAIL";
const GOOGLE_FAIL_PATH: &str = "/__mirror_response";
const GOOGLE_FAIL_DOMAINS: &str = include_str!("../google-fail-domains.txt");

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    upstream: Url,
    shadow_domain: Option<Url>,
    google_fail: Option<GoogleFail>,
    debug: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GoogleFail {
    Forbidden,
    NotFound,
    InternalServerError,
}

impl GoogleFail {
    fn response(self) -> (StatusCode, &'static str) {
        match self {
            Self::Forbidden => (StatusCode::FORBIDDEN, "Forbidden"),
            Self::NotFound => (StatusCode::NOT_FOUND, "Not Found"),
            Self::InternalServerError => (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
        }
    }
}

impl IntoResponse for GoogleFail {
    fn into_response(self) -> Response<Body> {
        self.response().into_response()
    }
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
    let shadow_domain = parse_optional_host(
        SHADOW_DOMAIN_ENV,
        env::var(SHADOW_DOMAIN_ENV).ok().as_deref(),
    )?;
    let debug = parse_debug(env::var(DEBUG_ENV).ok().as_deref())?;
    let google_fail = match env::var(GOOGLE_FAIL_ENV) {
        Ok(value) => parse_google_fail(Some(&value))?,
        Err(env::VarError::NotPresent) => None,
        Err(error) => return Err(format!("invalid {GOOGLE_FAIL_ENV}: {error}")),
    };
    let state = AppState {
        client: reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(|error| format!("cannot build HTTP client: {error}"))?,
        upstream,
        shadow_domain,
        google_fail,
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

fn parse_google_fail(value: Option<&str>) -> Result<Option<GoogleFail>, String> {
    match value {
        None => Ok(None),
        Some("403") => Ok(Some(GoogleFail::Forbidden)),
        Some("404") => Ok(Some(GoogleFail::NotFound)),
        Some("500") => Ok(Some(GoogleFail::InternalServerError)),
        Some(_) => Err(format!("{GOOGLE_FAIL_ENV} must be 403, 404, or 500")),
    }
}

fn is_google_service(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    GOOGLE_FAIL_DOMAINS
        .lines()
        .map(str::trim)
        .filter(|domain| !domain.is_empty() && !domain.starts_with('#'))
        .any(|domain| {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            host.strip_suffix(&domain)
                .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('.'))
        })
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
        if let Some(failure) = state.google_fail
            && (is_google_service(&state.upstream)
                || path == GOOGLE_FAIL_PATH
                || path.starts_with(&format!("{GOOGLE_FAIL_PATH}/")))
        {
            return Ok(failure.into_response());
        }
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

        build_response(upstream_response, &state, &public_url).await
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
    state: &AppState,
    public_url: &Url,
) -> Result<Response<Body>, ProxyError> {
    let rewrite_url = state.shadow_domain.as_ref().unwrap_or(public_url);
    let google_url = state.google_fail.map(|_| public_url);
    let status = upstream_response.status();
    let source_headers = upstream_response.headers().clone();
    let rewrite_body =
        is_rewritable(&source_headers) && !source_headers.contains_key(header::CONTENT_ENCODING);
    let mut headers = filtered_headers(&source_headers, rewrite_body);
    rewrite_response_headers(&mut headers, &state.upstream, rewrite_url, google_url);

    let body = if rewrite_body {
        let bytes = upstream_response
            .bytes()
            .await
            .map_err(|_| ProxyError::Upstream)?;
        Body::from(rewrite_bytes(&bytes, &state.upstream, rewrite_url, google_url))
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
    parse_host(UPSTREAM_ENV, value)
}

fn parse_optional_host(name: &str, value: Option<&str>) -> Result<Option<Url>, String> {
    match value {
        None | Some("") => Ok(None),
        Some(value) => parse_host(name, value).map(Some),
    }
}

fn parse_host(name: &str, value: &str) -> Result<Url, String> {
    let error = || format!("{name} must be a hostname without scheme, port, path, query, or fragment");
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
        rewrite_header(headers, name, public_url, upstream, None);
    }
}

fn rewrite_response_headers(
    headers: &mut HeaderMap,
    upstream: &Url,
    public_url: &Url,
    google_url: Option<&Url>,
) {
    for name in [
        header::LOCATION,
        header::CONTENT_LOCATION,
        header::LINK,
        header::REFRESH,
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::CONTENT_SECURITY_POLICY,
    ] {
        rewrite_header(headers, name, upstream, public_url, google_url);
    }
    rewrite_cookies(headers, upstream);
}

fn rewrite_header(
    headers: &mut HeaderMap,
    name: HeaderName,
    from: &Url,
    to: &Url,
    google_url: Option<&Url>,
) {
    let values: Vec<_> = headers
        .get_all(&name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|value| rewrite_text(value, from, to, google_url))
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

fn rewrite_bytes(bytes: &[u8], from: &Url, to: &Url, google_url: Option<&Url>) -> Vec<u8> {
    match std::str::from_utf8(bytes) {
        Ok(text) => rewrite_text(text, from, to, google_url).into_bytes(),
        Err(_) => bytes.to_vec(),
    }
}

fn rewrite_text(value: &str, from: &Url, to: &Url, google_url: Option<&Url>) -> String {
    let to_origin = to.origin().ascii_serialization();
    let from_authority = authority(from);
    let to_authority = authority(to);

    let mut rewritten = value.to_owned();
    for scheme in ["https", "http", "ftp", "rsync"] {
        rewritten = rewritten.replace(&format!("{scheme}://{from_authority}"), &to_origin);
    }
    rewritten = rewritten.replace(&format!("//{from_authority}"), &format!("//{to_authority}"));
    match google_url {
        Some(public_url) => rewrite_google_urls(&rewritten, public_url),
        None => rewritten,
    }
}

fn rewrite_google_urls(value: &str, public_url: &Url) -> String {
    // ponytail: literal URLs only; use a syntax-aware parser if escaped forms must be supported.
    let replacement = format!(
        "{}{GOOGLE_FAIL_PATH}",
        public_url.origin().ascii_serialization()
    );
    let mut rewritten = String::new();
    let mut cursor = 0;
    for (index, _) in value.match_indices("//") {
        let start = if index >= 6
            && value
                .get(index - 6..index)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https:"))
        {
            index - 6
        } else if index >= 5
            && value
                .get(index - 5..index)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http:"))
        {
            index - 5
        } else if index == 0
            || value[..index].chars().next_back().is_some_and(|ch| {
                ch.is_whitespace() || matches!(ch, '\'' | '"' | '(' | '=' | '<' | '>' | '`')
            })
        {
            index
        } else {
            continue;
        };
        let authority = value[index + 2..]
            .split(|ch: char| {
                ch.is_whitespace()
                    || matches!(ch, '/' | '?' | '#' | '\'' | '"' | '(' | ')' | '<' | '>' | '\\' | '`' | ';' | '{' | '}')
            })
            .next()
            .unwrap_or_default();
        if start < cursor
            || !Url::parse(&format!("https://{authority}/")).is_ok_and(|url| is_google_service(&url))
        {
            continue;
        }
        rewritten.push_str(&value[cursor..start]);
        rewritten.push_str(&replacement);
        cursor = index + 2 + authority.len();
    }
    rewritten.push_str(&value[cursor..]);
    rewritten
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
    fn parses_google_fail_setting() {
        assert_eq!(parse_google_fail(None).unwrap(), None);
        for (value, failure, status, body) in [
            ("403", GoogleFail::Forbidden, StatusCode::FORBIDDEN, "Forbidden"),
            ("404", GoogleFail::NotFound, StatusCode::NOT_FOUND, "Not Found"),
            ("500", GoogleFail::InternalServerError, StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
        ] {
            assert_eq!(
                parse_google_fail(Some(value)).unwrap(),
                Some(failure)
            );
            assert_eq!(failure.response(), (status, body));
        }
        for value in ["", "100", "200", "301", "401", "410", "502", "503", "599", "600", "999", "4030", " 403", "403 ", "abc"] {
            assert!(parse_google_fail(Some(value)).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn rewrites_google_service_urls_without_matching_lookalikes() {
        let public = Url::parse("https://mirror.example:8443/").unwrap();
        for source in [
            "https://googleapis.com/css2?family=Roboto",
            "HTTP://Fonts.GoogleApis.Com/css2?family=Roboto",
            "//fonts.googleapis.com/css2?family=Roboto",
            "https://fonts.gstatic.com:443/css2?family=Roboto",
            "https://fonts.gstatic.com./css2?family=Roboto",
        ] {
            assert_eq!(
                rewrite_google_urls(&format!("字体 @import url('{source}');"), &public),
                "字体 @import url('https://mirror.example:8443/__mirror_response/css2?family=Roboto');"
            );
        }
        for source in [
            "https://notgoogleapis.com/css",
            "https://googleapis.com.example/css",
            "https://fonts.gstatic.com@other.example/css",
            "https://example.com//fonts.googleapis.com/css",
            "//example.com/fonts.googleapis.com/css",
            "ftp://fonts.googleapis.com/css",
            "https:\\/\\/fonts.googleapis.com/css",
        ] {
            assert_eq!(rewrite_google_urls(source, &public), source);
        }
        assert_eq!(
            rewrite_google_urls(
                "@import 'https://fonts.googleapis.com/css'; src:url(//fonts.gstatic.com/font.woff2);",
                &public
            ),
            "@import 'https://mirror.example:8443/__mirror_response/css'; src:url(https://mirror.example:8443/__mirror_response/font.woff2);"
        );
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
        assert_eq!(
            parse_optional_host(SHADOW_DOMAIN_ENV, Some("shadow.example"))
                .unwrap()
                .unwrap()
                .as_str(),
            "https://shadow.example/"
        );
        assert!(
            parse_optional_host(SHADOW_DOMAIN_ENV, None)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_optional_host(SHADOW_DOMAIN_ENV, Some(""))
                .unwrap()
                .is_none()
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
            assert!(
                parse_optional_host(SHADOW_DOMAIN_ENV, Some(value)).is_err(),
                "accepted shadow domain {value}"
            );
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
                &public,
                None
            ),
            "https://mirror.example:8443/a //mirror.example:8443/b https://mirror.example:8443/c https://mirror.example:8443/d"
        );
    }

    #[test]
    fn rewrites_cors_and_redirect_headers_to_shadow_domain() {
        let upstream = Url::parse("https://upstream.example/").unwrap();
        let shadow = Url::parse("https://shadow.example/").unwrap();
        let mut headers = HeaderMap::from_iter([
            (
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("https://upstream.example"),
            ),
            (
                header::LOCATION,
                HeaderValue::from_static("http://upstream.example/asset.js"),
            ),
        ]);

        rewrite_response_headers(&mut headers, &upstream, &shadow, None);

        assert_eq!(
            headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://shadow.example"
        );
        assert_eq!(
            headers[header::LOCATION],
            "https://shadow.example/asset.js"
        );
    }

    #[tokio::test]
    async fn missing_host_is_rejected() {
        let state = AppState {
            client: reqwest::Client::new(),
            upstream: Url::parse("https://example.com/").unwrap(),
            shadow_domain: None,
            google_fail: None,
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
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream_origin = format!("http://{address}");
        let body_origin = upstream_origin.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let upstream_requests = requests.clone();
        let upstream_app = Router::new().fallback(any(move |request: Request<Body>| {
            let body_origin = body_origin.clone();
            let upstream_requests = upstream_requests.clone();
            async move {
                upstream_requests.fetch_add(1, Ordering::SeqCst);
                let method = request.method().clone();
                let uri = request.uri().clone();
                let body = to_bytes(request.into_body(), 1024).await.unwrap();
                if uri.path().ends_with("/style.css") {
                    return (
                        [
                            (header::CONTENT_TYPE, "text/css"),
                            (header::LINK, "<https://fonts.googleapis.com/css>; rel=stylesheet"),
                        ],
                        "@import url('https://fonts.googleapis.com/css'); src:url(//fonts.gstatic.com/font.woff2);".to_owned(),
                    );
                }
                (
                    [
                        (header::CONTENT_TYPE, "text/html"),
                        (header::LINK, "<https://fonts.googleapis.com/css>; rel=stylesheet"),
                    ],
                    format!(
                        "{method} {uri} {} <a href=\"{body_origin}/asset\">asset</a> <style>@import url('https://fonts.googleapis.com/css'); src:url(//fonts.gstatic.com/font.woff2);</style>",
                        String::from_utf8(body.to_vec()).unwrap()
                    ),
                )
            }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream_app).await.unwrap();
        });
        for (shadow_domain, expected_origin) in [
            (None, "http://mirror.example"),
            (
                Some(Url::parse("https://shadow.example/").unwrap()),
                "https://shadow.example",
            ),
        ] {
            for google_fail in [
                None,
                Some(GoogleFail::Forbidden),
                Some(GoogleFail::NotFound),
                Some(GoogleFail::InternalServerError),
            ] {
                let state = AppState {
                    client: reqwest::Client::builder()
                        .redirect(Policy::none())
                        .build()
                        .unwrap(),
                    upstream: Url::parse(&format!("{upstream_origin}/base/")).unwrap(),
                    shadow_domain: shadow_domain.clone(),
                    google_fail,
                    debug: false,
                };
                let router = app(state);
                let request = Request::builder()
                    .method("POST")
                    .uri("/nested?q=1")
                    .header(header::HOST, "mirror.example")
                    .body(Body::from("payload"))
                    .unwrap();

                let response = router.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let google_origin = if google_fail.is_some() {
                    "http://mirror.example/__mirror_response"
                } else {
                    "https://fonts.googleapis.com"
                };
                let font_origin = if google_fail.is_some() {
                    google_origin
                } else {
                    "//fonts.gstatic.com"
                };
                assert_eq!(
                    response.headers()[header::LINK],
                    format!("<{google_origin}/css>; rel=stylesheet")
                );
                assert_eq!(
                    to_bytes(response.into_body(), 4096).await.unwrap(),
                    format!(
                        "POST /base/nested?q=1 payload <a href=\"{expected_origin}/asset\">asset</a> <style>@import url('{google_origin}/css'); src:url({font_origin}/font.woff2);</style>"
                    )
                );
                let before = requests.load(Ordering::SeqCst);
                let stylesheet = router.clone().oneshot(
                    Request::builder()
                        .uri("/style.css")
                        .header(header::HOST, "mirror.example")
                        .body(Body::empty())
                        .unwrap()
                ).await.unwrap();
                assert_eq!(stylesheet.status(), StatusCode::OK);
                assert_eq!(stylesheet.headers()[header::CONTENT_TYPE], "text/css");
                assert_eq!(
                    to_bytes(stylesheet.into_body(), 1024).await.unwrap(),
                    format!("@import url('{google_origin}/css'); src:url({font_origin}/font.woff2);")
                );
                assert_eq!(requests.load(Ordering::SeqCst), before + 1);
                let before = requests.load(Ordering::SeqCst);
                for path in [
                    GOOGLE_FAIL_PATH,
                    "/__mirror_response/css?family=Roboto",
                    "/__mirror_response/font.woff2",
                    "/__mirror_response_other",
                ] {
                    let response = router.clone().oneshot(
                        Request::builder()
                            .uri(path)
                            .header(header::HOST, "mirror.example")
                            .body(Body::empty())
                            .unwrap()
                    ).await.unwrap();
                    if let Some(failure) = google_fail.filter(|_| path != "/__mirror_response_other") {
                        let (status, message) = failure.response();
                        assert_eq!(response.status(), status);
                        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain; charset=utf-8");
                        assert!(response.headers().iter().all(|(name, value)| {
                            !name.as_str().contains("google")
                                && !name.as_str().contains("block")
                                && !value.to_str().unwrap().contains("google")
                                && !value.to_str().unwrap().contains("block")
                        }));
                        assert_eq!(to_bytes(response.into_body(), 1024).await.unwrap(), message);
                        assert_eq!(requests.load(Ordering::SeqCst), before);
                    } else {
                        assert_eq!(response.status(), StatusCode::OK);
                    }
                }
            }
        }
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(&upstream_origin).unwrap())
            .build()
            .unwrap();
        let before = requests.load(Ordering::SeqCst);
        for host in ["googleapis.com", "fonts.googleapis.com", "fonts.gstatic.com"] {
            for failure in [GoogleFail::Forbidden, GoogleFail::NotFound, GoogleFail::InternalServerError] {
                let router = app(AppState {
                    client: client.clone(),
                    upstream: Url::parse(&format!("http://{host}/")).unwrap(),
                    shadow_domain: None,
                    google_fail: Some(failure),
                    debug: false,
                });
                for method in ["POST", "HEAD"] {
                    let response = router.clone().oneshot(
                        Request::builder()
                            .method(method)
                            .uri("/css?family=Roboto")
                            .header(header::HOST, "mirror.example")
                            .body(Body::from("payload"))
                            .unwrap()
                    ).await.unwrap();
                    let (status, message) = failure.response();
                    assert_eq!(response.status(), status);
                    let body = to_bytes(response.into_body(), 1024).await.unwrap();
                    assert_eq!(body, if method == "HEAD" { "" } else { message });
                    assert_eq!(requests.load(Ordering::SeqCst), before);
                }
            }
        }
        server.abort();
    }
}
