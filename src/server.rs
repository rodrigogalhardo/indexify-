use std::{
    collections::HashMap,
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{anyhow, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json,
    Router,
};
use axum_otel_metrics::HttpMetricsLayerBuilder;
use axum_server::{tls_rustls::RustlsConfig, Handle};
use axum_tracing_opentelemetry::middleware::OtelAxumLayer;
use hyper::{header::CONTENT_TYPE, Method};
use indexify_internal_api as internal_api;
use indexify_proto::indexify_coordinator::{
    self,
    ContentStreamItem,
    ContentStreamRequest,
    GcTaskAcknowledgement,
    ListStateChangesRequest,
    ListTasksRequest,
};
use indexify_ui::Assets as UiAssets;
use internal_api::ContentOffset;
use mime::Mime;
use prometheus::Encoder;
use serde_json::json;
use tokio::{
    signal,
    sync::{mpsc, watch},
};
use tokio_stream::StreamExt;
use tonic::Streaming;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;
use utoipa::{
    openapi::{self, InfoBuilder, OpenApiBuilder},
    OpenApi,
    ToSchema,
};
use utoipa_rapidoc::RapiDoc;
use utoipa_redoc::{Redoc, Servable};
use utoipa_swagger_ui::SwaggerUi;

use crate::{
    api::*,
    blob_storage::{BlobStorage, ContentReader},
    coordinator_client::CoordinatorClient,
    data_manager::DataManager,
    metrics,
    server_config::ServerConfig,
    tls::build_mtls_config,
};

const DEFAULT_SEARCH_LIMIT: u64 = 5;

#[derive(Clone, Debug)]
pub struct NamespaceEndpointState {
    pub data_manager: Arc<DataManager>,
    pub coordinator_client: Arc<CoordinatorClient>,
    pub content_reader: Arc<ContentReader>,
    pub registry: Arc<prometheus::Registry>,
    pub metrics: Arc<metrics::server::Metrics>,
}

#[derive(OpenApi)]
#[openapi(
        paths(
            create_namespace,
            list_namespaces,
            list_extractors,
            list_executors,
            list_content,
            new_content_stream,
            get_content_metadata,
            list_state_changes,
            create_extraction_graph,
            list_extraction_graphs,
            link_extraction_graphs,
            extraction_graph_links,
            upload_file,
            list_tasks,
            get_content_tree_metadata,
            download_content,
            extraction_graph_analytics,
        ),
        components(
            schemas(IndexDistance,
                TextAddRequest, TextAdditionResponse, Text, IndexSearchResponse,
                DocumentFragment, ListIndexesResponse, ExtractorOutputSchema, Index, SearchRequest, ListNamespacesResponse, ListExtractorsResponse
            , ExtractorDescription, DataNamespace, ExtractionPolicy, ExtractionPolicyRequest, ExtractionPolicyResponse, Executor,
            MetadataResponse, ExtractedMetadata, ListExecutorsResponse, EmbeddingSchema, ExtractResponse, ExtractRequest,
            Feature, FeatureType, GetContentMetadataResponse, ListTasksResponse,  Task, ExtractionGraph,
            Content, ContentMetadata, ListContentResponse, GetNamespaceResponse, ExtractionPolicyResponse, ListTasks,
            ListExtractionGraphResponse, ExtractionGraphLink, ExtractionGraphRequest, ExtractionGraphResponse,
            AddGraphToContent, NewContentStreamResponse, ExtractionGraphAnalytics, TaskAnalytics,
            IngestRemoteFileResponse, IngestRemoteFile
        )
        ),
        tags(
            (name = "indexify", description = "Indexify API")
        )
    )]
struct ApiDoc;

pub struct Server {
    addr: SocketAddr,
    config: Arc<ServerConfig>,
}
impl Server {
    pub fn new(config: Arc<super::server_config::ServerConfig>) -> Result<Self> {
        let addr: SocketAddr = config.listen_addr_sock()?;
        Ok(Self { addr, config })
    }

