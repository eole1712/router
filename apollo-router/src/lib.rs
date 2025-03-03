//! Starts a server that will handle http graphql requests.

extern crate core;

mod axum_http_server_factory;
pub mod configuration;
mod executable;
mod files;
mod http_server_factory;
pub mod plugins;
mod reload;
mod router_factory;
mod state_machine;
pub mod subscriber;

use crate::configuration::validate_configuration;
use crate::reload::Error as ReloadError;
use crate::router_factory::{RouterServiceFactory, YamlRouterServiceFactory};
use crate::state_machine::StateMachine;
use crate::Event::{NoMoreConfiguration, NoMoreSchema};
use apollo_router_core::prelude::*;
use axum_http_server_factory::AxumHttpServerFactory;
use configuration::{Configuration, ListenAddr};
use derivative::Derivative;
use derive_more::{Display, From};
use displaydoc::Display as DisplayDoc;
pub use executable::{main, rt_main};
use futures::channel::{mpsc, oneshot};
use futures::prelude::*;
use futures::FutureExt;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use thiserror::Error;
use tokio::task::spawn;
use tracing::subscriber::SetGlobalDefaultError;
use url::Url;
use Event::{Shutdown, UpdateConfiguration, UpdateSchema};

type SchemaStream = Pin<Box<dyn Stream<Item = graphql::Schema> + Send>>;

/// Error types for FederatedServer.
#[derive(Error, Debug, DisplayDoc)]
pub enum FederatedServerError {
    /// failed to start server
    StartupError,

    /// failed to stop HTTP Server
    HttpServerLifecycleError,

    /// no valid configuration was supplied
    NoConfiguration,

    /// no valid schema was supplied
    NoSchema,

    /// could not deserialize configuration: {0}
    DeserializeConfigError(serde_yaml::Error),

    /// could not read configuration: {0}
    ReadConfigError(std::io::Error),

    /// {0}
    ConfigError(configuration::ConfigurationError),

    /// could not read schema: {0}
    ReadSchemaError(graphql::SchemaError),

    /// could not create the HTTP pipeline: {0}
    ServiceCreationError(tower::BoxError),

    /// could not create the HTTP server: {0}
    ServerCreationError(std::io::Error),

    /// could not configure spaceport
    ServerSpaceportError,

    /// no reload handle available
    NoReloadTracingHandleError,

    /// could not set global subscriber: {0}
    SetGlobalSubscriberError(SetGlobalDefaultError),

    /// could not reload tracing layer: {0}
    ReloadTracingLayerError(ReloadError),
}

/// The user supplied schema. Either a static instance or a stream for hot reloading.
#[derive(From, Display, Derivative)]
#[derivative(Debug)]
pub enum SchemaKind {
    /// A static schema.
    #[display(fmt = "Instance")]
    Instance(Box<graphql::Schema>),

