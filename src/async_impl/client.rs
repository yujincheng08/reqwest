#[cfg(any(feature = "native-tls", feature = "__rustls",))]
use std::any::Any;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use std::{collections::HashMap, convert::TryInto, net::SocketAddr};
use std::{fmt, str};

use bytes::Bytes;
use http::header::{
    Entry, HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_TYPE, LOCATION, PROXY_AUTHORIZATION, RANGE, REFERER, TRANSFER_ENCODING, USER_AGENT,
};
use http::uri::Scheme;
use http::Uri;
use hyper_util::client::legacy::connect::HttpConnector;
#[cfg(feature = "native-tls-crate")]
use native_tls_crate::TlsConnector;
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::time::Sleep;

use super::decoder::Accepts;
use super::request::{Request, RequestBuilder};
use super::response::Response;
use super::Body;
#[cfg(feature = "http3")]
use crate::async_impl::h3_client::connect::H3Connector;
#[cfg(feature = "http3")]
use crate::async_impl::h3_client::{H3Client, H3ResponseFuture};
use crate::connect::Connector;
#[cfg(feature = "cookies")]
use crate::cookie;
#[cfg(feature = "trust-dns")]
use crate::dns::trust_dns::TrustDnsResolver;
use crate::dns::{gai::GaiResolver, DnsResolverWithOverrides, DynResolver, Resolve};
use crate::error;
use crate::into_url::{expect_uri, try_uri};
use crate::redirect::{self, remove_sensitive_headers};
#[cfg(feature = "__tls")]
use crate::tls::{self, TlsBackend};
#[cfg(feature = "__tls")]
use crate::Certificate;
#[cfg(any(feature = "native-tls", feature = "__rustls"))]
use crate::Identity;
use crate::{IntoUrl, Method, Proxy, StatusCode, Url};
use log::{debug, trace};
#[cfg(feature = "http3")]
use quinn::TransportConfig;
#[cfg(feature = "http3")]
use quinn::VarInt;

type HyperResponseFuture = hyper_util::client::legacy::ResponseFuture;

/// An asynchronous `Client` to make Requests with.
///
/// The Client has various configuration values to tweak, but the defaults
/// are set to what is usually the most commonly desired value. To configure a
/// `Client`, use `Client::builder()`.
///
/// The `Client` holds a connection pool internally, so it is advised that
/// you create one and **reuse** it.
///
/// You do **not** have to wrap the `Client` in an [`Rc`] or [`Arc`] to **reuse** it,
/// because it already uses an [`Arc`] internally.
///
/// [`Rc`]: std::rc::Rc
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientRef>,
}

/// A `ClientBuilder` can be used to create a `Client` with custom configuration.
#[must_use]
pub struct ClientBuilder {
    config: Config,
}

enum HttpVersionPref {
    Http1,
    Http2,
    #[cfg(feature = "http3")]
    Http3,
    All,
}

struct Config {
    // NOTE: When adding a new field, update `fmt::Debug for ClientBuilder`
    accepts: Accepts,
    headers: HeaderMap,
    #[cfg(feature = "native-tls")]
    hostname_verification: bool,
    #[cfg(feature = "__tls")]
    certs_verification: bool,
    #[cfg(feature = "__tls")]
    tls_sni: bool,
    connect_timeout: Option<Duration>,
    connection_verbose: bool,
    pool_idle_timeout: Option<Duration>,
    pool_max_idle_per_host: usize,
    tcp_keepalive: Option<Duration>,
    #[cfg(any(feature = "native-tls", feature = "__rustls"))]
    identity: Option<Identity>,
    proxies: Vec<Proxy>,
    auto_sys_proxy: bool,
    redirect_policy: redirect::Policy,
    referer: bool,
    timeout: Option<Duration>,
    #[cfg(feature = "__tls")]
    root_certs: Vec<Certificate>,
    #[cfg(feature = "__tls")]
    tls_built_in_root_certs: bool,
    #[cfg(feature = "__tls")]
    min_tls_version: Option<tls::Version>,
    #[cfg(feature = "__tls")]
    max_tls_version: Option<tls::Version>,
    #[cfg(feature = "__tls")]
    tls_info: bool,
    #[cfg(feature = "__tls")]
    tls: TlsBackend,
    http_version_pref: HttpVersionPref,
    http09_responses: bool,
    http1_title_case_headers: bool,
    http1_allow_obsolete_multiline_headers_in_responses: bool,
    http1_ignore_invalid_headers_in_responses: bool,
    http1_allow_spaces_after_header_name_in_responses: bool,
    http2_initial_stream_window_size: Option<u32>,
    http2_initial_connection_window_size: Option<u32>,
    http2_adaptive_window: bool,
    http2_max_frame_size: Option<u32>,
    http2_keep_alive_interval: Option<Duration>,
    http2_keep_alive_timeout: Option<Duration>,
    http2_keep_alive_while_idle: bool,
    local_address: Option<IpAddr>,
    interface: Option<String>,
    nodelay: bool,
    #[cfg(feature = "cookies")]
    cookie_store: Option<Arc<dyn cookie::CookieStore>>,
    trust_dns: bool,
    error: Option<crate::Error>,
    https_only: bool,
    #[cfg(feature = "http3")]
    tls_enable_early_data: bool,
    #[cfg(feature = "http3")]
    quic_max_idle_timeout: Option<Duration>,
    #[cfg(feature = "http3")]
    quic_stream_receive_window: Option<VarInt>,
    #[cfg(feature = "http3")]
    quic_receive_window: Option<VarInt>,
    #[cfg(feature = "http3")]
    quic_send_window: Option<u64>,
    dns_overrides: HashMap<String, Vec<SocketAddr>>,
    dns_resolver: Option<Arc<dyn Resolve>>,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientBuilder {
    /// Constructs a new `ClientBuilder`.
    ///
    /// This is the same as `Client::builder()`.
    pub fn new() -> ClientBuilder {
        let mut headers: HeaderMap<HeaderValue> = HeaderMap::with_capacity(2);
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));

