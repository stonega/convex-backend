#![feature(let_chains)]
#![feature(try_blocks)]
#![feature(iterator_try_collect)]
#![feature(coroutines)]
#![feature(exhaustive_patterns)]

use std::{
    sync::Arc,
    time::Duration,
};

use ::authentication::{
    access_token_auth::NullAccessTokenAuth,
    application_auth::ApplicationAuth,
};
use ::storage::{
    LocalDirStorage,
    StorageUseCase,
};
use application::{
    api::ApplicationApi,
    log_visibility::AllowLogging,
    Application,
    QueryCache,
};
use common::{
    http::{
        fetch::ProxiedFetchClient,
        RouteMapper,
    },
    knobs::{
        ACTION_USER_TIMEOUT,
        UDF_CACHE_MAX_SIZE,
    },
    log_streaming::NoopLogSender,
    pause::PauseClient,
    persistence::Persistence,
    types::{
        ConvexOrigin,
        ConvexSite,
    },
};
use config::LocalConfig;
use database::{
    Database,
    ShutdownSignal,
};
use events::usage::NoOpUsageEventLogger;
use file_storage::{
    FileStorage,
    TransactionalFileStorage,
};
use function_runner::{
    in_process_function_runner::InProcessFunctionRunner,
    server::InstanceStorage,
    FunctionRunner,
};
use model::{
    initialize_application_system_tables,
    virtual_system_mapping,
};
use node_executor::{
    local::LocalNodeExecutor,
    Actions,
};
use runtime::prod::ProdRuntime;
use search::{
    searcher::InProcessSearcher,
    Searcher,
    SegmentTermMetadataFetcher,
};
use serde::Serialize;

pub mod admin;
mod app_metrics;
mod args_structs;
pub mod authentication;
pub mod config;
pub mod custom_headers;
pub mod dashboard;
pub mod deploy_config;
pub mod deploy_config2;
pub mod environment_variables;
pub mod http_actions;
pub mod logs;
pub mod node_action_callbacks;
pub mod parse;
pub mod persistence;
pub mod proxy;
pub mod public_api;
pub mod router;
pub mod scheduling;
pub mod schema;
pub mod snapshot_export;
pub mod snapshot_import;
pub mod storage;
pub mod subs;
#[cfg(test)]
mod test_helpers;

pub const MAX_CONCURRENT_REQUESTS: usize = 128;

#[derive(Clone)]
pub struct LocalAppState {
    // Origin for the server (e.g. http://127.0.0.1:3210, https://demo.convex.cloud)
    pub origin: ConvexOrigin,
    // Origin for the corresponding convex.site (where we serve HTTP) (e.g. http://127.0.0.1:8001, https://crazy-giraffe-123.convex.site)
    pub site_origin: ConvexSite,
    // Name of the instance. (e.g. crazy-giraffe-123)
    pub instance_name: String,
    pub application: Application<ProdRuntime>,
    pub zombify_rx: async_broadcast::Receiver<()>,
}

impl LocalAppState {
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.application.shutdown().await?;

        Ok(())
    }
}

// Contains state needed to serve most http routes. Similar to LocalAppState,
// but uses ApplicationApi instead of Application, which allows it to be used
// in both Backend and Usher.
#[derive(Clone)]
pub struct RouterState {
    pub api: Arc<dyn ApplicationApi>,
    pub runtime: ProdRuntime,
}

#[derive(Serialize)]
pub struct EmptyResponse {}

