use actix_web::HttpResponse;
use actix_web::http::StatusCode;
use actix_web::{App, HttpServer, Responder, Result, post};
use actix_web_lab::sse::{self, Sse};
use falkordb::FalkorClientBuilder;
use falkordb::FalkorConnectionInfo;
use futures_util::StreamExt;
use genai::ModelIden;
use genai::resolver::AuthData;
use genai::resolver::AuthResolver;
use moka::sync::Cache;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use tokio::sync::mpsc;
use tracing_subscriber::fmt;
use utoipa::OpenApi;
use utoipa::ToSchema;
use utoipa_swagger_ui::SwaggerUi;

// Macro for functions returning ()
macro_rules! send {
    ($tx:expr, $progress:expr) => {
        match serde_json::to_string(&$progress) {
            Ok(json) => {
                let event = sse::Event::Data(sse::Data::new(json));
                if $tx.send(event).await.is_err() {
                    tracing::warn!("Client disconnected, stopping stream");
                    return;
                }
            }
            Err(e) => {
                tracing::error!("Failed to serialize progress update: {}", e);
                return;
            }
        }
    };
}

// Macro for functions returning Option<T>
macro_rules! send_option {
    ($tx:expr, $progress:expr) => {
        match serde_json::to_string(&$progress) {
            Ok(json) => {
                let event = sse::Event::Data(sse::Data::new(json));
                if $tx.send(event).await.is_err() {
                    tracing::warn!("Client disconnected, stopping stream");
                    return None;
                }
            }
            Err(e) => {
                tracing::error!("Failed to serialize progress update: {}", e);
                return None;
            }
        }
    };
}

// Macro for functions returning Result<T, ()>
macro_rules! send_result {
    ($tx:expr, $progress:expr) => {
        match serde_json::to_string(&$progress) {
            Ok(json) => {
                let event = sse::Event::Data(sse::Data::new(json));
                if $tx.send(event).await.is_err() {
                    tracing::warn!("Client disconnected, stopping stream");
                    return Err(());
                }
            }
            Err(e) => {
                tracing::error!("Failed to serialize progress update: {}", e);
                return Err(());
            }
        }
    };
}

// Macro for functions returning Result<T, ()> - same name, different internal marker
macro_rules! try_send {
    ($tx:expr, $progress:expr) => {
        match serde_json::to_string(&$progress) {
            Ok(json) => {
                let event = sse::Event::Data(sse::Data::new(json));
                if $tx.send(event).await.is_err() {
                    tracing::warn!("Client disconnected, stopping stream");
                    return Err(());
                }
            }
            Err(e) => {
                tracing::error!("Failed to serialize progress update: {}", e);
                return Err(());
            }
        }
    };
}

// Macro for functions returning Result<String, Box<dyn Error>>
macro_rules! try_send_boxed {
    ($tx:expr, $progress:expr) => {
        match serde_json::to_string(&$progress) {
            Ok(json) => {
                let event = sse::Event::Data(sse::Data::new(json));
                if $tx.send(event).await.is_err() {
                    tracing::warn!("Client disconnected, stopping stream");
                    return Err("Client disconnected".into());
                }
            }
            Err(e) => {
                tracing::error!("Failed to serialize progress update: {}", e);
                return Err(format!("Serialization failed: {}", e).into());
            }
        }
    };
}

// Macro for functions returning String (returns empty string on error)
macro_rules! send_or_empty {
    ($tx:expr, $progress:expr) => {
        match serde_json::to_string(&$progress) {
            Ok(json) => {
                let event = sse::Event::Data(sse::Data::new(json));
                if $tx.send(event).await.is_err() {
                    tracing::warn!("Client disconnected, stopping stream");
                    return String::new();
                }
            }
            Err(e) => {
                tracing::error!("Failed to serialize progress update: {}", e);
                return String::new();
            }
        }
    };
}

mod chat;
mod error;
mod formatter;
mod mcp;
mod schema;
mod template;

use chat::{ChatMessage, ChatRequest, ChatRole};
use formatter::format_query_records;
use mcp::run_mcp_server;
use template::TemplateEngine;

use crate::schema::discovery::Schema;

// Configuration structure for default values from .env file
#[derive(Debug, Clone)]
struct AppConfig {
    falkordb_connection: String,
    default_model: Option<String>,
    default_key: Option<String>,
    schema_cache: Cache<String, String>,
}