        ClientBuilder {
            config: Config {
                error: None,
                accepts: Accepts::default(),
                headers,
                #[cfg(feature = "native-tls")]
                hostname_verification: true,
                #[cfg(feature = "__tls")]
                certs_verification: true,
                #[cfg(feature = "__tls")]
                tls_sni: true,
                connect_timeout: None,
                connection_verbose: false,
                pool_idle_timeout: Some(Duration::from_secs(90)),
                pool_max_idle_per_host: std::usize::MAX,
                // TODO: Re-enable default duration once hyper's HttpConnector is fixed
                // to no longer error when an option fails.
                tcp_keepalive: None, //Some(Duration::from_secs(60)),
                proxies: Vec::new(),
                auto_sys_proxy: true,
                redirect_policy: redirect::Policy::default(),
                referer: true,
                timeout: None,
                #[cfg(feature = "__tls")]
                root_certs: Vec::new(),
                #[cfg(feature = "__tls")]
                tls_built_in_root_certs: true,
                #[cfg(any(feature = "native-tls", feature = "__rustls"))]
                identity: None,
                #[cfg(feature = "__tls")]
                min_tls_version: None,
                #[cfg(feature = "__tls")]
                max_tls_version: None,
                #[cfg(feature = "__tls")]
                tls_info: false,
                #[cfg(feature = "__tls")]
                tls: TlsBackend::default(),
                http_version_pref: HttpVersionPref::All,
                http09_responses: false,
                http1_title_case_headers: false,
                http1_allow_obsolete_multiline_headers_in_responses: false,
                http1_ignore_invalid_headers_in_responses: false,
                http1_allow_spaces_after_header_name_in_responses: false,
                http2_initial_stream_window_size: None,
                http2_initial_connection_window_size: None,
                http2_adaptive_window: false,
                http2_max_frame_size: None,
                http2_keep_alive_interval: None,
                http2_keep_alive_timeout: None,
                http2_keep_alive_while_idle: false,
                local_address: None,
                interface: None,
                nodelay: true,
                trust_dns: cfg!(feature = "trust-dns"),
                #[cfg(feature = "cookies")]
                cookie_store: None,
                https_only: false,
                dns_overrides: HashMap::new(),
                #[cfg(feature = "http3")]
                tls_enable_early_data: false,
                #[cfg(feature = "http3")]
                quic_max_idle_timeout: None,
                #[cfg(feature = "http3")]
                quic_stream_receive_window: None,
                #[cfg(feature = "http3")]
                quic_receive_window: None,
                #[cfg(feature = "http3")]
                quic_send_window: None,
                dns_resolver: None,
            },
        }
    }

    /// Returns a `Client` that uses this `ClientBuilder` configuration.
    ///
    /// # Errors
    ///
    /// This method fails if a TLS backend cannot be initialized, or the resolver
    /// cannot load the system configuration.
    pub fn build(self) -> crate::Result<Client> {
        let config = self.config;

        if let Some(err) = config.error {
            return Err(err);
        }

        let mut proxies = config.proxies;
        if config.auto_sys_proxy {
            proxies.push(Proxy::system());
        }
        let proxies = Arc::new(proxies);

        #[allow(unused)]
        #[cfg(feature = "http3")]
        let mut h3_connector = None;

        let mut connector = {
            #[cfg(feature = "__tls")]
            fn user_agent(headers: &HeaderMap) -> Option<HeaderValue> {
                headers.get(USER_AGENT).cloned()
            }

            let mut resolver: Arc<dyn Resolve> = match config.trust_dns {
                false => Arc::new(GaiResolver::new()),
                #[cfg(feature = "trust-dns")]
                true => Arc::new(TrustDnsResolver::default()),
                #[cfg(not(feature = "trust-dns"))]
                true => unreachable!("trust-dns shouldn't be enabled unless the feature is"),
            };
            if let Some(dns_resolver) = config.dns_resolver {
                resolver = dns_resolver;
            }
            if !config.dns_overrides.is_empty() {
                resolver = Arc::new(DnsResolverWithOverrides::new(
                    resolver,
                    config.dns_overrides,
                ));
            }
            let mut http = HttpConnector::new_with_resolver(DynResolver::new(resolver.clone()));
            http.set_connect_timeout(config.connect_timeout);

            #[cfg(all(feature = "http3", feature = "__rustls"))]
            let build_h3_connector =
                |resolver,
                 tls,
                 quic_max_idle_timeout: Option<Duration>,
                 quic_stream_receive_window,
                 quic_receive_window,
                 quic_send_window,
                 local_address,
                 http_version_pref: &HttpVersionPref| {
                    let mut transport_config = TransportConfig::default();

                    if let Some(max_idle_timeout) = quic_max_idle_timeout {
                        transport_config.max_idle_timeout(Some(
                            max_idle_timeout.try_into().map_err(error::builder)?,
                        ));
                    }

                    if let Some(stream_receive_window) = quic_stream_receive_window {
                        transport_config.stream_receive_window(stream_receive_window);
                    }

                    if let Some(receive_window) = quic_receive_window {
                        transport_config.receive_window(receive_window);
                    }

                    if let Some(send_window) = quic_send_window {
                        transport_config.send_window(send_window);
                    }

                    let res = H3Connector::new(
                        DynResolver::new(resolver),
                        tls,
                        local_address,
                        transport_config,
                    );

                    match res {
                        Ok(connector) => Ok(Some(connector)),
                        Err(err) => {
                            if let HttpVersionPref::Http3 = http_version_pref {
                                Err(error::builder(err))
                            } else {
                                Ok(None)
                            }
                        }
                    }
                };

            #[cfg(feature = "__tls")]
            match config.tls {
                #[cfg(feature = "default-tls")]
                TlsBackend::Default => {
                    let mut tls = TlsConnector::builder();

                    #[cfg(all(feature = "native-tls-alpn", not(feature = "http3")))]
                    {
                        match config.http_version_pref {
                            HttpVersionPref::Http1 => {
                                tls.request_alpns(&["http/1.1"]);
                            }
                            HttpVersionPref::Http2 => {
                                tls.request_alpns(&["h2"]);
                            }
                            HttpVersionPref::All => {
                                tls.request_alpns(&["h2", "http/1.1"]);
                            }
                        }
                    }

                    #[cfg(feature = "native-tls")]
                    {
                        tls.danger_accept_invalid_hostnames(!config.hostname_verification);
                    }

                    tls.danger_accept_invalid_certs(!config.certs_verification);

                    tls.use_sni(config.tls_sni);

                    tls.disable_built_in_roots(!config.tls_built_in_root_certs);

                    for cert in config.root_certs {
                        cert.add_to_native_tls(&mut tls);
                    }

                    #[cfg(feature = "native-tls")]
                    {
                        if let Some(id) = config.identity {
                            id.add_to_native_tls(&mut tls)?;
                        }
                    }
                    #[cfg(all(feature = "__rustls", not(feature = "native-tls")))]
                    {
                        // Default backend + rustls Identity doesn't work.
                        if let Some(_id) = config.identity {
                            return Err(crate::error::builder("incompatible TLS identity type"));
                        }
                    }

                    if let Some(min_tls_version) = config.min_tls_version {
                        let protocol = min_tls_version.to_native_tls().ok_or_else(|| {
                            // TLS v1.3. This would be entirely reasonable,
                            // native-tls just doesn't support it.
                            // https://github.com/sfackler/rust-native-tls/issues/140
                            crate::error::builder("invalid minimum TLS version for backend")
                        })?;
                        tls.min_protocol_version(Some(protocol));
                    }

                    if let Some(max_tls_version) = config.max_tls_version {
                        let protocol = max_tls_version.to_native_tls().ok_or_else(|| {
                            // TLS v1.3.
                            // We could arguably do max_protocol_version(None), given
                            // that 1.4 does not exist yet, but that'd get messy in the
                            // future.
                            crate::error::builder("invalid maximum TLS version for backend")
                        })?;
                        tls.max_protocol_version(Some(protocol));
                    }

                    Connector::new_default_tls(
                        http,
                        tls,
                        proxies.clone(),
                        user_agent(&config.headers),
                        config.local_address,
                        config.interface.as_deref(),
                        config.nodelay,
                        config.tls_info,
                    )?
                }
                #[cfg(feature = "native-tls")]
                TlsBackend::BuiltNativeTls(conn) => Connector::from_built_default_tls(
                    http,
                    conn,
                    proxies.clone(),
                    user_agent(&config.headers),
                    config.local_address,
                    config.interface.as_deref(),
                    config.nodelay,
                    config.tls_info,
                ),
                #[cfg(feature = "__rustls")]
                TlsBackend::BuiltRustls(conn) => {
                    #[cfg(feature = "http3")]
                    {
                        h3_connector = build_h3_connector(
                            resolver,
                            conn.clone(),
                            config.quic_max_idle_timeout,
                            config.quic_stream_receive_window,
                            config.quic_receive_window,
                            config.quic_send_window,
                            config.local_address,
                            config.interface.as_deref(),
                            &config.http_version_pref,
                        )?;
                    }

                    Connector::new_rustls_tls(
                        http,
                        conn,
                        proxies.clone(),
                        user_agent(&config.headers),
                        config.local_address,
                        config.interface.as_deref(),
                        config.nodelay,
                        config.tls_info,
                    )
                }
                #[cfg(feature = "__rustls")]
                TlsBackend::Rustls => {
                    use crate::tls::NoVerifier;

                    // Set root certificates.
                    let mut root_cert_store = rustls::RootCertStore::empty();
                    for cert in config.root_certs {
                        cert.add_to_rustls(&mut root_cert_store)?;
                    }

                    #[cfg(feature = "rustls-tls-webpki-roots")]
                    if config.tls_built_in_root_certs {
                        root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                    }

                    #[cfg(feature = "rustls-tls-native-roots")]
                    if config.tls_built_in_root_certs {
                        let mut valid_count = 0;
                        let mut invalid_count = 0;
                        for cert in rustls_native_certs::load_native_certs()
                            .map_err(crate::error::builder)?
                        {
                            // Continue on parsing errors, as native stores often include ancient or syntactically
                            // invalid certificates, like root certificates without any X509 extensions.
                            // Inspiration: https://github.com/rustls/rustls/blob/633bf4ba9d9521a95f68766d04c22e2b01e68318/rustls/src/anchors.rs#L105-L112
                            match root_cert_store.add(cert.into()) {
                                Ok(_) => valid_count += 1,
                                Err(err) => {
                                    invalid_count += 1;
                                    log::warn!(
                                        "rustls failed to parse DER certificate {:?} {:?}",
                                        &err,
                                        &cert
                                    );
                                }
                            }
                        }
                        if valid_count == 0 && invalid_count > 0 {
                            return Err(crate::error::builder(
                                "zero valid certificates found in native root store",
                            ));
                        }
                    }

                    // Set TLS versions.
                    let mut versions = rustls::ALL_VERSIONS.to_vec();

                    if let Some(min_tls_version) = config.min_tls_version {
                        versions.retain(|&supported_version| {
                            match tls::Version::from_rustls(supported_version.version) {
                                Some(version) => version >= min_tls_version,
                                // Assume it's so new we don't know about it, allow it
                                // (as of writing this is unreachable)
                                None => true,
                            }
                        });
                    }

                    if let Some(max_tls_version) = config.max_tls_version {
                        versions.retain(|&supported_version| {
                            match tls::Version::from_rustls(supported_version.version) {
                                Some(version) => version <= max_tls_version,
                                None => false,
                            }
                        });
                    }

                    // Build TLS config
                    let config_builder =
                        rustls::ClientConfig::builder().with_root_certificates(root_cert_store);

                    // Finalize TLS config
                    let mut tls = if let Some(id) = config.identity {
                        id.add_to_rustls(config_builder)?
                    } else {
                        config_builder.with_no_client_auth()
                    };

                    // Certificate verifier
                    if !config.certs_verification {
                        tls.dangerous()
                            .set_certificate_verifier(Arc::new(NoVerifier));
                    }

                    tls.enable_sni = config.tls_sni;

                    // ALPN protocol
                    match config.http_version_pref {
                        HttpVersionPref::Http1 => {
                            tls.alpn_protocols = vec!["http/1.1".into()];
                        }
                        HttpVersionPref::Http2 => {
                            tls.alpn_protocols = vec!["h2".into()];
                        }
                        #[cfg(feature = "http3")]
                        HttpVersionPref::Http3 => {
                            tls.alpn_protocols = vec!["h3".into()];
                        }
                        HttpVersionPref::All => {
                            tls.alpn_protocols = vec!["h2".into(), "http/1.1".into()];
                        }
                    }

                    #[cfg(feature = "http3")]
                    {
                        tls.enable_early_data = config.tls_enable_early_data;

                        h3_connector = build_h3_connector(
                            resolver,
                            tls.clone(),
                            config.quic_max_idle_timeout,
                            config.quic_stream_receive_window,
                            config.quic_receive_window,
                            config.quic_send_window,
                            config.local_address,
                            &config.http_version_pref,
                        )?;
                    }

                    Connector::new_rustls_tls(
                        http,
                        tls,
                        proxies.clone(),
                        user_agent(&config.headers),
                        config.local_address,
                        config.interface.as_deref(),
                        config.nodelay,
                        config.tls_info,
                    )
                }
                #[cfg(any(feature = "native-tls", feature = "__rustls",))]
                TlsBackend::UnknownPreconfigured => {
                    return Err(crate::error::builder(
                        "Unknown TLS backend passed to `use_preconfigured_tls`",
                    ));
                }
            }

            #[cfg(not(feature = "__tls"))]
            Connector::new(http, proxies.clone(), config.local_address, config.nodelay)
        };

        connector.set_timeout(config.connect_timeout);
        connector.set_verbose(config.connection_verbose);

        let mut builder =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new());
        if matches!(config.http_version_pref, HttpVersionPref::Http2) {
            builder.http2_only(true);
        }

        if let Some(http2_initial_stream_window_size) = config.http2_initial_stream_window_size {
            builder.http2_initial_stream_window_size(http2_initial_stream_window_size);
        }
        if let Some(http2_initial_connection_window_size) =
            config.http2_initial_connection_window_size
        {
            builder.http2_initial_connection_window_size(http2_initial_connection_window_size);
        }
        if config.http2_adaptive_window {
            builder.http2_adaptive_window(true);
        }
        if let Some(http2_max_frame_size) = config.http2_max_frame_size {
            builder.http2_max_frame_size(http2_max_frame_size);
        }
        if let Some(http2_keep_alive_interval) = config.http2_keep_alive_interval {
            builder.http2_keep_alive_interval(http2_keep_alive_interval);
        }
        if let Some(http2_keep_alive_timeout) = config.http2_keep_alive_timeout {
            builder.http2_keep_alive_timeout(http2_keep_alive_timeout);
        }
        if config.http2_keep_alive_while_idle {
            builder.http2_keep_alive_while_idle(true);
        }

        builder.pool_idle_timeout(config.pool_idle_timeout);
        builder.pool_max_idle_per_host(config.pool_max_idle_per_host);
        connector.set_keepalive(config.tcp_keepalive);

        if config.http09_responses {
            builder.http09_responses(true);
        }

        if config.http1_title_case_headers {
            builder.http1_title_case_headers(true);
        }

        if config.http1_allow_obsolete_multiline_headers_in_responses {
            builder.http1_allow_obsolete_multiline_headers_in_responses(true);
        }

        if config.http1_ignore_invalid_headers_in_responses {
            builder.http1_ignore_invalid_headers_in_responses(true);
        }

        if config.http1_allow_spaces_after_header_name_in_responses {
            builder.http1_allow_spaces_after_header_name_in_responses(true);
        }

        let proxies_maybe_http_auth = proxies.iter().any(|p| p.maybe_has_http_auth());

        Ok(Client {
            inner: Arc::new(ClientRef {
                accepts: config.accepts,
                #[cfg(feature = "cookies")]
                cookie_store: config.cookie_store,
                // Use match instead of map since config is partially moved
                // and it cannot be used in closure
                #[cfg(feature = "http3")]
                h3_client: match h3_connector {
                    Some(h3_connector) => {
                        Some(H3Client::new(h3_connector, config.pool_idle_timeout))
                    }
                    None => None,
                },
                hyper: builder.build(connector),
                headers: config.headers,
                redirect_policy: config.redirect_policy,
                referer: config.referer,
                request_timeout: config.timeout,
                proxies,
                proxies_maybe_http_auth,
                https_only: config.https_only,
            }),
        })
    }

    // Higher-level options

    /// Sets the `User-Agent` header to be used by this client.
    ///
    /// # Example
    ///
    /// ```rust
    /// # async fn doc() -> Result<(), reqwest::Error> {
    /// // Name your user agent after your app?
    /// static APP_USER_AGENT: &str = concat!(
    ///     env!("CARGO_PKG_NAME"),
    ///     "/",
    ///     env!("CARGO_PKG_VERSION"),
    /// );
    ///
    /// let client = reqwest::Client::builder()
    ///     .user_agent(APP_USER_AGENT)
    ///     .build()?;
    /// let res = client.get("https://www.rust-lang.org").send().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn user_agent<V>(mut self, value: V) -> ClientBuilder
    where
        V: TryInto<HeaderValue>,
        V::Error: Into<http::Error>,
    {
        match value.try_into() {
            Ok(value) => {
                self.config.headers.insert(USER_AGENT, value);
            }
            Err(e) => {
                self.config.error = Some(crate::error::builder(e.into()));
            }
        };
        self
    }
    /// Sets the default headers for every request.
    ///
    /// # Example
    ///
    /// ```rust
    /// use reqwest::header;
    /// # async fn doc() -> Result<(), reqwest::Error> {
    /// let mut headers = header::HeaderMap::new();
    /// headers.insert("X-MY-HEADER", header::HeaderValue::from_static("value"));
    ///
    /// // Consider marking security-sensitive headers with `set_sensitive`.
    /// let mut auth_value = header::HeaderValue::from_static("secret");
    /// auth_value.set_sensitive(true);
    /// headers.insert(header::AUTHORIZATION, auth_value);
    ///
    /// // get a client builder
    /// let client = reqwest::Client::builder()
    ///     .default_headers(headers)
    ///     .build()?;
    /// let res = client.get("https://www.rust-lang.org").send().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Override the default headers:
    ///
    /// ```rust
    /// use reqwest::header;
    /// # async fn doc() -> Result<(), reqwest::Error> {
    /// let mut headers = header::HeaderMap::new();
    /// headers.insert("X-MY-HEADER", header::HeaderValue::from_static("value"));
    ///
    /// // get a client builder
    /// let client = reqwest::Client::builder()
    ///     .default_headers(headers)
    ///     .build()?;
    /// let res = client
    ///     .get("https://www.rust-lang.org")
    ///     .header("X-MY-HEADER", "new_value")
    ///     .send()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn default_headers(mut self, headers: HeaderMap) -> ClientBuilder {
        for (key, value) in headers.iter() {
            self.config.headers.insert(key, value.clone());
        }
        self
    }

    /// Enable a persistent cookie store for the client.
    ///
    /// Cookies received in responses will be preserved and included in
    /// additional requests.
    ///
    /// By default, no cookie store is used.
    ///
    /// # Optional
    ///
    /// This requires the optional `cookies` feature to be enabled.
    #[cfg(feature = "cookies")]
    #[cfg_attr(docsrs, doc(cfg(feature = "cookies")))]
    pub fn cookie_store(mut self, enable: bool) -> ClientBuilder {
        if enable {
            self.cookie_provider(Arc::new(cookie::Jar::default()))
        } else {
            self.config.cookie_store = None;
            self
        }
    }

    /// Set the persistent cookie store for the client.
    ///
    /// Cookies received in responses will be passed to this store, and
    /// additional requests will query this store for cookies.
    ///
    /// By default, no cookie store is used.
    ///
    /// # Optional
    ///
    /// This requires the optional `cookies` feature to be enabled.
    #[cfg(feature = "cookies")]
    #[cfg_attr(docsrs, doc(cfg(feature = "cookies")))]
    pub fn cookie_provider<C: cookie::CookieStore + 'static>(
        mut self,
        cookie_store: Arc<C>,
    ) -> ClientBuilder {
        self.config.cookie_store = Some(cookie_store as _);
        self
    }

    /// Enable auto gzip decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto gzip decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain
    ///   an `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to `gzip`.
    ///   The request body is **not** automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding` value of
    ///   `gzip`, both `Content-Encoding` and `Content-Length` are removed from the
    ///   headers' set. The response body is automatically decompressed.
    ///
    /// If the `gzip` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `gzip` feature to be enabled
    #[cfg(feature = "gzip")]
    #[cfg_attr(docsrs, doc(cfg(feature = "gzip")))]
    pub fn gzip(mut self, enable: bool) -> ClientBuilder {
        self.config.accepts.gzip = enable;
        self
    }

    /// Enable auto brotli decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto brotli decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain
    ///   an `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to `br`.
    ///   The request body is **not** automatically compressed.
    /// - When receiving a response, if its headers contain a `Content-Encoding` value of
    ///   `br`, both `Content-Encoding` and `Content-Length` are removed from the
    ///   headers' set. The response body is automatically decompressed.
    ///
    /// If the `brotli` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `brotli` feature to be enabled
    #[cfg(feature = "brotli")]
    #[cfg_attr(docsrs, doc(cfg(feature = "brotli")))]
    pub fn brotli(mut self, enable: bool) -> ClientBuilder {
        self.config.accepts.brotli = enable;
        self
    }

    /// Enable auto deflate decompression by checking the `Content-Encoding` response header.
    ///
    /// If auto deflate decompression is turned on:
    ///
    /// - When sending a request and if the request's headers do not already contain
    ///   an `Accept-Encoding` **and** `Range` values, the `Accept-Encoding` header is set to `deflate`.
    ///   The request body is **not** automatically compressed.
    /// - When receiving a response, if it's headers contain a `Content-Encoding` value that
    ///   equals to `deflate`, both values `Content-Encoding` and `Content-Length` are removed from the
    ///   headers' set. The response body is automatically decompressed.
    ///
    /// If the `deflate` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `deflate` feature to be enabled
    #[cfg(feature = "deflate")]
    #[cfg_attr(docsrs, doc(cfg(feature = "deflate")))]
    pub fn deflate(mut self, enable: bool) -> ClientBuilder {
        self.config.accepts.deflate = enable;
        self
    }

    /// Disable auto response body gzip decompression.
    ///
    /// This method exists even if the optional `gzip` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use gzip decompression
    /// even if another dependency were to enable the optional `gzip` feature.
    pub fn no_gzip(self) -> ClientBuilder {
        #[cfg(feature = "gzip")]
        {
            self.gzip(false)
        }

        #[cfg(not(feature = "gzip"))]
        {
            self
        }
    }

    /// Disable auto response body brotli decompression.
    ///
    /// This method exists even if the optional `brotli` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use brotli decompression
    /// even if another dependency were to enable the optional `brotli` feature.
    pub fn no_brotli(self) -> ClientBuilder {
        #[cfg(feature = "brotli")]
        {
            self.brotli(false)
        }

        #[cfg(not(feature = "brotli"))]
        {
            self
        }
    }

    /// Disable auto response body deflate decompression.
    ///
    /// This method exists even if the optional `deflate` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use deflate decompression
    /// even if another dependency were to enable the optional `deflate` feature.
    pub fn no_deflate(self) -> ClientBuilder {
        #[cfg(feature = "deflate")]
        {
            self.deflate(false)
        }

        #[cfg(not(feature = "deflate"))]
        {
            self
        }
    }

    // Redirect options

    /// Set a `RedirectPolicy` for this client.
    ///
    /// Default will follow redirects up to a maximum of 10.
    pub fn redirect(mut self, policy: redirect::Policy) -> ClientBuilder {
        self.config.redirect_policy = policy;
        self
    }

    /// Enable or disable automatic setting of the `Referer` header.
    ///
    /// Default is `true`.
    pub fn referer(mut self, enable: bool) -> ClientBuilder {
        self.config.referer = enable;
        self
    }

    // Proxy options

    /// Add a `Proxy` to the list of proxies the `Client` will use.
    ///
    /// # Note
    ///
    /// Adding a proxy will disable the automatic usage of the "system" proxy.
    pub fn proxy(mut self, proxy: Proxy) -> ClientBuilder {
        self.config.proxies.push(proxy);
        self.config.auto_sys_proxy = false;
        self
    }

    /// Clear all `Proxies`, so `Client` will use no proxy anymore.
    ///
    /// # Note
    /// To add a proxy exclusion list, use [crate::proxy::Proxy::no_proxy()]
    /// on all desired proxies instead.
    ///
    /// This also disables the automatic usage of the "system" proxy.
    pub fn no_proxy(mut self) -> ClientBuilder {
        self.config.proxies.clear();
        self.config.auto_sys_proxy = false;
        self
    }

    // Timeout options

    /// Enables a request timeout.
    ///
    /// The timeout is applied from when the request starts connecting until the
    /// response body has finished.
    ///
    /// Default is no timeout.
    pub fn timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.timeout = Some(timeout);
        self
    }

    /// Set a timeout for only the connect phase of a `Client`.
    ///
    /// Default is `None`.
    ///
    /// # Note
    ///
    /// This **requires** the futures be executed in a tokio runtime with
    /// a tokio timer enabled.
    pub fn connect_timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.connect_timeout = Some(timeout);
        self
    }

    /// Set whether connections should emit verbose logs.
    ///
    /// Enabling this option will emit [log][] messages at the `TRACE` level
    /// for read and write operations on connections.
    ///
    /// [log]: https://crates.io/crates/log
    pub fn connection_verbose(mut self, verbose: bool) -> ClientBuilder {
        self.config.connection_verbose = verbose;
        self
    }

    // HTTP options

    /// Set an optional timeout for idle sockets being kept-alive.
    ///
    /// Pass `None` to disable timeout.
    ///
    /// Default is 90 seconds.
    pub fn pool_idle_timeout<D>(mut self, val: D) -> ClientBuilder
    where
        D: Into<Option<Duration>>,
    {
        self.config.pool_idle_timeout = val.into();
        self
    }

    /// Sets the maximum idle connection per host allowed in the pool.
    pub fn pool_max_idle_per_host(mut self, max: usize) -> ClientBuilder {
        self.config.pool_max_idle_per_host = max;
        self
    }

    /// Send headers as title case instead of lowercase.
    pub fn http1_title_case_headers(mut self) -> ClientBuilder {
        self.config.http1_title_case_headers = true;
        self
    }

    /// Set whether HTTP/1 connections will accept obsolete line folding for
    /// header values.
    ///
    /// Newline codepoints (`\r` and `\n`) will be transformed to spaces when
    /// parsing.
    pub fn http1_allow_obsolete_multiline_headers_in_responses(
        mut self,
        value: bool,
    ) -> ClientBuilder {
        self.config
            .http1_allow_obsolete_multiline_headers_in_responses = value;
        self
    }

    /// Sets whether invalid header lines should be silently ignored in HTTP/1 responses.
    pub fn http1_ignore_invalid_headers_in_responses(mut self, value: bool) -> ClientBuilder {
        self.config.http1_ignore_invalid_headers_in_responses = value;
        self
    }

    /// Set whether HTTP/1 connections will accept spaces between header
    /// names and the colon that follow them in responses.
    ///
    /// Newline codepoints (`\r` and `\n`) will be transformed to spaces when
    /// parsing.
    pub fn http1_allow_spaces_after_header_name_in_responses(
        mut self,
        value: bool,
    ) -> ClientBuilder {
        self.config
            .http1_allow_spaces_after_header_name_in_responses = value;
        self
    }

    /// Only use HTTP/1.
    pub fn http1_only(mut self) -> ClientBuilder {
        self.config.http_version_pref = HttpVersionPref::Http1;
        self
    }

    /// Allow HTTP/0.9 responses
    pub fn http09_responses(mut self) -> ClientBuilder {
        self.config.http09_responses = true;
        self
    }

    /// Only use HTTP/2.
    pub fn http2_prior_knowledge(mut self) -> ClientBuilder {
        self.config.http_version_pref = HttpVersionPref::Http2;
        self
    }

    /// Only use HTTP/3.
    #[cfg(feature = "http3")]
    #[cfg_attr(docsrs, doc(cfg(all(reqwest_unstable, feature = "http3",))))]
    pub fn http3_prior_knowledge(mut self) -> ClientBuilder {
        self.config.http_version_pref = HttpVersionPref::Http3;
        self
    }

    /// Sets the `SETTINGS_INITIAL_WINDOW_SIZE` option for HTTP2 stream-level flow control.
    ///
    /// Default is currently 65,535 but may change internally to optimize for common uses.
    pub fn http2_initial_stream_window_size(mut self, sz: impl Into<Option<u32>>) -> ClientBuilder {
        self.config.http2_initial_stream_window_size = sz.into();
        self
    }

    /// Sets the max connection-level flow control for HTTP2
    ///
    /// Default is currently 65,535 but may change internally to optimize for common uses.
    pub fn http2_initial_connection_window_size(
        mut self,
        sz: impl Into<Option<u32>>,
    ) -> ClientBuilder {
        self.config.http2_initial_connection_window_size = sz.into();
        self
    }

    /// Sets whether to use an adaptive flow control.
    ///
    /// Enabling this will override the limits set in `http2_initial_stream_window_size` and
    /// `http2_initial_connection_window_size`.
    pub fn http2_adaptive_window(mut self, enabled: bool) -> ClientBuilder {
        self.config.http2_adaptive_window = enabled;
        self
    }

    /// Sets the maximum frame size to use for HTTP2.
    ///
    /// Default is currently 16,384 but may change internally to optimize for common uses.
    pub fn http2_max_frame_size(mut self, sz: impl Into<Option<u32>>) -> ClientBuilder {
        self.config.http2_max_frame_size = sz.into();
        self
    }

    /// Sets an interval for HTTP2 Ping frames should be sent to keep a connection alive.
    ///
    /// Pass `None` to disable HTTP2 keep-alive.
    /// Default is currently disabled.
    pub fn http2_keep_alive_interval(
        mut self,
        interval: impl Into<Option<Duration>>,
    ) -> ClientBuilder {
        self.config.http2_keep_alive_interval = interval.into();
        self
    }

    /// Sets a timeout for receiving an acknowledgement of the keep-alive ping.
    ///
    /// If the ping is not acknowledged within the timeout, the connection will be closed.
    /// Does nothing if `http2_keep_alive_interval` is disabled.
    /// Default is currently disabled.
    pub fn http2_keep_alive_timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.config.http2_keep_alive_timeout = Some(timeout);
        self
    }

    /// Sets whether HTTP2 keep-alive should apply while the connection is idle.
    ///
    /// If disabled, keep-alive pings are only sent while there are open request/responses streams.
    /// If enabled, pings are also sent when no streams are active.
    /// Does nothing if `http2_keep_alive_interval` is disabled.
    /// Default is `false`.
    pub fn http2_keep_alive_while_idle(mut self, enabled: bool) -> ClientBuilder {
        self.config.http2_keep_alive_while_idle = enabled;
        self
    }

    // TCP options

    /// Set whether sockets have `TCP_NODELAY` enabled.
    ///
    /// Default is `true`.
    pub fn tcp_nodelay(mut self, enabled: bool) -> ClientBuilder {
        self.config.nodelay = enabled;
        self
    }

    /// Bind to a local IP Address.
    ///
    /// # Example
    ///
    /// ```
    /// use std::net::IpAddr;
    /// let local_addr = IpAddr::from([12, 4, 1, 8]);
    /// let client = reqwest::Client::builder()
    ///     .local_address(local_addr)
    ///     .build().unwrap();
    /// ```
    pub fn local_address<T>(mut self, addr: T) -> ClientBuilder
    where
        T: Into<Option<IpAddr>>,
    {
        self.config.local_address = addr.into();
        self
    }

    /// Bind to an interface.
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    pub fn interface(mut self, interface: &str) -> ClientBuilder {
        self.config.interface = Some(interface.to_string());
        self
    }

    /// Set that all sockets have `SO_KEEPALIVE` set with the supplied duration.
    ///
    /// If `None`, the option will not be set.
    pub fn tcp_keepalive<D>(mut self, val: D) -> ClientBuilder
    where
        D: Into<Option<Duration>>,
    {
        self.config.tcp_keepalive = val.into();
        self
    }

    // TLS options

    /// Add a custom root certificate.
    ///
    /// This can be used to connect to a server that has a self-signed
    /// certificate for example.
    ///
    /// # Optional
    ///
    /// This requires the optional `default-tls`, `native-tls`, or `rustls-tls(-...)`
    /// feature to be enabled.
    #[cfg(feature = "__tls")]
    #[cfg_attr(
        docsrs,
        doc(cfg(any(
            feature = "default-tls",
            feature = "native-tls",
            feature = "rustls-tls"
        )))
    )]
    pub fn add_root_certificate(mut self, cert: Certificate) -> ClientBuilder {
        self.config.root_certs.push(cert);
        self
    }

    /// Controls the use of built-in/preloaded certificates during certificate validation.
    ///
    /// Defaults to `true` -- built-in system certs will be used.
    ///
    /// # Optional
    ///
    /// This requires the optional `default-tls`, `native-tls`, or `rustls-tls(-...)`
    /// feature to be enabled.
    #[cfg(feature = "__tls")]
    #[cfg_attr(
        docsrs,
        doc(cfg(any(
            feature = "default-tls",
            feature = "native-tls",
            feature = "rustls-tls"
        )))
    )]
    pub fn tls_built_in_root_certs(mut self, tls_built_in_root_certs: bool) -> ClientBuilder {
        self.config.tls_built_in_root_certs = tls_built_in_root_certs;
        self
    }

    /// Sets the identity to be used for client certificate authentication.
    ///
    /// # Optional
    ///
    /// This requires the optional `native-tls` or `rustls-tls(-...)` feature to be
    /// enabled.
    #[cfg(any(feature = "native-tls", feature = "__rustls"))]
    #[cfg_attr(docsrs, doc(cfg(any(feature = "native-tls", feature = "rustls-tls"))))]
    pub fn identity(mut self, identity: Identity) -> ClientBuilder {
        self.config.identity = Some(identity);
        self
    }

    /// Controls the use of hostname verification.
    ///
    /// Defaults to `false`.
    ///
    /// # Warning
    ///
    /// You should think very carefully before you use this method. If
    /// hostname verification is not used, any valid certificate for any
    /// site will be trusted for use from any other. This introduces a
    /// significant vulnerability to man-in-the-middle attacks.
    ///
    /// # Optional
    ///
    /// This requires the optional `native-tls` feature to be enabled.
    #[cfg(feature = "native-tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "native-tls")))]
    pub fn danger_accept_invalid_hostnames(
        mut self,
        accept_invalid_hostname: bool,
    ) -> ClientBuilder {
        self.config.hostname_verification = !accept_invalid_hostname;
        self
    }

    /// Controls the use of certificate validation.
    ///
    /// Defaults to `false`.
    ///
    /// # Warning
    ///
    /// You should think very carefully before using this method. If
    /// invalid certificates are trusted, *any* certificate for *any* site
    /// will be trusted for use. This includes expired certificates. This
    /// introduces significant vulnerabilities, and should only be used
    /// as a last resort.
    ///
    /// # Optional
    ///
    /// This requires the optional `default-tls`, `native-tls`, or `rustls-tls(-...)`
    /// feature to be enabled.
    #[cfg(feature = "__tls")]
    #[cfg_attr(
        docsrs,
        doc(cfg(any(
            feature = "default-tls",
            feature = "native-tls",
            feature = "rustls-tls"
        )))
    )]
    pub fn danger_accept_invalid_certs(mut self, accept_invalid_certs: bool) -> ClientBuilder {
        self.config.certs_verification = !accept_invalid_certs;
        self
    }

    /// Controls the use of TLS server name indication.
    ///
    /// Defaults to `true`.
    ///
    /// # Optional
    ///
    /// This requires the optional `default-tls`, `native-tls`, or `rustls-tls(-...)`
    /// feature to be enabled.
    #[cfg(feature = "__tls")]
    #[cfg_attr(
        docsrs,
        doc(cfg(any(
            feature = "default-tls",
            feature = "native-tls",
            feature = "rustls-tls"
        )))
    )]
    pub fn tls_sni(mut self, tls_sni: bool) -> ClientBuilder {
        self.config.tls_sni = tls_sni;
        self
    }

    /// Set the minimum required TLS version for connections.
    ///
    /// By default the TLS backend's own default is used.
    ///
    /// # Errors
    ///
    /// A value of `tls::Version::TLS_1_3` will cause an error with the
    /// `native-tls`/`default-tls` backend. This does not mean the version
    /// isn't supported, just that it can't be set as a minimum due to
    /// technical limitations.
    ///
    /// # Optional
    ///
    /// This requires the optional `default-tls`, `native-tls`, or `rustls-tls(-...)`
    /// feature to be enabled.
    #[cfg(feature = "__tls")]
    #[cfg_attr(
        docsrs,
        doc(cfg(any(
            feature = "default-tls",
            feature = "native-tls",
            feature = "rustls-tls"
        )))
    )]
    pub fn min_tls_version(mut self, version: tls::Version) -> ClientBuilder {
        self.config.min_tls_version = Some(version);
        self
    }

    /// Set the maximum allowed TLS version for connections.
    ///
    /// By default there's no maximum.
    ///
    /// # Errors
    ///
    /// A value of `tls::Version::TLS_1_3` will cause an error with the
    /// `native-tls`/`default-tls` backend. This does not mean the version
    /// isn't supported, just that it can't be set as a maximum due to
    /// technical limitations.
    ///
    /// # Optional
    ///
    /// This requires the optional `default-tls`, `native-tls`, or `rustls-tls(-...)`
    /// feature to be enabled.
    #[cfg(feature = "__tls")]
    #[cfg_attr(
        docsrs,
        doc(cfg(any(
            feature = "default-tls",
            feature = "native-tls",
            feature = "rustls-tls"
        )))
    )]
    pub fn max_tls_version(mut self, version: tls::Version) -> ClientBuilder {
        self.config.max_tls_version = Some(version);
        self
    }

    /// Force using the native TLS backend.
    ///
    /// Since multiple TLS backends can be optionally enabled, this option will
    /// force the `native-tls` backend to be used for this `Client`.
    ///
    /// # Optional
    ///
    /// This requires the optional `native-tls` feature to be enabled.
    #[cfg(feature = "native-tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "native-tls")))]
    pub fn use_native_tls(mut self) -> ClientBuilder {
        self.config.tls = TlsBackend::Default;
        self
    }

    /// Force using the Rustls TLS backend.
    ///
    /// Since multiple TLS backends can be optionally enabled, this option will
    /// force the `rustls` backend to be used for this `Client`.
    ///
    /// # Optional
    ///
    /// This requires the optional `rustls-tls(-...)` feature to be enabled.
    #[cfg(feature = "__rustls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "rustls-tls")))]
    pub fn use_rustls_tls(mut self) -> ClientBuilder {
        self.config.tls = TlsBackend::Rustls;
        self
    }

    /// Use a preconfigured TLS backend.
    ///
    /// If the passed `Any` argument is not a TLS backend that reqwest
    /// understands, the `ClientBuilder` will error when calling `build`.
    ///
    /// # Advanced
    ///
    /// This is an advanced option, and can be somewhat brittle. Usage requires
    /// keeping the preconfigured TLS argument version in sync with reqwest,
    /// since version mismatches will result in an "unknown" TLS backend.
    ///
    /// If possible, it's preferable to use the methods on `ClientBuilder`
    /// to configure reqwest's TLS.
    ///
    /// # Optional
    ///
    /// This requires one of the optional features `native-tls` or
    /// `rustls-tls(-...)` to be enabled.
    #[cfg(any(feature = "native-tls", feature = "__rustls",))]
    #[cfg_attr(docsrs, doc(cfg(any(feature = "native-tls", feature = "rustls-tls"))))]
    pub fn use_preconfigured_tls(mut self, tls: impl Any) -> ClientBuilder {
        let mut tls = Some(tls);
        #[cfg(feature = "native-tls")]
        {
            if let Some(conn) =
                (&mut tls as &mut dyn Any).downcast_mut::<Option<native_tls_crate::TlsConnector>>()
            {
                let tls = conn.take().expect("is definitely Some");
                let tls = crate::tls::TlsBackend::BuiltNativeTls(tls);
                self.config.tls = tls;
                return self;
            }
        }
        #[cfg(feature = "__rustls")]
        {
            if let Some(conn) =
                (&mut tls as &mut dyn Any).downcast_mut::<Option<rustls::ClientConfig>>()
            {
                let tls = conn.take().expect("is definitely Some");
                let tls = crate::tls::TlsBackend::BuiltRustls(tls);
                self.config.tls = tls;
                return self;
            }
        }

        // Otherwise, we don't recognize the TLS backend!
        self.config.tls = crate::tls::TlsBackend::UnknownPreconfigured;
        self
    }

    /// Add TLS information as `TlsInfo` extension to responses.
    ///
    /// # Optional
    ///
    /// This requires the optional `default-tls`, `native-tls`, or `rustls-tls(-...)`
    /// feature to be enabled.
    #[cfg(feature = "__tls")]
    #[cfg_attr(
        docsrs,
        doc(cfg(any(
            feature = "default-tls",
            feature = "native-tls",
            feature = "rustls-tls"
        )))
    )]
    pub fn tls_info(mut self, tls_info: bool) -> ClientBuilder {
        self.config.tls_info = tls_info;
        self
    }

    /// Restrict the Client to be used with HTTPS only requests.
    ///
    /// Defaults to false.
    pub fn https_only(mut self, enabled: bool) -> ClientBuilder {
        self.config.https_only = enabled;
        self
    }

    /// Enables the [trust-dns](trust_dns_resolver) async resolver instead of a default threadpool using `getaddrinfo`.
    ///
    /// If the `trust-dns` feature is turned on, the default option is enabled.
    ///
    /// # Optional
    ///
    /// This requires the optional `trust-dns` feature to be enabled
    #[cfg(feature = "trust-dns")]
    #[cfg_attr(docsrs, doc(cfg(feature = "trust-dns")))]
    pub fn trust_dns(mut self, enable: bool) -> ClientBuilder {
        self.config.trust_dns = enable;
        self
    }

    /// Disables the trust-dns async resolver.
    ///
    /// This method exists even if the optional `trust-dns` feature is not enabled.
    /// This can be used to ensure a `Client` doesn't use the trust-dns async resolver
    /// even if another dependency were to enable the optional `trust-dns` feature.
    pub fn no_trust_dns(self) -> ClientBuilder {
        #[cfg(feature = "trust-dns")]
        {
            self.trust_dns(false)
        }

        #[cfg(not(feature = "trust-dns"))]
        {
            self
        }
    }

    /// Override DNS resolution for specific domains to a particular IP address.
    ///
    /// Warning
    ///
    /// Since the DNS protocol has no notion of ports, if you wish to send
    /// traffic to a particular port you must include this port in the URL
    /// itself, any port in the overridden addr will be ignored and traffic sent
    /// to the conventional port for the given scheme (e.g. 80 for http).
    pub fn resolve(self, domain: &str, addr: SocketAddr) -> ClientBuilder {
        self.resolve_to_addrs(domain, &[addr])
    }

    /// Override DNS resolution for specific domains to particular IP addresses.
    ///
    /// Warning
    ///
    /// Since the DNS protocol has no notion of ports, if you wish to send
    /// traffic to a particular port you must include this port in the URL
    /// itself, any port in the overridden addresses will be ignored and traffic sent
    /// to the conventional port for the given scheme (e.g. 80 for http).
    pub fn resolve_to_addrs(mut self, domain: &str, addrs: &[SocketAddr]) -> ClientBuilder {
        self.config
            .dns_overrides
            .insert(domain.to_string(), addrs.to_vec());
        self
    }

    /// Override the DNS resolver implementation.
    ///
    /// Pass an `Arc` wrapping a trait object implementing `Resolve`.
    /// Overrides for specific names passed to `resolve` and `resolve_to_addrs` will
    /// still be applied on top of this resolver.
    pub fn dns_resolver<R: Resolve + 'static>(mut self, resolver: Arc<R>) -> ClientBuilder {
        self.config.dns_resolver = Some(resolver as _);
        self
    }

    /// Whether to send data on the first flight ("early data") in TLS 1.3 handshakes
    /// for HTTP/3 connections.
    ///
    /// The default is false.
    #[cfg(feature = "http3")]
    #[cfg_attr(docsrs, doc(cfg(all(reqwest_unstable, feature = "http3",))))]
    pub fn set_tls_enable_early_data(mut self, enabled: bool) -> ClientBuilder {
        self.config.tls_enable_early_data = enabled;
        self
    }

    /// Maximum duration of inactivity to accept before timing out the QUIC connection.
    ///
    /// Please see docs in [`TransportConfig`] in [`quinn`].
    ///
    /// [`TransportConfig`]: https://docs.rs/quinn/latest/quinn/struct.TransportConfig.html
    #[cfg(feature = "http3")]
    #[cfg_attr(docsrs, doc(cfg(all(reqwest_unstable, feature = "http3",))))]
    pub fn set_quic_max_idle_timeout(mut self, value: Duration) -> ClientBuilder {
        self.config.quic_max_idle_timeout = Some(value);
        self
    }

    /// Maximum number of bytes the peer may transmit without acknowledgement on any one stream
    /// before becoming blocked.
    ///
    /// Please see docs in [`TransportConfig`] in [`quinn`].
    ///
    /// [`TransportConfig`]: https://docs.rs/quinn/latest/quinn/struct.TransportConfig.html
    #[cfg(feature = "http3")]
    #[cfg_attr(docsrs, doc(cfg(all(reqwest_unstable, feature = "http3",))))]
    pub fn set_quic_stream_receive_window(mut self, value: VarInt) -> ClientBuilder {
        self.config.quic_stream_receive_window = Some(value);
        self
    }

    /// Maximum number of bytes the peer may transmit across all streams of a connection before
    /// becoming blocked.
    ///
    /// Please see docs in [`TransportConfig`] in [`quinn`].
    ///
    /// [`TransportConfig`]: https://docs.rs/quinn/latest/quinn/struct.TransportConfig.html
    #[cfg(feature = "http3")]
    #[cfg_attr(docsrs, doc(cfg(all(reqwest_unstable, feature = "http3",))))]
    pub fn set_quic_receive_window(mut self, value: VarInt) -> ClientBuilder {
        self.config.quic_receive_window = Some(value);
        self
    }

    /// Maximum number of bytes to transmit to a peer without acknowledgment
    ///
    /// Please see docs in [`TransportConfig`] in [`quinn`].
    ///
    /// [`TransportConfig`]: https://docs.rs/quinn/latest/quinn/struct.TransportConfig.html
    #[cfg(feature = "http3")]
    #[cfg_attr(docsrs, doc(cfg(all(reqwest_unstable, feature = "http3",))))]
    pub fn set_quic_send_window(mut self, value: u64) -> ClientBuilder {
        self.config.quic_send_window = Some(value);
        self
    }
}

