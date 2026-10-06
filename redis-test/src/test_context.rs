use crate::server::{
    Module, Output, RedisServer, RedisServerBuilder, RedisServerCommand, use_protocol,
};
use crate::utils::{TlsFilePaths, build_single_client};
use crate::version::{AvailableComponents, TestContextVersioning};
#[cfg(feature = "aio")]
use redis::RedisResult;
use redis::{
    Client, Connection, ConnectionAddr, ErrorKind, ProtocolVersion, ServerErrorKind, TypedCommands,
};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// The duration in milliseconds to wait to let the server accept connections
const MAX_INITIAL_CONNECTION_DURATION_MS: u128 = 2500; // 2.5 seconds for initial connection

/// The duration in milliseconds to wait to let the server load data
const MAX_LOADING_DURATION_MS: u128 = 2500; // 2.5 seconds for loading database content

/// The backoff time in milliseconds between checks if the server is ready
///
/// This is short to ensure responsive tests, but not 0 to assure we're not starving the server
/// startup on hosts with only few CPUs.
const BACKOFF_MS: u64 = 1; // 1 ms

/// A builder for [`TestContext`]
///
/// # Example
///
/// ```rust,no_run
/// use redis_test::TestContextBuilder;
/// use redis_test::server::Module;
///
/// let ctx = TestContextBuilder::new().module(Module::Json).build();
/// let connection = ctx.connection();
/// // Use `connection` to run commands
/// ```
// Note that this builder is an owned-builder as we want to build in a single chain anyway and do
// not have to build multiple instances from the same builder. Also, this spares us cloning
// considerations.
#[derive(Default)]
pub struct TestContextBuilder {
    server_builder: RedisServerBuilder,
    protocol: Option<redis::ProtocolVersion>,
}

impl TestContextBuilder {
    /// Starts a fresh builder
    pub fn new() -> Self {
        Default::default()
    }

    pub fn address(mut self, address: ConnectionAddr) -> Self {
        self.server_builder = self.server_builder.address(address);
        self
    }

    pub fn server_type(mut self, server_type: crate::server::ServerType) -> Self {
        self.server_builder = self.server_builder.server_type(server_type);
        self
    }

    pub fn protocol(mut self, protocol: redis::ProtocolVersion) -> Self {
        self.protocol = Some(protocol);
        self
    }

    pub fn config(mut self, config_file: PathBuf) -> Self {
        self.server_builder = self.server_builder.config(config_file);
        self
    }

    pub fn cert_auth_field(mut self, cert_auth_field: impl Into<String>) -> Self {
        self.server_builder = self.server_builder.cert_auth_field(cert_auth_field);
        self
    }

    pub fn cert_auth_field_opt(mut self, opt_cert_auth_field: Option<impl Into<String>>) -> Self {
        self.server_builder = self.server_builder.cert_auth_field_opt(opt_cert_auth_field);
        self
    }

    pub fn module(mut self, module: Module) -> Self {
        self.server_builder = self.server_builder.module(module);
        self
    }

    pub fn modules(mut self, modules: &[Module]) -> Self {
        self.server_builder = self.server_builder.modules(modules);
        self
    }

    pub fn mtls(mut self, enable_mtls: bool) -> Self {
        self.server_builder = self.server_builder.mtls(enable_mtls);
        self
    }

    pub fn tls_paths(mut self, tls_paths: TlsFilePaths) -> Self {
        self.server_builder = self.server_builder.tls_paths(tls_paths);
        self
    }

    pub fn tls_paths_opt(mut self, opt_tls_paths: Option<TlsFilePaths>) -> Self {
        self.server_builder = self.server_builder.tls_paths_opt(opt_tls_paths);
        self
    }

    pub fn tempdir(mut self, tempdir: TempDir) -> Self {
        self.server_builder = self.server_builder.tempdir(tempdir);
        self
    }

    pub fn panicking_drop_info_output(mut self, output: Output) -> Self {
        self.server_builder = self.server_builder.panicking_drop_info_output(output);
        self
    }

    /// Builds the [`TestContext`] for this instance
    pub fn build(self) -> TestContext {
        self.refine_and_build(|_| {})
    }

    /// Builds the [`TestContext`] for this instance after refining the arguments for the server
    ///
    /// # Arguments
    ///
    /// * `refiner` - See [`RedisServerBuilder::refine_and_build`]
    pub fn refine_and_build(self, refiner: impl FnOnce(&mut RedisServerCommand)) -> TestContext {
        TestContext::from_builder_with_refiner(self, refiner)
    }
}

/// `panic`ks and dumps the server log file
macro_rules! panic_w_server_log_dump {
    ($server:ident, $msg:literal $(, $arg:tt)*) => {
        let msg = format!($msg, $(, $arg)*);
        let process_info = $server.stop_with_info();
        panic!("{msg}\n{process_info}")
    }
}