static APP_CONFIG: OnceLock<AppConfig> = OnceLock::new();

impl AppConfig {
    fn load() -> Self {
        // Load .env file if it exists, but don't fail if it doesn't
        let env_loaded = dotenvy::dotenv().is_ok();
        let falkordb_connection =
            std::env::var("FALKORDB_CONNECTION").unwrap_or_else(|_| "falkor://127.0.0.1:6379".to_string());
        let default_model = std::env::var("DEFAULT_MODEL").ok();
        let default_key = std::env::var("DEFAULT_KEY").ok();
        let schema_cache = Cache::new(100);

        tracing::info!(
            "Loaded configuration - env_file_loaded: {}, default_model: {:?}",
            env_loaded,
            default_model
        );

        Self {
            falkordb_connection,
            default_model,
            default_key,
            schema_cache,
        }
    }

    fn get() -> &'static Self {
        APP_CONFIG.get_or_init(Self::load)
    }

    /// Check if MCP server should be started based on configuration completeness
    #[allow(clippy::cognitive_complexity)]
    fn should_start_mcp_server(&self) -> bool {
        // Check if both required environment variables are available
        let has_model = self.default_model.is_some();
        let has_key = self.default_key.is_some();

        let should_start = has_model && has_key;

        if should_start {
            tracing::info!("MCP server will be started: both DEFAULT_MODEL and DEFAULT_KEY are configured");
        } else if !has_model {
            tracing::warn!("MCP server not started: DEFAULT_MODEL not set");
        } else if !has_key {
            tracing::warn!("MCP server not started: DEFAULT_KEY not set");
        }

        should_start
    }
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
struct TextToCypherRequest {
    graph_name: String,
    chat_request: ChatRequest,
    model: Option<String>,
    key: Option<String>,
}

impl std::fmt::Debug for TextToCypherRequest {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        let mut debug_struct = f.debug_struct("TextToCypherRequest");
        debug_struct
            .field("graph_name", &self.graph_name)
            .field("chat_request", &self.chat_request)
            .field("model", &self.model);

        if self.key.is_some() {
            debug_struct.field("key", &"***");
        }

        debug_struct.finish()
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
enum Progress {
    Status(String),
    Schema(String),
    CypherQuery(String),
    CypherResult(String),
    ModelOutputChunk(String),
    Result(String),
    Error(String),
}

fn process_clear_schema_cache(graph_name: &str) {
    tracing::info!("Clearing schema cache for graph: {graph_name}");
    let cache = AppConfig::get().schema_cache.clone();
    cache.invalidate(graph_name);
}

#[utoipa::path(
    post,
    path = "/clear_schema_cache/{graph_name}",
    params(
        ("graph_name" = String, Path, description = "Name of the graph to clear from cache")
    ),
    responses(
        (status = 200, description = "Schema cache cleared successfully")
    )
)]
#[post("/clear_schema_cache/{graph_name}")]
async fn clear_schema_cache(graph_name: actix_web::web::Path<String>) -> impl Responder {
    let graph_name = graph_name.into_inner();
    tracing::info!("Clearing schema cache for graph: {}", graph_name);
    process_clear_schema_cache(&graph_name);
    HttpResponse::new(StatusCode::OK)
}