    /// A stream of schema.
    #[display(fmt = "Stream")]
    Stream(#[derivative(Debug = "ignore")] SchemaStream),

    /// A YAML file that may be watched for changes.
    #[display(fmt = "File")]
    File {
        /// The path of the schema file.
        path: PathBuf,

        /// `true` to watch the file for changes and hot apply them.
        watch: bool,

        /// When watching, the delay to wait before applying the new schema.
        delay: Option<Duration>,
    },

    /// Apollo managed federation.
    #[display(fmt = "Registry")]
    Registry {
        /// The Apollo key: <YOUR_GRAPH_API_KEY>
        apollo_key: String,

        /// The apollo graph reference: <YOUR_GRAPH_ID>@<VARIANT>
        apollo_graph_ref: String,

        /// The endpoint polled to fetch its latest supergraph schema.
        url: Option<Url>,

        /// The duration between polling
        poll_interval: Duration,
    },
}

impl From<graphql::Schema> for SchemaKind {
    fn from(schema: graphql::Schema) -> Self {
        Self::Instance(Box::new(schema))
    }
}

impl SchemaKind {
    /// Convert this schema into a stream regardless of if is static or not. Allows for unified handling later.
    fn into_stream(self) -> impl Stream<Item = Event> {
        match self {
            SchemaKind::Instance(instance) => stream::iter(vec![UpdateSchema(instance)]).boxed(),
            SchemaKind::Stream(stream) => {
                stream.map(|schema| UpdateSchema(Box::new(schema))).boxed()
            }
            SchemaKind::File { path, watch, delay } => {
                // Sanity check, does the schema file exists, if it doesn't then bail.
                if !path.exists() {
                    tracing::error!(
                        "Schema file at path '{}' does not exist.",
                        path.to_string_lossy()
                    );
                    stream::empty().boxed()
                } else {
                    //The schema file exists try and load it
                    match ConfigurationKind::read_schema(&path) {
                        Ok(schema) => {
                            if watch {
                                files::watch(path.to_owned(), delay)
                                    .filter_map(move |_| {
                                        future::ready(ConfigurationKind::read_schema(&path).ok())
                                    })
                                    .map(|schema| UpdateSchema(Box::new(schema)))
                                    .boxed()
                            } else {
                                stream::once(future::ready(UpdateSchema(Box::new(schema)))).boxed()
                            }
                        }
                        Err(err) => {
                            tracing::error!("Failed to read schema: {}", err);
                            stream::empty().boxed()
                        }
                    }
                }
            }
            SchemaKind::Registry {
                apollo_key,
                apollo_graph_ref,
                url,
                poll_interval,
            } => apollo_uplink::stream_supergraph(apollo_key, apollo_graph_ref, url, poll_interval)
                .filter_map(|res| {
                    future::ready(match res {
                        Ok(schema_result) => schema_result
                            .schema
                            .parse()
                            .map_err(|e| {
                                tracing::error!("could not parse schema: {:?}", e);
                            })
                            .ok(),

                        Err(e) => {
                            tracing::error!("error downloading the schema from Uplink: {:?}", e);
                            None
                        }
                    })
                })
                .map(|schema| UpdateSchema(Box::new(schema)))
                .boxed(),
        }
        .chain(stream::iter(vec![NoMoreSchema]))
    }
}

type ConfigurationStream = Pin<Box<dyn Stream<Item = Configuration> + Send>>;

/// The user supplied config. Either a static instance or a stream for hot reloading.
#[derive(From, Display, Derivative)]
#[derivative(Debug)]
pub enum ConfigurationKind {
    /// A static configuration.
    #[display(fmt = "Instance")]
    #[from(types(Configuration))]
    Instance(Box<Configuration>),

    /// A configuration stream where the server will react to new configuration. If possible
    /// the configuration will be applied without restarting the internal http server.
    #[display(fmt = "Stream")]
    Stream(#[derivative(Debug = "ignore")] ConfigurationStream),

    /// A yaml file that may be watched for changes
    #[display(fmt = "File")]
    File {
        /// The path of the configuration file.
        path: PathBuf,

        /// `true` to watch the file for changes and hot apply them.
        watch: bool,

        /// When watching, the delay to wait before applying the new configuration.
        delay: Option<Duration>,
    },
}

impl ConfigurationKind {
    /// Convert this config into a stream regardless of if is static or not. Allows for unified handling later.
    fn into_stream(self) -> impl Stream<Item = Event> {
        match self {
            ConfigurationKind::Instance(instance) => {
                stream::iter(vec![UpdateConfiguration(instance)]).boxed()
            }
            ConfigurationKind::Stream(stream) => {
                stream.map(|x| UpdateConfiguration(Box::new(x))).boxed()
            }
            ConfigurationKind::File { path, watch, delay } => {
                // Sanity check, does the config file exists, if it doesn't then bail.
                if !path.exists() {
                    tracing::error!(
                        "configuration file at path '{}' does not exist.",
                        path.to_string_lossy()
                    );
                    stream::empty().boxed()
                } else {
                    match ConfigurationKind::read_config(&path) {
                        Ok(configuration) => {
                            if watch {
                                files::watch(path.to_owned(), delay)
                                    .filter_map(move |_| {
                                        future::ready(match ConfigurationKind::read_config(&path) {
                                            Ok(config) => Some(config),
                                            Err(err) => {
                                                tracing::error!("{}", err);
                                                None
                                            }
                                        })
                                    })
                                    .map(|x| UpdateConfiguration(Box::new(x)))
                                    .boxed()
                            } else {
                                stream::once(future::ready(UpdateConfiguration(Box::new(
                                    configuration,
                                ))))
                                .boxed()
                            }
                        }
                        Err(err) => {
                            tracing::error!("{}", err);
                            stream::empty().boxed()
                        }
                    }
                }
            }
        }
        .chain(stream::iter(vec![NoMoreConfiguration]))
        .boxed()
    }