/// Utility wrapper for a standalone Redis server instance for testing.
///
/// # Example
///
/// Use `default()` to build a [`TestContext`] with default settings:
///
/// ```rust,no_run
/// use redis_test::TestContext;
///
/// let ctx = TestContext::default();
/// let connection = ctx.connection();
/// // Use `connection` to run commands
/// ```
///
/// If you need a custom setup, use [`TestContextBuilder`]:
///
/// ```rust,no_run
/// use redis_test::TestContextBuilder;
/// use redis_test::server::Module;
///
/// let ctx = TestContextBuilder::new().module(Module::Json).build();
/// let connection = ctx.connection();
/// // Use `connection` to run commands
/// ```
#[non_exhaustive]
pub struct TestContext {
    pub server: RedisServer,
    pub client: redis::Client,
    pub protocol: ProtocolVersion,
}

impl Default for TestContext {
    fn default() -> Self {
        TestContextBuilder::new().build()
    }
}

impl TestContext {
    /// Builds a new instance from a [`TestContextBuilder`]
    // We intentionally do _not_ implement `From<RedisServer>` as that would be public.
    //
    // Instead, users should use the [`TestContextBuilder`] to trigger the building.
    fn from_builder_with_refiner(
        builder: TestContextBuilder,
        refiner: impl FnOnce(&mut RedisServerCommand),
    ) -> Self {
        let mut server = builder.server_builder.refine_and_build(refiner);
        let protocol = builder.protocol.unwrap_or_else(use_protocol);
        let client = build_single_client(
            server.connection_info_with_protocol(protocol),
            &server.tls_paths,
            server.mtls,
        )
        .unwrap();

        if server.tls_paths.is_some() {
            crate::utils::start_tls_crypto_provider();
        }

        // Give the server some time to come up
        let con = Self::get_initial_connection(&mut server, &client);
        Self::wait_until_ready(&mut server, con);

        // Here the server is up and usable. Done :-)
        Self {
            server,
            client,
            protocol,
        }
    }

    /// Get the initial connection to a server
    ///
    /// # Panics
    ///
    /// Getting a connection is tried for [`MAX_INITIAL_CONNECTION_DURATION_MS`] milliseconds. If
    /// there's still no connection after that, the function panics.
    fn get_initial_connection(server: &mut RedisServer, client: &Client) -> Connection {
        let backoff = Duration::from_millis(BACKOFF_MS);
        let connection_ts = Instant::now();
        loop {
            let err = match client.get_connection() {
                Err(err) => {
                    if !err.is_connection_refusal() {
                        panic_w_server_log_dump!(server, "Could not connect: {err}");
                    }
                    err
                }
                Ok(con) => {
                    // We've got a connection! Run with it
                    return con;
                }
            };

            // Getting a connection was refused.
            // That's worth a retry, as the server might not be up yet.

            // Check if the server is still alive
            if !server.is_alive() {
                panic_w_server_log_dump!(server, "Server exited before we could connect");
            }

            // Check if there is time left to retry
            let waited_ms = Instant::now().duration_since(connection_ts).as_millis();
            if waited_ms > MAX_INITIAL_CONNECTION_DURATION_MS {
                panic_w_server_log_dump!(
                    server,
                    "still no connection after {waited_ms} ms. Aborting. Last error: {err}"
                );
            }

            // Wait before re-trying
            sleep(backoff);
        }
    }

    /// Wait until the server finished loading its data
    ///
    /// # Panics
    ///
    /// The server is checked for [`MAX_LOADING_DURATION_MS`] milliseconds if it has
    /// finished loading its data. If did not finish after that, the function panics.
    fn wait_until_ready(server: &mut RedisServer, mut con: Connection) {
        let backoff = Duration::from_millis(BACKOFF_MS);
        let loading_ts = Instant::now();
        loop {
            let err = match con.flushdb() {
                Ok(_) => return,
                Err(err) => {
                    if !matches!(err.kind(), ErrorKind::Server(ServerErrorKind::BusyLoading)) {
                        panic_w_server_log_dump!(server, "Failed to flush database: {err}");
                    }
                    err
                }
            };

            // The server is still busy loading its data

            // Check if there is time left to retry
            let waited_ms = Instant::now().duration_since(loading_ts).as_millis();
            if waited_ms > MAX_LOADING_DURATION_MS {
                panic_w_server_log_dump!(
                    server,
                    "still loading after {waited_ms} ms. Aborting. Last error: {err}"
                );
            }

            // Wait before re-trying
            sleep(backoff);
        }
    }

    pub fn connection(&self) -> redis::Connection {
        self.client.get_connection().unwrap()
    }

    #[cfg(feature = "aio")]
    pub async fn async_connection(&self) -> RedisResult<redis::aio::MultiplexedConnection> {
        self.client.get_multiplexed_async_connection().await
    }

    #[cfg(feature = "aio")]
    pub async fn async_pubsub(&self) -> RedisResult<redis::aio::PubSub> {
        self.client.get_async_pubsub().await
    }

    pub fn stop_server(&mut self) {
        self.server.stop();
    }
}

impl TestContextVersioning for TestContext {
    fn get_available_components(&self) -> AvailableComponents {
        let mut conn = self.connection();
        AvailableComponents::from(&mut conn)
    }
}