type HyperClient = hyper_util::client::legacy::Client<Connector, super::Body>;

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// Constructs a new `Client`.
    ///
    /// # Panics
    ///
    /// This method panics if a TLS backend cannot be initialized, or the resolver
    /// cannot load the system configuration.
    ///
    /// Use `Client::builder()` if you wish to handle the failure as an `Error`
    /// instead of panicking.
    pub fn new() -> Client {
        ClientBuilder::new().build().expect("Client::new()")
    }

    /// Creates a `ClientBuilder` to configure a `Client`.
    ///
    /// This is the same as `ClientBuilder::new()`.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Convenience method to make a `GET` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn get<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    /// Convenience method to make a `POST` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn post<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// Convenience method to make a `PUT` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn put<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    /// Convenience method to make a `PATCH` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn patch<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }

    /// Convenience method to make a `DELETE` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn delete<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }

    /// Convenience method to make a `HEAD` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn head<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }

    /// Start building a `Request` with the `Method` and `Url`.
    ///
    /// Returns a `RequestBuilder`, which will allow setting headers and
    /// the request body before sending.
    ///
    /// # Errors
    ///
    /// This method fails whenever the supplied `Url` cannot be parsed.
    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        let req = url.into_url().map(move |url| Request::new(method, url));
        RequestBuilder::new(self.clone(), req)
    }

    /// Executes a `Request`.
    ///
    /// A `Request` can be built manually with `Request::new()` or obtained
    /// from a RequestBuilder with `RequestBuilder::build()`.
    ///
    /// You should prefer to use the `RequestBuilder` and
    /// `RequestBuilder::send()`.
    ///
    /// # Errors
    ///
    /// This method fails if there was an error while sending request,
    /// redirect loop was detected or redirect limit was exhausted.
    pub fn execute(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, crate::Error>> {
        self.execute_request(request)
    }

    pub(super) fn execute_request(&self, req: Request) -> Pending {
        let (method, url, mut headers, body, timeout, version) = req.pieces();
        if url.scheme() != "http" && url.scheme() != "https" {
            return Pending::new_err(error::url_bad_scheme(url));
        }

        // check if we're in https_only mode and check the scheme of the current URL
        if self.inner.https_only && url.scheme() != "https" {
            return Pending::new_err(error::url_bad_scheme(url));
        }

        // insert default headers in the request headers
        // without overwriting already appended headers.
        for (key, value) in &self.inner.headers {
            if let Entry::Vacant(entry) = headers.entry(key) {
                entry.insert(value.clone());
            }
        }

        // Add cookies from the cookie store.
        #[cfg(feature = "cookies")]
        {
            if let Some(cookie_store) = self.inner.cookie_store.as_ref() {
                if headers.get(crate::header::COOKIE).is_none() {
                    add_cookie_header(&mut headers, &**cookie_store, &url);
                }
            }
        }

        let accept_encoding = self.inner.accepts.as_str();

        if let Some(accept_encoding) = accept_encoding {
            if !headers.contains_key(ACCEPT_ENCODING) && !headers.contains_key(RANGE) {
                headers.insert(ACCEPT_ENCODING, HeaderValue::from_static(accept_encoding));
            }
        }

        let uri = expect_uri(&url);

        let (reusable, body) = match body {
            Some(body) => {
                let (reusable, body) = body.try_reuse();
                (Some(reusable), body)
            }
            None => (None, Body::empty()),
        };

        self.proxy_auth(&uri, &mut headers);

        let builder = hyper::Request::builder()
            .method(method.clone())
            .uri(uri)
            .version(version);

        let in_flight = match version {
            #[cfg(feature = "http3")]
            http::Version::HTTP_3 if self.inner.h3_client.is_some() => {
                let mut req = builder.body(body).expect("valid request parts");
                *req.headers_mut() = headers.clone();
                ResponseFuture::H3(self.inner.h3_client.as_ref().unwrap().request(req))
            }
            _ => {
                let mut req = builder.body(body).expect("valid request parts");
                *req.headers_mut() = headers.clone();
                ResponseFuture::Default(self.inner.hyper.request(req))
            }
        };

        let timeout = timeout
            .or(self.inner.request_timeout)
            .map(tokio::time::sleep)
            .map(Box::pin);

        Pending {
            inner: PendingInner::Request(PendingRequest {
                method,
                url,
                headers,
                body: reusable,

                urls: Vec::new(),

                retry_count: 0,

                client: self.inner.clone(),

                in_flight,
                timeout,
            }),
        }
    }

    fn proxy_auth(&self, dst: &Uri, headers: &mut HeaderMap) {
        if !self.inner.proxies_maybe_http_auth {
            return;
        }

        // Only set the header here if the destination scheme is 'http',
        // since otherwise, the header will be included in the CONNECT tunnel
        // request instead.
        if dst.scheme() != Some(&Scheme::HTTP) {
            return;
        }

        if headers.contains_key(PROXY_AUTHORIZATION) {
            return;
        }

        for proxy in self.inner.proxies.iter() {
            if proxy.is_match(dst) {
                if let Some(header) = proxy.http_basic_auth(dst) {
                    headers.insert(PROXY_AUTHORIZATION, header);
                }

                break;
            }
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = f.debug_struct("Client");
        self.inner.fmt_fields(&mut builder);
        builder.finish()
    }
}

impl tower_service::Service<Request> for Client {
    type Response = Response;
    type Error = crate::Error;
    type Future = Pending;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        self.execute_request(req)
    }
}