    fn read_config(path: &Path) -> Result<Configuration, FederatedServerError> {
        let config = fs::read_to_string(path).map_err(FederatedServerError::ReadConfigError)?;
        let config = validate_configuration(&config).map_err(FederatedServerError::ConfigError)?;

        Ok(config)
    }

    fn read_schema(path: &Path) -> Result<graphql::Schema, FederatedServerError> {
        graphql::Schema::read(path).map_err(FederatedServerError::ReadSchemaError)
    }
}

type ShutdownFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The user supplied shutdown hook.
#[derive(Display, Derivative)]
#[derivative(Debug)]
pub enum ShutdownKind {
    /// No graceful shutdown
    #[display(fmt = "None")]
    None,

    /// A custom shutdown future.
    #[display(fmt = "Custom")]
    Custom(#[derivative(Debug = "ignore")] ShutdownFuture),

    /// Watch for Ctl-C signal.
    #[display(fmt = "CtrlC")]
    CtrlC,
}

impl ShutdownKind {
    /// Convert this shutdown hook into a future. Allows for unified handling later.
    fn into_stream(self) -> impl Stream<Item = Event> {
        match self {
            ShutdownKind::None => stream::pending::<Event>().boxed(),
            ShutdownKind::Custom(future) => future.map(|_| Shutdown).into_stream().boxed(),
            ShutdownKind::CtrlC => async {
                tokio::signal::ctrl_c()
                    .await
                    .expect("Failed to install CTRL+C signal handler");
            }
            .map(|_| Shutdown)
            .into_stream()
            .boxed(),
        }
    }
}

/// Federated server takes requests and federates a response based on calls to subgraphs.
///
/// # Examples
///
/// ```
/// use apollo_router_core::prelude::*;
/// use apollo_router::ApolloRouterBuilder;
/// use apollo_router::{ConfigurationKind, SchemaKind, ShutdownKind};
/// use apollo_router::configuration::Configuration;
///
/// async {
///     let configuration = serde_yaml::from_str::<Configuration>("Config").unwrap();
///     let schema: graphql::Schema = "schema".parse().unwrap();
///     let server = ApolloRouterBuilder::default()
///             .configuration(ConfigurationKind::Instance(Box::new(configuration)))
///             .schema(SchemaKind::Instance(Box::new(schema)))
///             .shutdown(ShutdownKind::CtrlC)
///             .build();
///     server.serve().await;
/// };
/// ```
///
/// Shutdown via handle.
/// ```
/// use apollo_router_core::prelude::*;
/// use apollo_router::ApolloRouterBuilder;
/// use apollo_router::{ConfigurationKind, SchemaKind, ShutdownKind};
/// use apollo_router::configuration::Configuration;
///
/// async {
///     let configuration = serde_yaml::from_str::<Configuration>("Config").unwrap();
///     let schema: graphql::Schema = "schema".parse().unwrap();
///     let server = ApolloRouterBuilder::default()
///             .configuration(ConfigurationKind::Instance(Box::new(configuration)))
///             .schema(SchemaKind::Instance(Box::new(schema)))
///             .shutdown(ShutdownKind::CtrlC)
///             .build();
///     let handle = server.serve();
///     handle.shutdown().await;
/// };
/// ```
///
pub struct ApolloRouter<RF>
where
    RF: RouterServiceFactory,
{
    /// The Configuration that the server will use. This can be static or a stream for hot reloading.
    configuration: ConfigurationKind,

    /// The Schema that the server will use. This can be static or a stream for hot reloading.
    schema: SchemaKind,

    /// A future that when resolved will shut down the server.
    shutdown: ShutdownKind,

    router_factory: RF,
}

/// A builder for an [`ApolloRouter`]
#[derive(Default)]
pub struct ApolloRouterBuilder<Factory = ()> {
    /// The Configuration that the server will use. This can be static or a stream for hot reloading.
    configuration: Option<ConfigurationKind>,

    /// The Schema that the server will use. This can be static or a stream for hot reloading.
    schema: Option<SchemaKind>,