#[utoipa::path(
    post,
    path = "/text_to_cypher",
    request_body = TextToCypherRequest,
    responses(
        (status = 200, description = "Stream text to Cypher conversion progress", content_type = "text/event-stream")
    )
)]
#[post("/text_to_cypher")]
async fn text_to_cypher(req: actix_web::web::Json<TextToCypherRequest>) -> Result<impl Responder, actix_web::Error> {
    let mut request = req.into_inner();
    let config = AppConfig::get();

    // Apply defaults from .env file if values are not provided
    if request.model.is_none() {
        request.model.clone_from(&config.default_model);
    }

    if request.key.is_none() {
        request.key.clone_from(&config.default_key);
    }

    let (tx, rx) = mpsc::channel(100);

    // Ensure we have a model after applying defaults
    if request.model.is_none() {
        // Send error via SSE instead of returning HTTP error
        tokio::spawn(async move {
            let error_event = sse::Event::Data(sse::Data::new(
                serde_json::to_string(&Progress::Error(
                    "Model must be provided either in request or as DEFAULT_MODEL in .env file".to_string(),
                ))
                .unwrap_or_else(|_| r#"{"Error":"Serialization failed"}"#.to_string()),
            ));
            let _ = tx.send(error_event).await;
        });
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, actix_web::Error>);
        return Ok(Sse::from_stream(stream));
    }

    let model = request.model.as_ref().unwrap(); // Safe to unwrap after the check above

    let client = request.key.as_ref().map_or_else(genai::Client::default, |key| {
        let key = key.clone(); // Clone the key for use in the closure
        let auth_resolver = AuthResolver::from_resolver_fn(
            move |model_iden: ModelIden| -> Result<Option<AuthData>, genai::resolver::Error> {
                let ModelIden {
                    adapter_kind,
                    model_name,
                } = model_iden;
                tracing::info!("Using custom auth provider for {adapter_kind} (model: {model_name})");

                // Use the provided key instead of reading from environment
                Ok(Some(AuthData::from_single(key.clone())))
            },
        );
        genai::Client::builder().with_auth_resolver(auth_resolver).build()
    });

    // Handle service target resolution errors via SSE
    let service_target = match client.resolve_service_target(model).await {
        Ok(target) => target,
        Err(e) => {
            // Send error via SSE instead of returning HTTP error
            tokio::spawn(async move {
                let error_event = sse::Event::Data(sse::Data::new(
                    serde_json::to_string(&Progress::Error(format!("Failed to resolve service target: {e}")))
                        .unwrap_or_else(|_| r#"{"Error":"Serialization failed"}"#.to_string()),
                ));
                let _ = tx.send(error_event).await;
            });
            let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, actix_web::Error>);
            return Ok(Sse::from_stream(stream));
        }
    };

    tokio::spawn(async move {
        process_text_to_cypher_request(request, client, service_target, tx).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, actix_web::Error>);

    Ok(Sse::from_stream(stream))
}

#[allow(clippy::cognitive_complexity)]
async fn process_text_to_cypher_request(
    request: TextToCypherRequest,
    client: genai::Client,
    service_target: genai::ServiceTarget,
    tx: mpsc::Sender<sse::Event>,
) {
    tracing::info!("Processing text to Cypher request: {request:?}");

    let model = request
        .model
        .as_ref()
        .expect("Model should be available after applying defaults");

    // Step 1: Send processing status
    send_processing_status(&request, &service_target, &tx).await;

    // Step 2: Discover schema
    let Some(schema) = get_or_discover_schema(&request.graph_name, &tx).await else {
        send!(tx, Progress::Error("Failed to discover schema".to_string()));
        return;
    };

    // Step 3: Generate and execute cypher query
    let Some(query) = generate_cypher_query(&request, &schema, &client, model, &tx).await else {
        return;
    };

    // Step 4: Execute the query and get results
    let Ok(query_result) = execute_cypher_query(&query, &request.graph_name, &tx).await else {
        return;
    };

    // Step 5: Generate final answer using AI
    generate_final_answer(&request, &query, &query_result, &client, model, &tx).await;
}

async fn get_or_discover_schema(
    graph_name: &str,
    tx: &mpsc::Sender<sse::Event>,
) -> Option<String> {
    let cache = AppConfig::get().schema_cache.clone();
    let schema = match cache.get(graph_name) {
        Some(schema) => schema,
        None => match discover_and_send_schema(graph_name, tx).await {
            Ok(schema) => schema,
            Err(()) => return None,
        },
    };
    send_option!(tx, Progress::Schema(schema.clone()));
    cache.insert(graph_name.to_string(), schema.clone());
    Some(schema.clone())
}

#[allow(clippy::cognitive_complexity)]
async fn generate_cypher_query(
    request: &TextToCypherRequest,
    schema: &str,
    client: &genai::Client,
    model: &str,
    tx: &mpsc::Sender<sse::Event>,
) -> Option<String> {
    send_option!(
        tx,
        Progress::Status(String::from("Generating Cypher query using schema ..."))
    );

    let genai_chat_request = generate_create_cypher_query_chat_request(&request.chat_request, schema);
    let query = execute_chat(client, model, genai_chat_request, tx).await;

    if query.trim().is_empty() {
        tracing::warn!("No query generated from AI model");
        send_option!(tx, Progress::Error("No valid query was generated".to_string()));
        return None;
    }

    let clean_query = query.replace('\n', " ").replace("```", "").trim().to_string();
    send_option!(tx, Progress::CypherQuery(clean_query.clone()));
    Some(clean_query)
}

#[allow(clippy::cognitive_complexity)]
async fn execute_cypher_query(
    query: &str,
    graph_name: &str,
    tx: &mpsc::Sender<sse::Event>,
) -> Result<String, ()> {
    send_result!(tx, Progress::Status(String::from("Executing Cypher query...")));
    tracing::info!("Executing Cypher Query: {}", query);

    match execute_query(query, graph_name, tx).await {
        Ok(result) => {
            tracing::info!("Query executed successfully, result: {}", result);
            send_result!(tx, Progress::CypherResult(result.clone()));
            Ok(result)
        }
        Err(e) => {
            tracing::error!("Query execution failed: {}", e);
            send_result!(tx, Progress::Error(format!("Query execution failed: {e}")));
            Err(())
        }
    }
}

async fn generate_final_answer(
    request: &TextToCypherRequest,
    query: &str,
    query_result: &str,
    client: &genai::Client,
    model: &str,
    tx: &mpsc::Sender<sse::Event>,
) {
    send!(
        tx,
        Progress::Status(String::from(
            "Generating answer from chat history and Cypher output using AI model..."
        ))
    );

    let genai_chat_request = generate_answer_chat_request(&request.chat_request, query, query_result);
    execute_chat_stream(client, model, genai_chat_request, tx).await;
}

async fn execute_query(
    query: &str,
    graph_name: &str,
    tx: &mpsc::Sender<sse::Event>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let connection_info: FalkorConnectionInfo = AppConfig::get()
        .falkordb_connection
        .as_str()
        .try_into()
        .map_err(|e| format!("Invalid connection info: {e}"))?;

    let client = FalkorClientBuilder::new_async()
        .with_connection_info(connection_info)
        .build()
        .await
        .map_err(|e| format!("Failed to build client: {e}"))?;

    let graph_name = graph_name.to_string();
    let query = query.to_string();

    // Run the FalkorDB operations in a blocking context
    let result = tokio::task::spawn_blocking(move || execute_query_blocking(&client, &graph_name, &query))
        .await
        .map_err(|e| format!("Failed to execute blocking task: {e}"))?;

    let formatted_result = match result {
        Ok(records) => format_query_records(&records),
        Err(e) => {
            let error_msg = format!("Query execution failed: {e}");
            try_send_boxed!(tx, Progress::Error(error_msg.clone()));
            return Err(error_msg.into());
        }
    };

    Ok(formatted_result)
}

fn execute_query_blocking(
    client: &falkordb::FalkorAsyncClient,
    graph_name: &str,
    query: &str,
) -> Result<Vec<Vec<falkordb::FalkorValue>>, Box<dyn std::error::Error + Send + Sync>> {
    // Create a new Tokio runtime for this blocking operation
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("Failed to create runtime: {e}"))?;

    rt.block_on(async {
        let mut graph = client.select_graph(graph_name);
        let query_result = graph
            .ro_query(query)
            .execute()
            .await
            .map_err(|e| format!("Query execution failed: {e}"))?;

        let mut records = Vec::new();
        for record in query_result.data {
            records.push(record);
        }
        Ok(records)
    })
}