impl tower_service::Service<Request> for &'_ Client {
    type Response = Response;
    type Error = crate::Error;
    type Future = Pending;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        self.execute_request(req)
    }
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut builder = f.debug_struct("ClientBuilder");
        self.config.fmt_fields(&mut builder);
        builder.finish()
    }
}

impl Config {
    fn fmt_fields(&self, f: &mut fmt::DebugStruct<'_, '_>) {
        // Instead of deriving Debug, only print fields when their output
        // would provide relevant or interesting data.

        #[cfg(feature = "cookies")]
        {
            if let Some(_) = self.cookie_store {
                f.field("cookie_store", &true);
            }
        }

        f.field("accepts", &self.accepts);

        if !self.proxies.is_empty() {
            f.field("proxies", &self.proxies);
        }

        if !self.redirect_policy.is_default() {
            f.field("redirect_policy", &self.redirect_policy);
        }

        if self.referer {
            f.field("referer", &true);
        }

        f.field("default_headers", &self.headers);

        if self.http1_title_case_headers {
            f.field("http1_title_case_headers", &true);
        }

        if self.http1_allow_obsolete_multiline_headers_in_responses {
            f.field("http1_allow_obsolete_multiline_headers_in_responses", &true);
        }

        if self.http1_ignore_invalid_headers_in_responses {
            f.field("http1_ignore_invalid_headers_in_responses", &true);
        }

        if self.http1_allow_spaces_after_header_name_in_responses {
            f.field("http1_allow_spaces_after_header_name_in_responses", &true);
        }

        if matches!(self.http_version_pref, HttpVersionPref::Http1) {
            f.field("http1_only", &true);
        }

        if matches!(self.http_version_pref, HttpVersionPref::Http2) {
            f.field("http2_prior_knowledge", &true);
        }

        if let Some(ref d) = self.connect_timeout {
            f.field("connect_timeout", d);
        }

        if let Some(ref d) = self.timeout {
            f.field("timeout", d);
        }

        if let Some(ref v) = self.local_address {
            f.field("local_address", v);
        }

        if let Some(ref v) = self.interface {
            f.field("interface", v);
        }

        if self.nodelay {
            f.field("tcp_nodelay", &true);
        }

        #[cfg(feature = "native-tls")]
        {
            if !self.hostname_verification {
                f.field("danger_accept_invalid_hostnames", &true);
            }
        }

        #[cfg(feature = "__tls")]
        {
            if !self.certs_verification {
                f.field("danger_accept_invalid_certs", &true);
            }

            if let Some(ref min_tls_version) = self.min_tls_version {
                f.field("min_tls_version", min_tls_version);
            }

            if let Some(ref max_tls_version) = self.max_tls_version {
                f.field("max_tls_version", max_tls_version);
            }

            f.field("tls_sni", &self.tls_sni);

            f.field("tls_info", &self.tls_info);
        }

        #[cfg(all(feature = "native-tls-crate", feature = "__rustls"))]
        {
            f.field("tls_backend", &self.tls);
        }

        if !self.dns_overrides.is_empty() {
            f.field("dns_overrides", &self.dns_overrides);
        }

        #[cfg(feature = "http3")]
        {
            if self.tls_enable_early_data {
                f.field("tls_enable_early_data", &true);
            }
        }
    }
}