    /// A future that when resolved will shut down the server.
    shutdown: Option<ShutdownKind>,

    router_factory: Factory,
}

impl ApolloRouterBuilder {
    pub fn configuration(mut self, configuration: ConfigurationKind) -> Self {
        self.configuration = Some(configuration);
        self
    }

    pub fn schema(mut self, schema: SchemaKind) -> Self {
        self.schema = Some(schema);
        self
    }

    pub fn shutdown(mut self, shutdown: ShutdownKind) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    /// Use a custom RouterServiceFactory
    pub fn with_factory<RF>(self, router_factory: RF) -> ApolloRouterBuilder<RF>
    where
        RF: RouterServiceFactory,
    {
        ApolloRouterBuilder {
            configuration: self.configuration,
            schema: self.schema,
            shutdown: self.shutdown,
            router_factory,
        }
    }

    pub fn build(self) -> ApolloRouter<YamlRouterServiceFactory> {
        ApolloRouter {
            configuration: self
                .configuration
                .expect("Configuration must be set on builder"),
            schema: self.schema.expect("Schema must be set on builder"),
            shutdown: self.shutdown.unwrap_or(ShutdownKind::CtrlC),
            router_factory: YamlRouterServiceFactory::default(),
        }
    }
}

impl<RF: RouterServiceFactory> ApolloRouterBuilder<RF> {
    pub fn build(self) -> ApolloRouter<RF> {
        ApolloRouter {
            configuration: self
                .configuration
                .expect("Configuration must be set on builder"),
            schema: self.schema.expect("Schema must be set on builder"),
            shutdown: self.shutdown.unwrap_or(ShutdownKind::CtrlC),
            router_factory: self.router_factory,
        }
    }
}

/// Messages that are broadcast across the app.
#[derive(Debug)]
enum Event {
    /// The configuration was updated.
    UpdateConfiguration(Box<Configuration>),

    /// There are no more updates to the configuration
    NoMoreConfiguration,

    /// The schema was updated.
    UpdateSchema(Box<graphql::Schema>),

    /// There are no more updates to the schema
    NoMoreSchema,

    /// The server should gracefully shutdown.
    Shutdown,
}

/// Public state that the client can be notified with via state listener
/// This is useful for waiting until the server is actually serving requests.
#[derive(Debug, PartialEq)]
pub enum State {
    /// The server is starting up.
    Startup,

    /// The server is running on a particular address.
    Running { address: ListenAddr, schema: String },

    /// The server has stopped.
    Stopped,

    /// The server has errored.
    Errored,
}

impl Display for State {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            State::Startup => write!(f, "startup"),
            State::Running { .. } => write!(f, "running"),
            State::Stopped => write!(f, "stopped"),
            State::Errored => write!(f, "errored"),
        }
    }
}

/// A handle that allows the client to await for various server events.
pub struct FederatedServerHandle {
    result: Pin<Box<dyn Future<Output = Result<(), FederatedServerError>> + Send>>,
    shutdown_sender: oneshot::Sender<()>,
    state_receiver: Option<mpsc::Receiver<State>>,
}

impl FederatedServerHandle {
    /// Wait until the server is ready and return the socket address that it is listening on.
    /// If the socket address has been configured to port zero the OS will choose the port.
    /// The socket address returned is the actual port that was bound.
    ///
    /// This method can only be called once, and is not designed for use in dynamic configuration
    /// scenarios.
    ///
    /// returns: Option<SocketAddr>
    pub async fn ready(&mut self) -> Option<ListenAddr> {
        self.state_receiver()
            .map(|state| {
                if let State::Running { address, .. } = state {
                    Some(address)
                } else {
                    None
                }
            })
            .filter(|socket| future::ready(socket != &None))
            .map(|s| s.unwrap())
            .next()
            .boxed()
            .await
    }

    /// Return a receiver of lifecycle events for the server. This method may only be called once.
    ///
    /// returns: mspc::Receiver<State>
    pub fn state_receiver(&mut self) -> mpsc::Receiver<State> {
        self.state_receiver.take().expect(
            "State listener has already been taken. 'ready' or 'state' may be called once only.",
        )
    }