fn generate_create_cypher_query_chat_request(
    chat_request: &ChatRequest,
    ontology: &str,
) -> genai::chat::ChatRequest {
    let mut chat_req = genai::chat::ChatRequest::default();
    for (index, message) in chat_request.messages.iter().enumerate() {
        let is_last_user_message = index == chat_request.messages.len() - 1 && message.role == ChatRole::User;

        let genai_message = match message.role {
            ChatRole::User => {
                if is_last_user_message {
                    // Special processing for the last user message
                    let processed_content = process_last_user_message(&message.content);
                    genai::chat::ChatMessage::user(processed_content)
                } else {
                    genai::chat::ChatMessage::user(message.content.clone())
                }
            }
            ChatRole::Assistant => genai::chat::ChatMessage::assistant(message.content.clone()),
            ChatRole::System => genai::chat::ChatMessage::system(message.content.clone()),
        };

        chat_req = chat_req.append_message(genai_message);
    }

    chat_req = chat_req.with_system(TemplateEngine::render_system_prompt(ontology).unwrap_or_else(|e| {
        tracing::error!("Failed to load system prompt template: {}", e);
        format!("Generate OpenCypher statements using this ontology: {ontology}")
    }));

    // Pretty print the chat request as JSON for logging
    if let Ok(pretty_json) = serde_json::to_string_pretty(&chat_req) {
        tracing::info!("Generated genai chat request:\n{}", pretty_json);
    } else {
        tracing::info!("Generated genai chat request: {:?}", chat_req);
    }
    chat_req
}