struct ClientRef {
    accepts: Accepts,
    #[cfg(feature = "cookies")]
    cookie_store: Option<Arc<dyn cookie::CookieStore>>,
    headers: HeaderMap,
    hyper: HyperClient,
    #[cfg(feature = "http3")]
    h3_client: Option<H3Client>,
    redirect_policy: redirect::Policy,
    referer: bool,
    request_timeout: Option<Duration>,
    proxies: Arc<Vec<Proxy>>,
    proxies_maybe_http_auth: bool,
    https_only: bool,
}

impl ClientRef {
    fn fmt_fields(&self, f: &mut fmt::DebugStruct<'_, '_>) {
        // Instead of deriving Debug, only print fields when their output
        // would provide relevant or interesting data.

        #[cfg(feature = "cookies")]
        {
            if let Some(_) = self.cookie_store {
                f.field("cookie_store", &true);
            }
        }

        f.field("accepts", &self.accepts);

        if !self.proxies.is_empty() {
            f.field("proxies", &self.proxies);
        }

        if !self.redirect_policy.is_default() {
            f.field("redirect_policy", &self.redirect_policy);
        }

        if self.referer {
            f.field("referer", &true);
        }

        f.field("default_headers", &self.headers);

        if let Some(ref d) = self.request_timeout {
            f.field("timeout", d);
        }
    }
}