    pub async fn run(&self, registry: Arc<prometheus::Registry>) -> Result<()> {
        // let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // TLS is set to true if the "tls" field is present in the config and the
        // TlsConfig "api" field is set to true
        let use_tls = self.config.tls.is_some() && self.config.tls.as_ref().unwrap().api;
        match use_tls {
            true => {
                match self.config.tls.as_ref().unwrap().ca_file {
                    Some(_) => info!("starting indexify server with mTLS enabled"),
                    None => info!("starting indexify server with TLS enabled. No CA file provided, so mTLS is disabled"),
                }
            }
            false => info!("starting indexify server with TLS disabled"),
        }
        let coordinator_client = Arc::new(CoordinatorClient::new(Arc::clone(&self.config)));
        let blob_storage = Arc::new(BlobStorage::new_with_config(
            self.config.blob_storage.clone(),
        ));
        let data_manager = Arc::new(DataManager::new(
            blob_storage.clone(),
            coordinator_client.clone(),
        ));
        let ingestion_server_id = nanoid::nanoid!(16);

        let namespace_endpoint_state = NamespaceEndpointState {
            data_manager: data_manager.clone(),
            coordinator_client: coordinator_client.clone(),
            content_reader: Arc::new(ContentReader::new(self.config.clone())),
            registry,
            metrics: Arc::new(crate::metrics::server::Metrics::new()),
        };
        let cors = CorsLayer::new()
            .allow_methods([Method::GET, Method::POST])
            .allow_origin(Any)
            .allow_headers([CONTENT_TYPE]);

        let metrics = HttpMetricsLayerBuilder::new().build();
        let app = Router::new()
            .merge(metrics.routes())
            .merge(SwaggerUi::new("/api-docs-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
            .merge(Redoc::with_url("/redoc", ApiDoc::openapi()))
            .merge(RapiDoc::new("/api-docs/openapi.json").path("/rapidoc"))
            .route("/", get(root))
            .route(
                "/namespaces/:namespace/openapi.json",
                get(namespace_open_api).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces/:namespace/extraction_graphs",
                post(create_extraction_graph).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces/:namespace/extraction_graphs",
                get(list_extraction_graphs).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces/:namespace/extraction_graphs/:extraction_graph/extract",
                post(upload_file).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces/:namespace/extraction_graphs/:extraction_graph/content",
                get(list_content).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces/:namespace/extraction_graphs/:extraction_graph/extraction_policies/:extraction_policy/tasks",
                get(list_tasks).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces/:namespace/extraction_graphs/:extraction_graph/analytics",
                get(extraction_graph_analytics).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces/:namespace/extraction_graphs/:extraction_graph/extraction_policies/:extraction_policy/new_content",
                get(new_content_stream).with_state(namespace_endpoint_state.clone()),
            )
            .route("/namespaces/:namespace/content/:content_id/download",
                get(download_content).with_state(namespace_endpoint_state.clone()))
            .route("/namespaces/:namespace/extraction_graphs/:extraction_graph/content/:content_id/extraction_policies/:extraction_policy",
                get(get_content_tree_metadata).with_state(namespace_endpoint_state.clone()))
            .route(
                "/namespaces/:namespace/extraction_graphs/:graph/links",
                post(link_extraction_graphs).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces/:namespace/extraction_graphs/:graph/links",
                get(extraction_graph_links).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces/:namespace/content/:content_id/metadata",
                get(get_content_metadata).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces",
                post(create_namespace).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/namespaces",
                get(list_namespaces).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/executors",
                get(list_executors).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/extractors",
                get(list_extractors).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/state_changes",
                get(list_state_changes).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/task_assignments",
                get(list_task_assignments).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/metrics/raft",
                get(get_raft_metrics_snapshot).with_state(namespace_endpoint_state.clone()),
            )
            .route(
                "/metrics/ingest",
                get(ingest_metrics).with_state(namespace_endpoint_state.clone()),
            )
            .route("/ui", get(ui_index_handler))
            .route("/ui/*rest", get(ui_handler))
            .layer(OtelAxumLayer::default())
            .layer(metrics)
            .layer(cors)
            .layer(DefaultBodyLimit::disable())
            .layer(tower_http::trace::TraceLayer::new_for_http());

        let handle = Handle::new();

        let handle_sh = handle.clone();
        tokio::spawn(async move {
            shutdown_signal(handle_sh).await;
            info!("received graceful shutdown signal. Telling tasks to shutdown");

            let _ = shutdown_tx.send(true);
        });

        // Create the default namespace. It's idempotent so we can keep trying
        while let Err(err) = data_manager
            .create_namespace(&DataNamespace {
                name: "default".to_string(),
            })
            .await
        {
            info!("failed to create default namespace: {}", err);
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

            if *shutdown_rx.borrow() {
                info!("shutting down create namespace loop");
                break;
            }
        }

        let handle = handle.clone();
        if use_tls {
            if let Some(tls_config) = self.config.tls.clone() {
                let config = build_mtls_config(&tls_config)?;
                let rustls_config = RustlsConfig::from_config(config);
                axum_server::tls_rustls::bind_rustls(self.addr, rustls_config)
                    .handle(handle)
                    .serve(app.into_make_service())
                    .await?;
            } else {
                return Err(anyhow!("TLS is enabled but no TLS config provided"));
            }
        } else {
            let handle = handle.clone();
            axum_server::bind(self.addr)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }

        Ok(())
    }
}

#[tracing::instrument]
async fn root() -> &'static str {
    "Indexify Server"
}

/// Create a new namespace
#[tracing::instrument]
#[axum::debug_handler]
#[utoipa::path(
    post,
    path = "/namespaces",
    request_body = DataNamespace,
    tag = "operations",
    responses(
        (status = 200, description = "Namespace created successfully"),
        (status = INTERNAL_SERVER_ERROR, description = "Unable to create namespace")
    ),
)]
async fn create_namespace(
    State(state): State<NamespaceEndpointState>,
    Json(payload): Json<DataNamespace>,
) -> Result<(), IndexifyAPIError> {
    state
        .data_manager
        .create_namespace(&payload)
        .await
        .map_err(|e| {
            IndexifyAPIError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to create namespace: {}", e),
            )
        })?;
    Ok(())
}