fn generate_answer_chat_request(
    chat_request: &ChatRequest,
    cypher_query: &str,
    cypher_result: &str,
) -> genai::chat::ChatRequest {
    let mut chat_req = genai::chat::ChatRequest::default();
    for (index, message) in chat_request.messages.iter().enumerate() {
        let is_last_user_message = index == chat_request.messages.len() - 1 && message.role == ChatRole::User;

        let genai_message = match message.role {
            ChatRole::User => {
                if is_last_user_message {
                    // Special processing for the last user message
                    let processed_content = process_last_request_prompt(&message.content, cypher_query, cypher_result);
                    genai::chat::ChatMessage::user(processed_content)
                } else {
                    genai::chat::ChatMessage::user(message.content.clone())
                }
            }
            ChatRole::Assistant => genai::chat::ChatMessage::assistant(message.content.clone()),
            ChatRole::System => genai::chat::ChatMessage::system(message.content.clone()),
        };

        chat_req = chat_req.append_message(genai_message);
    }

    // Pretty print the chat request as JSON for logging
    if let Ok(pretty_json) = serde_json::to_string_pretty(&chat_req) {
        tracing::info!("Generated genai chat request:\n{}", pretty_json);
    } else {
        tracing::info!("Generated genai chat request: {:?}", chat_req);
    }
    chat_req
}

fn process_last_request_prompt(
    content: &str,
    cypher_query: &str,
    cypher_result: &str,
) -> String {
    TemplateEngine::render_last_request_prompt(content, cypher_query, cypher_result).unwrap_or_else(|e| {
        tracing::error!("Failed to load last_request_prompt template: {}", e);
        format!("Generate an answer for: {content}")
    })
}

#[derive(OpenApi)]
#[openapi(
    paths(text_to_cypher, clear_schema_cache),
    components(schemas(
        TextToCypherRequest,
        Progress,
        ChatRequest,
        ChatMessage,
        ChatRole,
        error::ErrorResponse
    ))
)]
struct ApiDoc;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    fmt().with_max_level(tracing::Level::INFO).init();

    // Initialize configuration from .env file
    let config = AppConfig::get();

    tracing::info!("Starting server at http://localhost:8080/swagger-ui/");

    // Conditionally start MCP server based on configuration
    let mcp_handle = if config.should_start_mcp_server() {
        tracing::info!("Starting MCP server at http://localhost:8081/sse");
        Some(tokio::spawn(async {
            tracing::info!("MCP server started");
            if let Err(e) = run_mcp_server().await {
                tracing::error!("MCP server error: {}", e);
            }
        }))
    } else {
        tracing::info!("MCP server not started - check .env file configuration");
        None
    };

    // Start the HTTP server with Swagger UI at /swagger-ui/
    // OpenAPI documentation will be available at /api-doc/openapi.json
    // Swagger UI will be accessible at:
    // http://localhost:8080/swagger-ui/

    let http_server = HttpServer::new(|| {
        App::new()
            .service(text_to_cypher)
            .service(clear_schema_cache)
            .service(SwaggerUi::new("/swagger-ui/{_:.*}").url("/api-doc/openapi.json", ApiDoc::openapi()))
    })
    .bind(("0.0.0.0", 8080))?
    .run();

    // Run server(s) concurrently
    if let Some(mcp_handle) = mcp_handle {
        // Run both HTTP and MCP servers
        tokio::select! {
            result = http_server => {
                tracing::info!("HTTP server stopped");
                result
            }
            _ = mcp_handle => {
                tracing::info!("MCP server stopped");
                Ok(())
            }
        }
    } else {
        // Run only HTTP server
        tracing::info!("Running HTTP server only");
        let result = http_server.await;
        tracing::info!("HTTP server stopped");
        result
    }
}