pin_project! {
    pub struct Pending {
        #[pin]
        inner: PendingInner,
    }
}

enum PendingInner {
    Request(PendingRequest),
    Error(Option<crate::Error>),
}

pin_project! {
    struct PendingRequest {
        method: Method,
        url: Url,
        headers: HeaderMap,
        body: Option<Option<Bytes>>,

        urls: Vec<Url>,

        retry_count: usize,

        client: Arc<ClientRef>,

        #[pin]
        in_flight: ResponseFuture,
        #[pin]
        timeout: Option<Pin<Box<Sleep>>>,
    }
}

enum ResponseFuture {
    Default(HyperResponseFuture),
    #[cfg(feature = "http3")]
    H3(H3ResponseFuture),
}

impl PendingRequest {
    fn in_flight(self: Pin<&mut Self>) -> Pin<&mut ResponseFuture> {
        self.project().in_flight
    }

    fn timeout(self: Pin<&mut Self>) -> Pin<&mut Option<Pin<Box<Sleep>>>> {
        self.project().timeout
    }

    fn urls(self: Pin<&mut Self>) -> &mut Vec<Url> {
        self.project().urls
    }

    fn headers(self: Pin<&mut Self>) -> &mut HeaderMap {
        self.project().headers
    }

    fn retry_error(mut self: Pin<&mut Self>, err: &(dyn std::error::Error + 'static)) -> bool {
        if !is_retryable_error(err) {
            return false;
        }

        trace!("can retry {:?}", err);

        let body = match self.body {
            Some(Some(ref body)) => Body::reusable(body.clone()),
            Some(None) => {
                debug!("error was retryable, but body not reusable");
                return false;
            }
            None => Body::empty(),
        };

        if self.retry_count >= 2 {
            trace!("retry count too high");
            return false;
        }
        self.retry_count += 1;

        let uri = expect_uri(&self.url);

        *self.as_mut().in_flight().get_mut() = match *self.as_mut().in_flight().as_ref() {
            #[cfg(feature = "http3")]
            ResponseFuture::H3(_) => {
                let mut req = hyper::Request::builder()
                    .method(self.method.clone())
                    .uri(uri)
                    .body(body)
                    .expect("valid request parts");
                *req.headers_mut() = self.headers.clone();
                ResponseFuture::H3(
                    self.client
                        .h3_client
                        .as_ref()
                        .expect("H3 client must exists, otherwise we can't have a h3 request here")
                        .request(req),
                )
            }
            _ => {
                let mut req = hyper::Request::builder()
                    .method(self.method.clone())
                    .uri(uri)
                    .body(body)
                    .expect("valid request parts");
                *req.headers_mut() = self.headers.clone();
                ResponseFuture::Default(self.client.hyper.request(req))
            }
        };

        true
    }
}

fn is_retryable_error(err: &(dyn std::error::Error + 'static)) -> bool {
    #[cfg(feature = "http3")]
    if let Some(cause) = err.source() {
        if let Some(err) = cause.downcast_ref::<h3::Error>() {
            debug!("determining if HTTP/3 error {} can be retried", err);
            // TODO: Does h3 provide an API for checking the error?
            return err.to_string().as_str() == "timeout";
        }
    }

    if let Some(cause) = err.source() {
        if let Some(err) = cause.downcast_ref::<h2::Error>() {
            // They sent us a graceful shutdown, try with a new connection!
            if err.is_go_away() && err.is_remote() && err.reason() == Some(h2::Reason::NO_ERROR) {
                return true;
            }

            // REFUSED_STREAM was sent from the server, which is safe to retry.
            // https://www.rfc-editor.org/rfc/rfc9113.html#section-8.7-3.2
            if err.is_reset() && err.is_remote() && err.reason() == Some(h2::Reason::REFUSED_STREAM)
            {
                return true;
            }
        }
    }
    false
}

impl Pending {
    pub(super) fn new_err(err: crate::Error) -> Pending {
        Pending {
            inner: PendingInner::Error(Some(err)),
        }
    }

    fn inner(self: Pin<&mut Self>) -> Pin<&mut PendingInner> {
        self.project().inner
    }
}

impl Future for Pending {
    type Output = Result<Response, crate::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.inner();
        match inner.get_mut() {
            PendingInner::Request(ref mut req) => Pin::new(req).poll(cx),
            PendingInner::Error(ref mut err) => Poll::Ready(Err(err
                .take()
                .expect("Pending error polled more than once"))),
        }
    }
}

impl Future for PendingRequest {
    type Output = Result<Response, crate::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(delay) = self.as_mut().timeout().as_mut().as_pin_mut() {
            if let Poll::Ready(()) = delay.poll(cx) {
                return Poll::Ready(Err(
                    crate::error::request(crate::error::TimedOut).with_url(self.url.clone())
                ));
            }
        }