/// List all namespaces registered on the server
#[tracing::instrument]
#[utoipa::path(
    get,
    path = "/namespaces",
    tag = "operations",
    responses(
        (status = 200, description = "List of Data Namespaces registered on the server", body = ListNamespacesResponse),
        (status = INTERNAL_SERVER_ERROR, description = "Unable to sync namespace")
    ),
)]
async fn list_namespaces(
    State(state): State<NamespaceEndpointState>,
) -> Result<Json<ListNamespacesResponse>, IndexifyAPIError> {
    let namespaces = state.data_manager.list_namespaces().await.map_err(|e| {
        IndexifyAPIError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to list namespaces: {}", e),
        )
    })?;
    let data_namespaces: Vec<DataNamespace> = namespaces.into_iter().collect();
    Ok(Json(ListNamespacesResponse {
        namespaces: data_namespaces,
    }))
}

async fn namespace_open_api(
    Path(namespace): Path<String>,
    State(state): State<NamespaceEndpointState>,
) -> Result<Json<openapi::OpenApi>, IndexifyAPIError> {
    let extraction_graphs = state
        .data_manager
        .list_extraction_graphs(&namespace)
        .await
        .map_err(|e| {
            IndexifyAPIError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to get namespace: {}", e),
            )
        })?;
    let mut builder = OpenApiBuilder::default();
    let info = InfoBuilder::default()
        .title("Indexify API")
        .version(env!("CARGO_PKG_VERSION"))
        .build();
    builder = builder.info(info);
    let mut paths = utoipa::openapi::PathsBuilder::default();
    for eg in extraction_graphs {
        let response = utoipa::openapi::response::ResponseBuilder::default()
            .description("content uploaded successfully")
            .content(
                "application/json",
                utoipa::openapi::content::ContentBuilder::default()
                    .schema(utoipa::openapi::Ref::from_schema_name("UploadFileResponse"))
                    .build(),
            )
            .build();
        let operation = utoipa::openapi::path::OperationBuilder::default()
            .summary(Some(format!(
                "upload and extract content using graph '{}'",
                eg.name
            )))
            .request_body(Some(
                utoipa::openapi::request_body::RequestBodyBuilder::default()
                    .description(Some("content to be uploaded and extracted"))
                    .required(Some(openapi::Required::True))
                    .build(),
            ))
            .response("200", response)
            .response(
                "500",
                utoipa::openapi::response::ResponseBuilder::new()
                    .description("Internal Server Error")
                    .build(),
            )
            .parameter(
                openapi::path::ParameterBuilder::default()
                    .name("id")
                    .parameter_in(openapi::path::ParameterIn::Query)
                    .description(Some("id of the content"))
                    .required(openapi::Required::False)
                    .build(),
            )
            .description(eg.description);
        let item = utoipa::openapi::path::PathItemBuilder::default()
            .operation(utoipa::openapi::path::PathItemType::Post, operation.build())
            .build();
        paths = paths.path(
            format!(
                "/namespaces/{}/extraction_graphs/{}/extract",
                namespace, eg.name
            ),
            item,
        );
    }
    let components = utoipa::openapi::ComponentsBuilder::default()
        .schema_from::<UploadFileResponse>()
        .schema_from::<UploadFileQueryParams>()
        .build();
    builder = builder.paths(paths.build()).components(Some(components));
    let openapi = builder.build();
    Ok(Json(openapi))
}

/// Create a new extraction graph in the namespace
#[utoipa::path(
    post,
    path = "/namespaces/{namespace}/extraction_graphs",
    request_body(content = ExtractionGraphRequest, description = "Definition of extraction graph to create", content_type = "application/json"),
    tag = "ingestion",
    responses(
        (status = 200, description = "Extraction graph added successfully", body = ExtractionGraphResponse),
        (status = INTERNAL_SERVER_ERROR, description = "Unable to add extraction graph to namespace")
    ),
)]
#[axum::debug_handler]
async fn create_extraction_graph(
    // FIXME: this throws a 500 when the binding already exists
    // FIXME: also throws a 500 when the index name already exists
    headers: HeaderMap,
    Path(namespace): Path<String>,
    State(state): State<NamespaceEndpointState>,
    payload: String,
) -> Result<Json<ExtractionGraphResponse>, IndexifyAPIError> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    let payload: ExtractionGraphRequest = match content_type {
        Some("application/json") => serde_json::from_str(&payload).map_err(|_| {
            IndexifyAPIError::new(StatusCode::BAD_REQUEST, "Unable to parse json payload")
        })?,
        Some("application/x-yaml") => serde_yaml::from_str(&payload).map_err(|e| {
            IndexifyAPIError::new(
                StatusCode::BAD_REQUEST,
                format!("Unable to parse yaml payload {}", e).as_str(),
            )
        })?,
        _ => {
            return Err(IndexifyAPIError::new(
                StatusCode::BAD_REQUEST,
                "Unsupported content type",
            ))
        }
    };

    let indexes = state
        .data_manager
        .create_extraction_graph(&namespace, payload)
        .await
        .map_err(IndexifyAPIError::internal_error)?
        .into_iter()
        .collect();

    Ok(Json(ExtractionGraphResponse { indexes }))
}