pub async fn make_app(
    runtime: ProdRuntime,
    config: LocalConfig,
    persistence: Arc<dyn Persistence>,
    zombify_rx: async_broadcast::Receiver<()>,
    preempt_tx: ShutdownSignal,
) -> anyhow::Result<LocalAppState> {
    let key_broker = config.key_broker()?;
    let in_process_searcher = InProcessSearcher::new(runtime.clone()).await?;
    let searcher: Arc<dyn Searcher> = Arc::new(in_process_searcher.clone());
    // TODO(CX-6572) Separate `SegmentMetadataFetcher` from `SearcherImpl`
    let segment_metadata_fetcher: Arc<dyn SegmentTermMetadataFetcher> =
        Arc::new(in_process_searcher);
    let database = Database::load(
        persistence.clone(),
        runtime.clone(),
        searcher.clone(),
        preempt_tx,
        virtual_system_mapping(),
        Arc::new(NoOpUsageEventLogger),
    )
    .await?;
    initialize_application_system_tables(&database).await?;
    let files_storage = Arc::new(LocalDirStorage::for_use_case(
        runtime.clone(),
        &config.storage_dir().to_string_lossy(),
        StorageUseCase::Files,
    )?);
    let modules_storage = Arc::new(LocalDirStorage::for_use_case(
        runtime.clone(),
        &config.storage_dir().to_string_lossy(),
        StorageUseCase::Modules,
    )?);
    let search_storage = Arc::new(LocalDirStorage::for_use_case(
        runtime.clone(),
        &config.storage_dir().to_string_lossy(),
        StorageUseCase::SearchIndexes,
    )?);
    // Search storage needs to be set for Database to be fully initialized
    database.set_search_storage(search_storage.clone());
    let exports_storage = Arc::new(LocalDirStorage::for_use_case(
        runtime.clone(),
        &config.storage_dir().to_string_lossy(),
        StorageUseCase::Exports,
    )?);
    let snapshot_imports_storage = Arc::new(LocalDirStorage::for_use_case(
        runtime.clone(),
        &config.storage_dir().to_string_lossy(),
        StorageUseCase::SnapshotImports,
    )?);

    let file_storage = FileStorage {
        transactional_file_storage: TransactionalFileStorage::new(
            runtime.clone(),
            files_storage.clone(),
            config.convex_origin_url(),
        ),
        database: database.clone(),
    };

    let node_process_timeout = *ACTION_USER_TIMEOUT + Duration::from_secs(5);
    let node_executor = Arc::new(LocalNodeExecutor::new(node_process_timeout)?);
    let actions = Actions::new(
        node_executor,
        config.convex_origin_url(),
        *ACTION_USER_TIMEOUT,
        runtime.clone(),
    );

    #[cfg(not(debug_assertions))]
    if config.convex_http_proxy.is_none() {
        tracing::warn!(
            "Running without a proxy in release mode -- UDF `fetch` requests are unrestricted!"
        );
    }
    let fetch_client = Arc::new(ProxiedFetchClient::new(
        config.convex_http_proxy.clone(),
        config.name(),
    ));
    let function_runner: Arc<dyn FunctionRunner<ProdRuntime>> = Arc::new(
        InProcessFunctionRunner::new(
            config.name().clone(),
            config.secret()?,
            config.convex_origin_url(),
            runtime.clone(),
            persistence.reader(),
            InstanceStorage {
                files_storage: files_storage.clone(),
                modules_storage: modules_storage.clone(),
            },
            database.clone(),
            fetch_client,
        )
        .await?,
    );
    let application = Application::new(
        runtime.clone(),
        database.clone(),
        file_storage.clone(),
        files_storage.clone(),
        modules_storage.clone(),
        search_storage.clone(),
        exports_storage.clone(),
        snapshot_imports_storage.clone(),
        database.usage_counter(),
        key_broker.clone(),
        config.name(),
        function_runner,
        config.convex_origin_url(),
        config.convex_site_url(),
        searcher.clone(),
        segment_metadata_fetcher.clone(),
        persistence,
        actions,
        Arc::new(NoopLogSender),
        Arc::new(AllowLogging),
        PauseClient::new(),
        PauseClient::new(),
        Arc::new(ApplicationAuth::new(
            key_broker.clone(),
            Arc::new(NullAccessTokenAuth),
        )),
        QueryCache::new(*UDF_CACHE_MAX_SIZE),
    )
    .await?;

    let origin = config.convex_origin_url();
    let instance_name = config.name().clone();

    let app_state = LocalAppState {
        origin,
        site_origin: config.convex_site_url(),
        instance_name,
        application,
        zombify_rx,
    };

    Ok(app_state)
}

#[derive(Clone)]
pub struct HttpActionRouteMapper;

impl RouteMapper for HttpActionRouteMapper {
    fn map_route(&self, route: String) -> String {
        // Backend can receive arbitrary HTTP requests, so group all of these
        // under one tag.
        if route.starts_with("/http/") {
            "/http/:user_http_action".into()
        } else {
            route
        }
    }
}