    /// Trigger and wait until the server has shut down.
    ///
    /// returns: Result<(), FederatedServerError>
    pub async fn shutdown(mut self) -> Result<(), FederatedServerError> {
        self.maybe_close_state_receiver();
        if self.shutdown_sender.send(()).is_err() {
            tracing::error!("Failed to send shutdown event")
        }
        self.result.await
    }

    /// If the state receiver has not been set it must be closed otherwise it'll block the
    /// state machine from progressing.
    fn maybe_close_state_receiver(&mut self) {
        if let Some(mut state_receiver) = self.state_receiver.take() {
            state_receiver.close();
        }
    }

    /// State receiver that prints out basic lifecycle events.
    pub async fn with_default_state_receiver(&mut self) {
        self.state_receiver()
            .for_each(|state| {
                match state {
                    State::Startup => {
                        tracing::info!("starting Apollo Router")
                    }
                    State::Running { .. } => {}
                    State::Stopped => {
                        tracing::info!("stopped")
                    }
                    State::Errored => {
                        tracing::info!("stopped with error")
                    }
                }
                future::ready(())
            })
            .await
    }
}

impl Future for FederatedServerHandle {
    type Output = Result<(), FederatedServerError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.maybe_close_state_receiver();
        self.result.poll_unpin(cx)
    }
}