/// Create a link with a given extraction graph
#[utoipa::path(
    post,
    path = "/namespace/{namespace}/extraction_graphs/{graph}/links",
    request_body = ExtractionGraphLink,
    tag = "operations",
    responses(
        (status = 200, description = "Extraction graphs linked successfully"),
        (status = INTERNAL_SERVER_ERROR, description = "Unable to link extraction graphs")
    ),
)]
#[axum::debug_handler]
async fn link_extraction_graphs(
    Path((namespace, graph_name)): Path<(String, String)>,
    State(state): State<NamespaceEndpointState>,
    Json(payload): Json<ExtractionGraphLink>,
) -> Result<(), IndexifyAPIError> {
    state
        .data_manager
        .link_extraction_graphs(namespace, graph_name, payload)
        .await
        .map_err(IndexifyAPIError::internal_error)
}

/// Get all the extraction graph links for a given extraction graph
#[utoipa::path(
    get,
    path = "/namespace/{namespace}/extraction_graphs/{graph}/links",
    tag = "operations",
    responses(
        (status = 200, description = "List of extraction graph links", body = Vec<ExtractionGraphLink>),
        (status = INTERNAL_SERVER_ERROR, description = "Unable to list links")
    ),
)]
#[axum::debug_handler]
async fn extraction_graph_links(
    Path((namespace, graph_name)): Path<(String, String)>,
    State(state): State<NamespaceEndpointState>,
) -> Result<Json<Vec<ExtractionGraphLink>>, IndexifyAPIError> {
    let res = state
        .data_manager
        .extraction_graph_links(namespace, graph_name)
        .await
        .map_err(IndexifyAPIError::internal_error)?;
    Ok(Json(res))
}

/// List all the content ingested into an extraction graph
#[tracing::instrument]
#[utoipa::path(
    get,
    path= "/namespaces/{namespace}/extraction_graphs/{extraction_graph}/content",
    params(
        ("namespace" = String, Path, description = "Namespace of the content"),
        ("extraction_graph" = String, Path, description = "Extraction graph name"),
        ("source" = Option<String>, Query, description = "Filter by source, either extraction policy name or 'ingestion' for top level content"),
        ("parent_id" = Option<String>, Query, description = "Filter by parent ID"),
        ("ingested_content_id" = Option<String>, Query, description = "Filter by ingested content ID"),
        ("labels_filter" = Option<Vec<String>>, Query, description = "Filter by labels. 
        Filter expression is the name of the label, comparison operator, and desired value, e.g. &labels_filter=key>=value. 
        Multiple expressions can be specified as separate query parameters."),
        ("start_id" = Option<String>, Query, description = "Pagination start ID. 
        Omit to start from beginning. To continue iteration, 
        specify id of the last content in the previous response"),
        ("limit" = Option<u32>, Query, description = "Maximum number of items to return"),
    ),
    tag = "retrieval",
    responses(
        (status = 200, description = "Lists the contents in the namespace", body = ListContentResponse),
        (status = BAD_REQUEST, description = "Unable to list contents")
    ),
)]
#[axum::debug_handler]
async fn list_content(
    Path((namespace, extraction_graph)): Path<(String, String)>,
    State(state): State<NamespaceEndpointState>,
    axum_extra::extract::Query(filter): axum_extra::extract::Query<super::api::ListContent>,
) -> Result<Json<ListContentResponse>, IndexifyAPIError> {
    let response = state
        .data_manager
        .list_content(
            &namespace,
            &extraction_graph,
            &filter.source,
            &filter.parent_id,
            &filter.ingested_content_id,
            &filter::LabelsFilter(filter.labels_filter),
            filter.restart_key.unwrap_or_default(),
            filter.limit.unwrap_or(10),
        )
        .await
        .map_err(IndexifyAPIError::internal_error)?;
    Ok(Json(response))
}

/// Get content metadata for a specific content id
#[tracing::instrument]
#[utoipa::path(
    get,
    path = "/namespaces/{namespace}/content/{content_id}/metadata",
    tag = "retrieval",
    responses(
        (status = 200, description = "Reads a specific content in the namespace", body = GetContentMetadataResponse),
        (status = BAD_REQUEST, description = "Unable to read content")
    ),
)]
#[axum::debug_handler]
async fn get_content_metadata(
    Path((namespace, content_id)): Path<(String, String)>,
    State(state): State<NamespaceEndpointState>,
) -> Result<Json<GetContentMetadataResponse>, IndexifyAPIError> {
    let content_list = state
        .data_manager
        .get_content_metadata(&namespace, vec![content_id])
        .await
        .map_err(IndexifyAPIError::internal_error)?;
    let content_metadata = content_list
        .first()
        .ok_or_else(|| IndexifyAPIError::new(StatusCode::NOT_FOUND, "content not found"))?;

    Ok(Json(GetContentMetadataResponse {
        content_metadata: content_metadata.clone(),
    }))
}