async fn discover_graph_schema(graph_name: &str) -> Schema {
    let connection_info: FalkorConnectionInfo = AppConfig::get()
        .falkordb_connection
        .as_str()
        .try_into()
        .expect("Invalid connection info");

    let client = FalkorClientBuilder::new_async()
        .with_connection_info(connection_info)
        .build()
        .await
        .expect("Failed to build client");

    // Select the specified graph
    let mut graph = client.select_graph(graph_name);
    let schema = Schema::discover_from_graph(&mut graph, 100)
        .await
        .expect("Failed to discover schema from graph");

    // Print the discovered schema
    tracing::info!("Discovered schema: {schema}");
    schema
}

fn process_last_user_message(question: &str) -> String {
    TemplateEngine::render_user_prompt(question).unwrap_or_else(|e| {
        tracing::error!("Failed to load user prompt template: {}", e);
        format!("Generate an OpenCypher statement for: {question}")
    })
}

#[allow(clippy::cognitive_complexity)]
async fn discover_and_send_schema(
    graph_name: &str,
    tx: &mpsc::Sender<sse::Event>,
) -> Result<String, ()> {
    try_send!(
        tx,
        Progress::Status(format!("Discovering schema for graph: {graph_name}"))
    );

    let schema = discover_graph_schema(graph_name).await;

    // Serialize and handle errors inline
    let Ok(json_schema) = serde_json::to_string(&schema) else {
        tracing::error!("Failed to serialize schema to JSON");
        try_send!(tx, Progress::Error("Failed to serialize schema".to_string()));
        return Err(());
    };

    tracing::info!("Discovered schema: {}", json_schema);
    Ok(json_schema)
}

async fn send_processing_status(
    request: &TextToCypherRequest,
    service_target: &genai::ServiceTarget,
    tx: &mpsc::Sender<sse::Event>,
) {
    let adapter_kind = service_target.model.adapter_kind;
    let model_name = request.model.as_deref().unwrap_or("unknown");
    send!(
        tx,
        Progress::Status(format!(
            "Processing query for graph: {} using model: {} ({:?})",
            request.graph_name, model_name, adapter_kind
        ))
    );
}

async fn execute_chat(
    client: &genai::Client,
    model: &str,
    genai_chat_request: genai::chat::ChatRequest,
    tx: &mpsc::Sender<sse::Event>,
) -> String {
    // Make the actual request to the model
    let chat_response = match client.exec_chat(model, genai_chat_request, None).await {
        Ok(response) => response,
        Err(e) => {
            let error_update = Progress::Error(format!("Chat request failed: {e}"));
            send_or_empty!(tx, error_update);
            return String::from("NO ANSWER");
        }
    };
    chat_response
        .content_text_into_string()
        .unwrap_or_else(|| String::from("NO ANSWER"))
}

async fn execute_chat_stream(
    client: &genai::Client,
    model: &str,
    genai_chat_request: genai::chat::ChatRequest,
    tx: &mpsc::Sender<sse::Event>,
) -> String {
    // Make the actual request to the model
    let chat_response = match client.exec_chat_stream(model, genai_chat_request, None).await {
        Ok(response) => response,
        Err(e) => {
            let error_update = Progress::Error(format!("Chat request failed: {e}"));
            send_or_empty!(tx, error_update);
            return String::new();
        }
    };

    process_chat_stream(chat_response, tx).await
}

#[allow(clippy::cognitive_complexity)]
async fn process_chat_stream(
    chat_response: genai::chat::ChatStreamResponse,
    tx: &mpsc::Sender<sse::Event>,
) -> String {
    let mut answer = String::new();

    // Extract the response stream
    let mut stream = chat_response.stream;
    while let Some(Ok(stream_event)) = stream.next().await {
        match stream_event {
            genai::chat::ChatStreamEvent::Start => {}
            genai::chat::ChatStreamEvent::Chunk(chunk) => {
                answer.push_str(&chunk.content);
                send_or_empty!(tx, Progress::ModelOutputChunk(chunk.content));
            }
            genai::chat::ChatStreamEvent::ReasoningChunk(_chunk) => {}
            genai::chat::ChatStreamEvent::End(_end_event) => {}
        }
    }

    tracing::info!("Final answer: {}", answer);
    send_or_empty!(tx, Progress::Result(answer.clone()));
    answer
}