        loop {
            let res = match self.as_mut().in_flight().get_mut() {
                ResponseFuture::Default(r) => match Pin::new(r).poll(cx) {
                    Poll::Ready(Err(e)) => {
                        if self.as_mut().retry_error(&e) {
                            continue;
                        }
                        return Poll::Ready(Err(
                            crate::error::request(e).with_url(self.url.clone())
                        ));
                    }
                    Poll::Ready(Ok(res)) => res,
                    Poll::Pending => return Poll::Pending,
                },
                #[cfg(feature = "http3")]
                ResponseFuture::H3(r) => match Pin::new(r).poll(cx) {
                    Poll::Ready(Err(e)) => {
                        if self.as_mut().retry_error(&e) {
                            continue;
                        }
                        return Poll::Ready(Err(
                            crate::error::request(e).with_url(self.url.clone())
                        ));
                    }
                    Poll::Ready(Ok(res)) => res,
                    Poll::Pending => return Poll::Pending,
                },
            };

            #[cfg(feature = "cookies")]
            {
                if let Some(ref cookie_store) = self.client.cookie_store {
                    let mut cookies =
                        cookie::extract_response_cookie_headers(&res.headers()).peekable();
                    if cookies.peek().is_some() {
                        cookie_store.set_cookies(&mut cookies, &self.url);
                    }
                }
            }
            let should_redirect = match res.status() {
                StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER => {
                    self.body = None;
                    for header in &[
                        TRANSFER_ENCODING,
                        CONTENT_ENCODING,
                        CONTENT_TYPE,
                        CONTENT_LENGTH,
                    ] {
                        self.headers.remove(header);
                    }

                    match self.method {
                        Method::GET | Method::HEAD => {}
                        _ => {
                            self.method = Method::GET;
                        }
                    }
                    true
                }
                StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {
                    match self.body {
                        Some(Some(_)) | None => true,
                        Some(None) => false,
                    }
                }
                _ => false,
            };
            if should_redirect {
                let loc = res.headers().get(LOCATION).and_then(|val| {
                    let loc = (|| -> Option<Url> {
                        // Some sites may send a utf-8 Location header,
                        // even though we're supposed to treat those bytes
                        // as opaque, we'll check specifically for utf8.
                        self.url.join(str::from_utf8(val.as_bytes()).ok()?).ok()
                    })();

                    // Check that the `url` is also a valid `http::Uri`.
                    //
                    // If not, just log it and skip the redirect.
                    let loc = loc.and_then(|url| {
                        if try_uri(&url).is_some() {
                            Some(url)
                        } else {
                            None
                        }
                    });

                    if loc.is_none() {
                        debug!("Location header had invalid URI: {:?}", val);
                    }
                    loc
                });
                if let Some(loc) = loc {
                    if self.client.referer {
                        if let Some(referer) = make_referer(&loc, &self.url) {
                            self.headers.insert(REFERER, referer);
                        }
                    }
                    let url = self.url.clone();
                    self.as_mut().urls().push(url);
                    let action = self
                        .client
                        .redirect_policy
                        .check(res.status(), &loc, &self.urls);

                    match action {
                        redirect::ActionKind::Follow => {
                            debug!("redirecting '{}' to '{}'", self.url, loc);

                            if loc.scheme() != "http" && loc.scheme() != "https" {
                                return Poll::Ready(Err(error::url_bad_scheme(loc)));
                            }

                            if self.client.https_only && loc.scheme() != "https" {
                                return Poll::Ready(Err(error::redirect(
                                    error::url_bad_scheme(loc.clone()),
                                    loc,
                                )));
                            }

                            self.url = loc;
                            let mut headers =
                                std::mem::replace(self.as_mut().headers(), HeaderMap::new());

                            remove_sensitive_headers(&mut headers, &self.url, &self.urls);
                            let uri = expect_uri(&self.url);
                            let body = match self.body {
                                Some(Some(ref body)) => Body::reusable(body.clone()),
                                _ => Body::empty(),
                            };

                            // Add cookies from the cookie store.
                            #[cfg(feature = "cookies")]
                            {
                                if let Some(ref cookie_store) = self.client.cookie_store {
                                    add_cookie_header(&mut headers, &**cookie_store, &self.url);
                                }
                            }

                            *self.as_mut().in_flight().get_mut() =
                                match *self.as_mut().in_flight().as_ref() {
                                    #[cfg(feature = "http3")]
                                    ResponseFuture::H3(_) => {
                                        let mut req = hyper::Request::builder()
                                            .method(self.method.clone())
                                            .uri(uri.clone())
                                            .body(body)
                                            .expect("valid request parts");
                                        *req.headers_mut() = headers.clone();
                                        std::mem::swap(self.as_mut().headers(), &mut headers);
                                        ResponseFuture::H3(self.client.h3_client
                        .as_ref()
                        .expect("H3 client must exists, otherwise we can't have a h3 request here")
                                            .request(req))
                                    }
                                    _ => {
                                        let mut req = hyper::Request::builder()
                                            .method(self.method.clone())
                                            .uri(uri.clone())
                                            .body(body)
                                            .expect("valid request parts");
                                        *req.headers_mut() = headers.clone();
                                        std::mem::swap(self.as_mut().headers(), &mut headers);
                                        ResponseFuture::Default(self.client.hyper.request(req))
                                    }
                                };

                            continue;
                        }
                        redirect::ActionKind::Stop => {
                            debug!("redirect policy disallowed redirection to '{}'", loc);
                        }
                        redirect::ActionKind::Error(err) => {
                            return Poll::Ready(Err(crate::error::redirect(err, self.url.clone())));
                        }
                    }
                }
            }

            let res = Response::new(
                res,
                self.url.clone(),
                self.client.accepts,
                self.timeout.take(),
            );
            return Poll::Ready(Ok(res));
        }
    }
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.inner {
            PendingInner::Request(ref req) => f
                .debug_struct("Pending")
                .field("method", &req.method)
                .field("url", &req.url)
                .finish(),
            PendingInner::Error(ref err) => f.debug_struct("Pending").field("error", err).finish(),
        }
    }
}

fn make_referer(next: &Url, previous: &Url) -> Option<HeaderValue> {
    if next.scheme() == "http" && previous.scheme() == "https" {
        return None;
    }

    let mut referer = previous.clone();
    let _ = referer.set_username("");
    let _ = referer.set_password(None);
    referer.set_fragment(None);
    referer.as_str().parse().ok()
}

#[cfg(feature = "cookies")]
fn add_cookie_header(headers: &mut HeaderMap, cookie_store: &dyn cookie::CookieStore, url: &Url) {
    if let Some(header) = cookie_store.cookies(url) {
        headers.insert(crate::header::COOKIE, header);
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn execute_request_rejects_invald_urls() {
        let url_str = "hxxps://www.rust-lang.org/";
        let url = url::Url::parse(url_str).unwrap();
        let result = crate::get(url.clone()).await;

        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.is_builder());
        assert_eq!(url_str, err.url().unwrap().as_str());
    }
}