#[tracing::instrument]
#[utoipa::path(
    get,
    path ="/namespaces/{namespace}/content/{content_id}/wait",
    tag = "indexify",
    responses(
        (status = 200, description = "wait for all extraction tasks for content to complete"),
    ),
)]
#[axum::debug_handler]
async fn wait_content_extraction(
    Path((namespace, content_id)): Path<(String, String)>,
    State(state): State<NamespaceEndpointState>,
) -> Result<(), IndexifyAPIError> {
    state
        .data_manager
        .wait_content_extraction(&content_id)
        .await
        .map_err(IndexifyAPIError::internal_error)
}

/// Get extracted content metadata for a specific content id and extraction
/// graph
#[tracing::instrument]
#[utoipa::path(
    get,
    path = "/namespaces/:namespace/extraction_graphs/{extraction_graph}/content/{content_id}/extraction_policies/{extraction_policy}",
    tag = "retrieval",
    responses(
        (status = 200, description = "Gets a content tree rooted at a specific content id in the namespace"),
        (status = BAD_REQUEST, description = "Unable to read content tree")
    )
)]
#[axum::debug_handler]
async fn get_content_tree_metadata(
    Path((namespace, extraction_graph, content_id, extraction_policy)): Path<(
        String,
        String,
        String,
        String,
    )>,
    State(state): State<NamespaceEndpointState>,
) -> Result<Json<GetContentTreeMetadataResponse>, IndexifyAPIError> {
    let content_tree_metadata = state
        .data_manager
        .get_content_tree_metadata(
            &namespace,
            &content_id,
            &extraction_graph,
            &extraction_policy,
        )
        .await
        .map_err(IndexifyAPIError::internal_error)?;
    Ok(Json(GetContentTreeMetadataResponse {
        content_tree_metadata,
    }))
}