impl<RF> ApolloRouter<RF>
where
    RF: RouterServiceFactory,
{
    /// Start the federated server on a separate thread.
    ///
    /// The returned handle allows the user to await until the server is ready and shutdown.
    /// Alternatively the user can await on the server handle itself to wait for shutdown via the
    /// configured shutdown mechanism.
    ///
    /// returns: FederatedServerHandle
    ///
    pub fn serve(self) -> FederatedServerHandle {
        let (state_listener, state_receiver) = mpsc::channel::<State>(1);
        let server_factory = AxumHttpServerFactory::new();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let event_stream = Self::generate_event_stream(
            self.shutdown,
            self.configuration,
            self.schema,
            shutdown_receiver,
        );

        let state_machine =
            StateMachine::new(server_factory, Some(state_listener), self.router_factory);
        let result = spawn(async move { state_machine.process_events(event_stream).await })
            .map(|r| match r {
                Ok(Ok(ok)) => Ok(ok),
                Ok(Err(err)) => Err(err),
                Err(_err) => Err(FederatedServerError::StartupError),
            })
            .boxed();

        FederatedServerHandle {
            result,
            shutdown_sender,
            state_receiver: Some(state_receiver),
        }
    }

    /// Create the unified event stream.
    /// This merges all contributing streams and sets up shutdown handling.
    /// When a shutdown message is received no more events are emitted.
    fn generate_event_stream(
        shutdown: ShutdownKind,
        configuration: ConfigurationKind,
        schema: SchemaKind,
        shutdown_receiver: oneshot::Receiver<()>,
    ) -> impl Stream<Item = Event> {
        // Chain is required so that the final shutdown message is sent.
        let messages = stream::select_all(vec![
            shutdown.into_stream().boxed(),
            configuration.into_stream().boxed(),
            schema.into_stream().boxed(),
            shutdown_receiver.into_stream().map(|_| Shutdown).boxed(),
        ])
        .take_while(|msg| future::ready(!matches!(msg, Shutdown)))
        .chain(stream::iter(vec![Shutdown]))
        .boxed();
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::tests::{create_temp_file, write_and_flush};
    use serde_json::to_string_pretty;
    use std::env::temp_dir;
    use test_log::test;

    fn init_with_server() -> FederatedServerHandle {
        let configuration =
            serde_yaml::from_str::<Configuration>(include_str!("testdata/supergraph_config.yaml"))
                .unwrap();
        let schema: graphql::Schema = include_str!("testdata/supergraph.graphql").parse().unwrap();
        ApolloRouterBuilder::default()
            .configuration(ConfigurationKind::Instance(Box::new(configuration)))
            .schema(SchemaKind::Instance(Box::new(schema)))
            .build()
            .serve()
    }

    #[test(tokio::test)]
    async fn basic_request() {
        let mut server_handle = init_with_server();
        let listen_addr = server_handle.ready().await.expect("Server never ready");
        assert_federated_response(&listen_addr, r#"{ topProducts { name } }"#).await;
        server_handle.shutdown().await.expect("Could not shutdown");
    }

    async fn assert_federated_response(listen_addr: &ListenAddr, request: &str) {
        let request = graphql::Request::builder()
            .query(Some(request.to_string()))
            .build();
        let expected = query(listen_addr, &request).await.unwrap();

        let response = to_string_pretty(&expected).unwrap();
        assert!(!response.is_empty());
    }

    async fn query(
        listen_addr: &ListenAddr,
        request: &graphql::Request,
    ) -> Result<graphql::Response, graphql::FetchError> {
        Ok(reqwest::Client::new()
            .post(format!("{}/", listen_addr))
            .json(request)
            .send()
            .await
            .expect("couldn't send request")
            .json()
            .await
            .expect("couldn't deserialize into json"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_by_file_watching() {
        let (path, mut file) = create_temp_file();
        let contents = include_str!("testdata/supergraph_config.yaml");
        write_and_flush(&mut file, contents).await;
        let mut stream = ConfigurationKind::File {
            path,
            watch: true,
            delay: Some(Duration::from_millis(10)),
        }
        .into_stream()
        .boxed();

        // First update is guaranteed
        assert!(matches!(
            stream.next().await.unwrap(),
            UpdateConfiguration(_)
        ));

        // Modify the file and try again
        write_and_flush(&mut file, contents).await;
        assert!(matches!(
            stream.next().await.unwrap(),
            UpdateConfiguration(_)
        ));

        // This time write garbage, there should not be an update.
        write_and_flush(&mut file, ":garbage").await;
        assert!(stream.into_future().now_or_never().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_by_file_invalid() {
        let (path, mut file) = create_temp_file();
        write_and_flush(&mut file, "Garbage").await;
        let mut stream = ConfigurationKind::File {
            path,
            watch: true,
            delay: None,
        }
        .into_stream();

        // First update fails because the file is invalid.
        assert!(matches!(stream.next().await.unwrap(), NoMoreConfiguration));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_by_file_missing() {
        let mut stream = ConfigurationKind::File {
            path: temp_dir().join("does_not_exit"),
            watch: true,
            delay: None,
        }
        .into_stream();

        // First update fails because the file is invalid.
        assert!(matches!(stream.next().await.unwrap(), NoMoreConfiguration));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_by_file_no_watch() {
        let (path, mut file) = create_temp_file();
        let contents = include_str!("testdata/supergraph_config.yaml");
        write_and_flush(&mut file, contents).await;

        let mut stream = ConfigurationKind::File {
            path,
            watch: false,
            delay: None,
        }
        .into_stream();
        assert!(matches!(
            stream.next().await.unwrap(),
            UpdateConfiguration(_)
        ));
        assert!(matches!(stream.next().await.unwrap(), NoMoreConfiguration));
    }

    #[test(tokio::test)]
    async fn schema_by_file_watching() {
        let (path, mut file) = create_temp_file();
        let schema = include_str!("testdata/supergraph.graphql");
        write_and_flush(&mut file, schema).await;
        let mut stream = SchemaKind::File {
            path,
            watch: true,
            delay: Some(Duration::from_millis(10)),
        }
        .into_stream()
        .boxed();

        // First update is guaranteed
        assert!(matches!(stream.next().await.unwrap(), UpdateSchema(_)));

        // Modify the file and try again
        write_and_flush(&mut file, schema).await;
        assert!(matches!(stream.next().await.unwrap(), UpdateSchema(_)));
    }

    #[test(tokio::test)]
    async fn schema_by_file_missing() {
        let mut stream = SchemaKind::File {
            path: temp_dir().join("does_not_exit"),
            watch: true,
            delay: None,
        }
        .into_stream();

        // First update fails because the file is invalid.
        assert!(matches!(stream.next().await.unwrap(), NoMoreSchema));
    }

    #[test(tokio::test)]
    async fn schema_by_file_no_watch() {
        let (path, mut file) = create_temp_file();
        let schema = include_str!("testdata/supergraph.graphql");
        write_and_flush(&mut file, schema).await;

        let mut stream = SchemaKind::File {
            path,
            watch: false,
            delay: None,
        }
        .into_stream();
        assert!(matches!(stream.next().await.unwrap(), UpdateSchema(_)));
        assert!(matches!(stream.next().await.unwrap(), NoMoreSchema));
    }
}