/// Download content with a given id
#[axum::debug_handler]
#[tracing::instrument]
#[utoipa::path(
    get,
    path = "/namespaces/{namespace}/content/{content_id}/download",
    tag = "retrieval",
    responses(
        (status = 200, description = "Downloads the bytes of the content", body = Vec<u8>),
        (status = BAD_REQUEST, description = "Unable to read content tree")
    )
)]
async fn download_content(
    Path((namespace, content_id)): Path<(String, String)>,
    State(state): State<NamespaceEndpointState>,
) -> Result<Response<Body>, IndexifyAPIError> {
    let content_list = state
        .data_manager
        .get_content_metadata(&namespace, vec![content_id])
        .await;
    let content_list = content_list.map_err(IndexifyAPIError::internal_error)?;
    let content_metadata = content_list
        .first()
        .ok_or(anyhow!("content not found"))
        .map_err(|e| IndexifyAPIError::not_found(&e.to_string()))?
        .clone();
    let mut resp_builder =
        Response::builder().header("Content-Type", content_metadata.mime_type.clone());
    if content_metadata.size > 0 {
        resp_builder = resp_builder.header("Content-Length", content_metadata.size);
    }

    let storage_reader = state.content_reader.get(&content_metadata.storage_url);
    let content_stream = storage_reader
        .get(&content_metadata.storage_url)
        .await
        .map_err(|e| IndexifyAPIError::new(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    resp_builder
        .body(Body::from_stream(content_stream))
        .map_err(|e| IndexifyAPIError::new(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
}

#[derive(Debug, serde::Deserialize, ToSchema)]
struct UploadFileQueryParams {
    id: Option<String>,
}

/// List all extraction graphs in a namespace
#[tracing::instrument]
#[utoipa::path(
    get,
    path = "/namespaces/{namespace}/extraction_graphs",
    tag = "ingestion",
    responses(
        (status = 200, description = "List of Extraction Graphs registered on the server", body = ListExtractionGraphResponse),
        (status = BAD_REQUEST, description = "Unable to list extraction graphs")
    ),
)]
async fn list_extraction_graphs(
    Path(namespace): Path<String>,
    State(state): State<NamespaceEndpointState>,
) -> Result<Json<ListExtractionGraphResponse>, IndexifyAPIError> {
    let graphs = state
        .data_manager
        .list_extraction_graphs(&namespace)
        .await
        .map_err(|e| {
            IndexifyAPIError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to list extraction graphs: {}", e),
            )
        })?;
    Ok(Json(ListExtractionGraphResponse {
        extraction_graphs: graphs,
    }))
}

#[allow(dead_code)]
#[derive(ToSchema)]
struct UploadType {
    labels: Option<HashMap<String, serde_json::Value>>,
    #[schema(format = "binary")]
    file: String,
}

async fn upload_file_inner(
    state: &NamespaceEndpointState,
    namespace: String,
    extraction_graph: String,
    params: UploadFileQueryParams,
    mut files: Multipart,
    url: &mut String,
) -> Result<Json<UploadFileResponse>, IndexifyAPIError> {
    let mut labels: HashMap<String, serde_json::Value> = HashMap::new();

    let id = params.id.clone().unwrap_or_else(DataManager::make_id);
    if !DataManager::is_hex_string(&id) {
        return Err(IndexifyAPIError::new(
            StatusCode::BAD_REQUEST,
            "Invalid ID format, ID must be a hex string",
        ));
    }

    //  check if the id already exists for content metadata
    let retrieved_content = state
        .data_manager
        .get_content_metadata(&namespace, vec![id.clone()])
        .await
        .map_err(IndexifyAPIError::internal_error)?;
    if !retrieved_content.is_empty() {
        return Err(IndexifyAPIError::new(
            StatusCode::BAD_REQUEST,
            "content with the provided id already exists",
        ));
    }

    let mut write_result = None;
    let mut ext = String::new();
    while let Some(field) = files.next_field().await.unwrap() {
        if let Some(name) = field.file_name() {
            if write_result.is_some() {
                return Err(IndexifyAPIError::new(
                    StatusCode::BAD_REQUEST,
                    "multiple files provided",
                ));
            }
            info!("user provided file name = {:?}", name);
            ext = std::path::Path::new(&name)
                .extension()
                .unwrap_or_default()
                .to_str()
                .unwrap_or_default()
                .to_string();
            let name = nanoid::nanoid!(16);
            let name = if !ext.is_empty() {
                format!("{}.{}", name, ext)
            } else {
                name
            };
            info!("writing to blob store, file name = {:?}", name);

            let stream = field.map(|res| res.map_err(|err| anyhow::anyhow!(err)));
            write_result = Some(
                state
                    .data_manager
                    .write_stream(&namespace, stream, Some(&name))
                    .await
                    .map_err(|e| {
                        IndexifyAPIError::new(
                            StatusCode::BAD_REQUEST,
                            &format!("failed to upload file: {}", e),
                        )
                    })?,
            );
            *url = write_result.as_ref().unwrap().url.clone();
        } else if let Some(name) = field.name() {
            if name != "labels" {
                continue;
            }

            labels = serde_json::from_str(&field.text().await.unwrap()).map_err(|e| {
                IndexifyAPIError::new(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to upload file: {}", e),
                )
            })?;
        }
    }
    if let Some(write_result) = write_result {
        let content_mime = labels.get("mime_type").and_then(|v| v.as_str());
        let content_mime = content_mime.map(Mime::from_str).transpose().map_err(|e| {
            IndexifyAPIError::new(
                StatusCode::BAD_REQUEST,
                &format!("invalid mime type: {}", e),
            )
        })?;
        let content_mime =
            content_mime.unwrap_or(mime_guess::from_ext(&ext).first_or_octet_stream());
        let labels = internal_api::utils::convert_map_serde_to_prost_json(labels).map_err(|e| {
            IndexifyAPIError::new(StatusCode::BAD_REQUEST, &format!("invalid labels: {}", e))
        })?;
        let current_ts_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| IndexifyAPIError::new(StatusCode::INTERNAL_SERVER_ERROR, "invalid time"))?
            .as_secs();
        let size_bytes = write_result.size_bytes;
        let content_metadata = indexify_coordinator::ContentMetadata {
            id: id.clone(),
            file_name: write_result.file_name,
            storage_url: write_result.url,
            parent_id: "".to_string(),
            root_content_id: "".to_string(),
            created_at: current_ts_secs as i64,
            mime: content_mime.to_string(),
            namespace,
            labels,
            source: "".to_string(),
            size_bytes: write_result.size_bytes,
            hash: write_result.hash,
            extraction_policy_ids: HashMap::new(),
            extraction_graph_names: vec![extraction_graph],
            extracted_metadata: json!({}).to_string(),
        };
        state
            .data_manager
            .create_content_metadata(content_metadata)
            .await
            .map_err(|e| {
                IndexifyAPIError::new(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to create content for file: {}", e),
                )
            })?;
        state.metrics.node_content_uploads.add(1, &[]);
        state
            .metrics
            .node_content_bytes_uploaded
            .add(size_bytes, &[]);
        Ok(Json(UploadFileResponse { content_id: id }))
    } else {
        Err(IndexifyAPIError::new(
            StatusCode::BAD_REQUEST,
            "no file provided",
        ))
    }
}

/// Upload a file to an extraction graph in a namespace
#[tracing::instrument(skip(state))]
#[utoipa::path(
    post,
    path = "/namespaces/{namespace}/extraction_graphs/{extraction_graph}/extract",
    params(
        ("namespace" = String, Path, description = "Namespace of the content"),
        ("extraction_graph" = String, Path, description = "Extraction graph name"),
        ("id" = Option<String>, Query, description = "id of content to create, if not provided a random id will be generated"),
    ),
    request_body(content_type = "multipart/form-data", content = inline(UploadType)),
    tag = "ingestion",
    responses(
        (status = 200, description = "Uploads a file to the namespace"),
        (status = BAD_REQUEST, description = "Unable to upload file")
    ),
)]
#[axum::debug_handler]
async fn upload_file(
    Path((namespace, extraction_graph)): Path<(String, String)>,
    State(state): State<NamespaceEndpointState>,
    Query(params): Query<UploadFileQueryParams>,
    files: Multipart,
) -> Result<Json<UploadFileResponse>, IndexifyAPIError> {
    let mut url = String::new();
    let res = upload_file_inner(&state, namespace, extraction_graph, params, files, &mut url).await;
    if res.is_err() && !url.is_empty() {
        let _ = state.data_manager.delete_file(&url).await.map_err(|e| {
            tracing::error!("failed to delete file: {}", e);
        });
    }
    res
}

async fn get_new_content_stream(
    state: &NamespaceEndpointState,
    namespace: String,
    extraction_graph: String,
    extraction_policy: String,
    start: NewContentStreamStart,
) -> Result<Streaming<ContentStreamItem>> {
    let mut client = state.data_manager.get_coordinator_client().await?;
    let stream = client
        .content_stream(ContentStreamRequest {
            change_offset: match start {
                NewContentStreamStart::FromLast => u64::MAX,
                NewContentStreamStart::FromOffset(offset) => offset.0,
            },
            namespace,
            extraction_graph,
            extraction_policy,
        })
        .await?;
    Ok(stream.into_inner())
}

#[utoipa::path(
    get,
    path = "/namespaces/{namespace}/extraction_graphs/{extraction_graph}/extraction_policies/{extraction_policy}/content",
    params(
        ("namespace" = String, Path, description = "Namespace of the content"),
        ("extraction_graph" = String, Path, description = "Extraction graph name"),
        ("extraction_policy" = String, Path, description = "Extraction policy name"),
        ("offset" = Option<u64>, Query, description = "Offset to start from, if not provided will start from last")
    ),
    tag = "ingestion",
    responses(
        (status = 200, description = "Started stream of new content", body = NewContentStreamResponse),
        (status = BAD_REQUEST, description = "Unable to start new content stream")
    ),
)]
async fn new_content_stream(
    Path((namespace, extraction_graph, extraction_policy)): Path<(String, String, String)>,
    State(state): State<NamespaceEndpointState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, IndexifyAPIError> {
    let offset = params.get("offset").and_then(|s| s.parse().ok());
    let start = match offset {
        Some(offset) => NewContentStreamStart::FromOffset(ContentOffset(offset)),
        None => NewContentStreamStart::FromLast,
    };
    let stream = get_new_content_stream(
        &state,
        namespace,
        extraction_graph,
        extraction_policy,
        start,
    )
    .await
    .map_err(|e| IndexifyAPIError::new(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let stream = stream.map(|item| match item {
        Ok(item) => {
            let item: Result<NewContentStreamResponse, _> = item.try_into();
            match item {
                Ok(item) => axum::response::sse::Event::default().json_data(item),
                Err(e) => {
                    tracing::error!("error in new content stream: {}", e);
                    Err(axum::Error::new(e))
                }
            }
        }
        Err(e) => {
            tracing::error!("error in new content stream: {}", e);
            Err(axum::Error::new(e))
        }
    });
    Ok(axum::response::Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

/// List all executors running extractors in the cluster
#[tracing::instrument]
#[utoipa::path(
    get,
    path = "/executors",
    tag = "operations",
    responses(
        (status = 200, description = "List of currently running executors", body = ListExecutorsResponse),
        (status = INTERNAL_SERVER_ERROR, description = "Unable to load executors")
    ),
)]
#[axum::debug_handler]
async fn list_executors(
    State(_state): State<NamespaceEndpointState>,
) -> Result<Json<ListExecutorsResponse>, IndexifyAPIError> {
    Ok(Json(ListExecutorsResponse { executors: vec![] }))
}

/// List all extractors available in the cluster
#[tracing::instrument]
#[utoipa::path(
    get,
    path = "/extractors",
    tag = "operations",
    responses(
        (status = 200, description = "List of extractors available", body = ListExtractorsResponse),
        (status = INTERNAL_SERVER_ERROR, description = "Unable to search index")
    ),
)]
#[axum::debug_handler]
async fn list_extractors(
    State(state): State<NamespaceEndpointState>,
) -> Result<Json<ListExtractorsResponse>, IndexifyAPIError> {
    let extractors = state
        .data_manager
        .list_extractors()
        .await
        .map_err(IndexifyAPIError::internal_error)?
        .into_iter()
        .collect();
    Ok(Json(ListExtractorsResponse { extractors }))
}

/// List the state changes in the system
#[utoipa::path(
    get,
    path = "/state_changes",
    tag = "operations",
    responses(
        (status = 200, description = "Extract content from an extractor", body = ExtractResponse),
        (status = INTERNAL_SERVER_ERROR, description = "Unable to search index")
    ),
)]
#[axum::debug_handler]
async fn list_state_changes(
    State(state): State<NamespaceEndpointState>,
    Query(_query): Query<ListStateChanges>,
) -> Result<Json<ListStateChangesResponse>, IndexifyAPIError> {
    let state_changes = state
        .coordinator_client
        .get()
        .await
        .map_err(IndexifyAPIError::internal_error)?
        .list_state_changes(ListStateChangesRequest {})
        .await
        .map_err(|e| IndexifyAPIError::new(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .into_inner()
        .changes;

    let state_changes: Vec<indexify_internal_api::StateChange> = state_changes
        .into_iter()
        .map(|c| c.try_into())
        .filter_map(|c| c.ok())
        .collect();

    Ok(Json(ListStateChangesResponse { state_changes }))
}

/// Get Analytics for an extraction graph
#[tracing::instrument]
#[utoipa::path(
    get,
    path = "/namespaces/{namespace}/extraction_graphs/{extraction_graph}/analytics",
    params(
        ("namespace" = String, Path, description = "Namespace of the content"),
        ("extraction_graph" = String, Path, description = "Extraction graph name"),
    ),
    tag = "operations",
    responses(
        (status = 200, description = "Return Analytics", body = ExtractionGraphAnalytics),
        (status = INTERNAL_SERVER_ERROR, description = "Unable to list tasks")
    ),
)]
async fn extraction_graph_analytics(
    Path((namespace, extraction_graph)): Path<(String, String)>,
    State(state): State<NamespaceEndpointState>,
    Query(query): Query<ListTasks>,
) -> Result<Json<ExtractionGraphAnalytics>, IndexifyAPIError> {
    let resp = state
        .coordinator_client
        .get_extraction_graph_analytics(&namespace, &extraction_graph)
        .await
        .map_err(|e| IndexifyAPIError::new(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;

    Ok(Json(resp.into()))
}

/// List Tasks generated for a given content and a given extraction policy
#[tracing::instrument]
#[utoipa::path(
    get,
    path = "/namespaces/{namespace}/extraction_graphs/{extraction_graph}/extraction_policies/{extraction_policy}/tasks",
    params(
        ("namespace" = String, Path, description = "Namespace of the content"),
        ("extraction_graph" = String, Path, description = "Extraction graph name"),
        ("extraction_policy" = String, Path, description = "Extraction policy name"),
        ("content_id" = Option<String>, Query, description = "Filter by content ID"),
        ("outcome" = Option<String>, Query, description = "Filter by task outcome"),
        ("start_id" = Option<String>, Query, description = "Pagination start ID. 
        Omit to start from beginning. To continue iteration, 
        specify id of the last task in the previous response"),
        ("limit" = Option<u32>, Query, description = "Maximum number of items to return"),
    ),
    tag = "operations",
    responses(
        (status = 200, description = "Lists tasks", body = ListTasksResponse),
        (status = INTERNAL_SERVER_ERROR, description = "Unable to list tasks")
    ),
)]
async fn list_tasks(
    Path((namespace, extraction_graph, extraction_policy)): Path<(String, String, String)>,
    State(state): State<NamespaceEndpointState>,
    Query(query): Query<ListTasks>,
) -> Result<Json<ListTasksResponse>, IndexifyAPIError> {
    let outcome: indexify_coordinator::TaskOutcomeFilter = query.outcome.into();
    let resp = state
        .coordinator_client
        .get()
        .await
        .map_err(IndexifyAPIError::internal_error)?
        .list_tasks(ListTasksRequest {
            extraction_graph,
            namespace,
            extraction_policy,
            start_id: query.start_id.unwrap_or_default(),
            limit: query.limit.unwrap_or(10),
            content_id: query.content_id.unwrap_or_default(),
            outcome: outcome as i32,
        })
        .await
        .map_err(|e| IndexifyAPIError::new(StatusCode::INTERNAL_SERVER_ERROR, e.message()))?
        .into_inner();
    Ok(Json(resp.try_into()?))
}

#[axum::debug_handler]
async fn list_task_assignments(
    State(namespace_endpoint): State<NamespaceEndpointState>,
) -> Result<Json<TaskAssignments>, IndexifyAPIError> {
    let response = namespace_endpoint
        .coordinator_client
        .all_task_assignments()
        .await
        .map_err(IndexifyAPIError::internal_error)?;
    Ok(Json(response))
}

#[axum::debug_handler]
#[tracing::instrument]
async fn get_raft_metrics_snapshot(
    State(state): State<NamespaceEndpointState>,
) -> Result<Json<RaftMetricsSnapshotResponse>, IndexifyAPIError> {
    state.coordinator_client.get_raft_metrics_snapshot().await
}

#[axum::debug_handler]
#[tracing::instrument]
async fn ingest_metrics(
    State(state): State<NamespaceEndpointState>,
) -> Result<Response<Body>, IndexifyAPIError> {
    let metric_families = state.registry.gather();
    let mut buffer = vec![];
    let encoder = prometheus::TextEncoder::new();
    encoder.encode(&metric_families, &mut buffer).map_err(|_| {
        IndexifyAPIError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encode metrics",
        )
    })?;

    Ok(Response::new(Body::from(buffer)))
}

#[axum::debug_handler]
#[tracing::instrument(skip_all)]
async fn ui_index_handler() -> impl IntoResponse {
    let content = UiAssets::get("index.html").unwrap();
    (
        [(hyper::header::CONTENT_TYPE, content.metadata.mimetype())],
        content.data,
    )
        .into_response()
}

#[axum::debug_handler]
#[tracing::instrument(skip_all)]
async fn ui_handler(Path(url): Path<String>) -> impl IntoResponse {
    let content = UiAssets::get(url.trim_start_matches('/'))
        .unwrap_or_else(|| UiAssets::get("index.html").unwrap());
    (
        [(hyper::header::CONTENT_TYPE, content.metadata.mimetype())],
        content.data,
    )
        .into_response()
}

#[tracing::instrument]
pub async fn shutdown_signal(handle: Handle) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
        },
        _ = terminate => {
        },
    }
    handle.shutdown();
    info!("signal received, shutting down server gracefully");
}
