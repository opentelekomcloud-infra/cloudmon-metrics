//! cloudmon-metrics is an application that produces CloudMon metrics based on the configuration
//! for Grafana Json Datasource plugin
//!
use chrono::{DateTime, FixedOffset};
use evalexpr::*;
use new_string_template::template::Template;
use regex::Regex;
use reqwest::Error;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use std::{
    collections::{BTreeMap, HashMap},
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    extract::Extension, handler::Handler, http::StatusCode, response::IntoResponse, routing::get,
    Json, Router,
};
use reqwest::ClientBuilder;
use tokio::signal;
// use tracing::Span;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Use Jemalloc only for musl-64 bits platforms
#[cfg(all(target_env = "musl", target_pointer_width = "64"))]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[derive(Debug, Deserialize)]
struct Config {
    datasource: Datasource,
    server: ConfigServer,
    metric_templates: HashMap<String, BinaryMetricRawDef>,
    bin_metrics: HashMap<String, BinaryMetricDef>,
    expr_metrics: HashMap<String, ExpressionMetricDef>,
}

#[derive(Debug, Deserialize)]
struct ConfigServer {
    #[serde(default = "default_address")]
    address: String,
    #[serde(default = "default_port")]
    port: u16,
}

fn default_address() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    3000
}

fn default_timeout() -> u16 {
    5
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DatasourceType {
    Graphite,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CmpType {
    Lt,
    Gt,
    Eq,
}

#[derive(Debug, Deserialize)]
struct Datasource {
    url: String,
    // #[serde(rename(deserialize = "type"))]
    // ds_type: DatasourceType,
    #[serde(default = "default_timeout")]
    timeout: u16,
}

#[derive(Debug, Deserialize)]
struct BinaryMetricRawDef {
    query: String,
    op: CmpType,
    threshold: f32,
}

impl Default for BinaryMetricRawDef {
    fn default() -> Self {
        BinaryMetricRawDef {
            query: String::new(),
            op: CmpType::Lt,
            threshold: 0.0,
        }
    }
}

#[derive(Debug, Deserialize)]
struct BinaryMetricDef {
    query: Option<String>,
    op: Option<CmpType>,
    threshold: Option<f32>,
    template: Option<MetricTemplateRef>,
    #[serde(skip)]
    raw: BinaryMetricRawDef,
}

#[derive(Clone, Debug, Deserialize)]
struct MetricTemplateRef {
    name: String,
    vars: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ExpressionMetricDef {
    metrics: Vec<String>,
    expressions: Vec<MetricExpressionDef>,
}
#[derive(Debug, Deserialize)]
struct MetricExpressionDef {
    expression: String,
    weight: i32,
}

type MetricPoints = BTreeMap<u32, bool>;
#[derive(Debug, Deserialize, Serialize)]
struct MetricData {
    target: String,
    #[serde(rename(serialize = "datapoints"))]
    points: MetricPoints,
}

#[derive(Deserialize, Debug)]
struct GraphiteData {
    target: String,
    datapoints: Vec<(Option<f32>, u32)>,
}

struct AppState {
    config: Config,
    req_client: reqwest::Client,
}

#[derive(Deserialize, Debug)]
struct GrafanaJsonSearchRequest {
    target: String,
}

#[derive(Deserialize, Debug)]
struct GrafanaJsonQueryRequest {
    // #[serde(rename(deserialize = "startTime"))]
    // start_time: u64,
    // interval: String,
    // #[serde(rename(deserialize = "intervalMs"))]
    // interval_ms: u32,
    range: GrafanaJsonQueryRequestRange,
    // #[serde(rename(deserialize = "rangeRaw"))]
    // range_raw: GrafanaJsonQueryRequestRangeRaw,
    targets: Vec<GrafanaTarget>,
    #[serde(rename(deserialize = "maxDataPoints"))]
    max_data_points: u16,
}

#[derive(Debug, Deserialize)]
struct GrafanaJsonQueryRequestRange {
    from: String,
    to: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GrafanaJsonTargetType {
    Timeserie,
    Timeseries,
    Table,
}

impl GrafanaJsonTargetType {
    fn timeseries() -> Self {
        GrafanaJsonTargetType::Timeseries
    }
}

#[derive(Deserialize, Debug)]
struct GrafanaTarget {
    target: String,
    #[serde(rename(deserialize = "type"))]
    #[serde(default = "GrafanaJsonTargetType::timeseries")]
    target_type: GrafanaJsonTargetType,
    // #[serde(rename(deserialize = "refId"))]
    // ref_id: String,
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
enum GrafanaDataFrameMessage {
    Data {
        target: String,
        datapoints: Vec<(f32, u64)>,
    },
    Table {
        columns: Vec<GrafanaDataTableColumnType>,
        rows: Vec<Vec<serde_json::Value>>,
        #[serde(rename(serialize = "type"))]
        response_type: String,
    },
}

#[derive(Serialize, Debug)]
struct GrafanaDataTableColumnType {
    text: String,
    #[serde(rename(serialize = "type"))]
    column_type: GrafanaTableColumnType,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum GrafanaTableColumnType {
    Time,
    // String,
    Number,
}

fn alias_graphite_query(query: &str, alias: &str) -> String {
    format!("alias({},'{}')", query, alias)
}

/// Fetch required data from Graphite
async fn get_graphite_data(
    client: &reqwest::Client,
    url: &str,
    targets: HashMap<&str, String>,
    from: Option<DateTime<FixedOffset>>,
    to: Option<DateTime<FixedOffset>>,
    max_data_points: u16,
) -> Result<Vec<GraphiteData>, Error> {
    // Prepare vector of query parameters
    let mut query_params: Vec<(_, String)> = [
        ("format", "json".to_string()),
        // ("noNullPoints", "true".to_string()),
        ("maxDataPoints", max_data_points.to_string()),
    ]
    .into();
    if let Some(xfrom) = from {
        query_params.push(("from", xfrom.format("%H:%M_%Y%m%d").to_string()));
    }
    if let Some(xto) = to {
        query_params.push(("until", xto.format("%H:%M_%Y%m%d").to_string()));
    }
    query_params.extend(
        targets
            .iter()
            .map(|x| ("target", alias_graphite_query(x.1, x.0))),
    );
    let res = client
        .get(format!("{}/render", url))
        .query(&query_params)
        .send()
        .await?;
    tracing::debug!("Status: {}", res.status());
    tracing::debug!("Headers:\n{:#?}", res.headers());

    let data: Vec<GraphiteData> = res.json().await?;
    Ok(data)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "cloudmon=debug,tower_http=debug".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting cloudmon-metrics");

    let f = std::fs::File::open("config.yaml").expect("Could not open file.");
    let config: Config =
        process_config(serde_yaml::from_reader(f).expect("Could not read values."));

    let timeout = Duration::from_secs(config.datasource.timeout as u64);
    let req_client: reqwest::Client = ClientBuilder::new().timeout(timeout).build()?;

    let addr = SocketAddr::from((
        config.server.address.as_str().parse::<IpAddr>().unwrap(),
        config.server.port,
    ));
    let app_state = Arc::new(AppState { config, req_client });

    // build our application with a single route
    let app = Router::new()
        .route("/", get(|| async { "" }))
        .route("/query", get(handler_query).post(handler_query))
        .route("/search", get(handler_search).post(handler_search))
        .route("/annotations", get(|| async { "" }))
        .layer(
            ServiceBuilder::new()
                .layer(Extension(app_state))
                // `TraceLayer` is provided by tower-http so you have to add that as a dependency.
                // It provides good defaults but is also very customizable.
                //
                // See https://docs.rs/tower-http/0.1.1/tower_http/trace/index.html for more details.
                //        .layer(TraceLayer::new_for_http().on_request(
                //            |request: &axum::http::Request<_>, _span: &Span| {
                //                tracing::debug!(
                //                    "started {} {} {:?}",
                //                    request.method(),
                //                    request.uri().path(),
                //                    request
                //                )
                //            },
                //        ));
                .layer(TraceLayer::new_for_http()),
        );

    // add a fallback service for handling routes to unknown paths
    let app = app.fallback(handler_404.into_service());

    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    tracing::info!("Stopped cloudmon-metrics");
    Ok(())
}

/// Process config file to improve things we are going to search there
fn process_config(mut config: Config) -> Config {
    // We substitute $var syntax
    let custom_regex = Regex::new(r"(?mi)\$([^\.]+)").unwrap();
    for (_, metric_def) in config.bin_metrics.iter_mut() {
        if let Some(tmpl_ref) = metric_def.template.clone() {
            let tmpl = config.metric_templates.get(&tmpl_ref.name).unwrap();
            metric_def.raw.op = tmpl.op.clone();
            metric_def.raw.threshold = tmpl.threshold;
            let tmpl_query = Template::new(tmpl.query.clone()).with_regex(&custom_regex);
            let data = {
                let mut map: HashMap<&str, &str> = HashMap::new();
                for (k, v) in tmpl_ref.vars.iter() {
                    map.insert(k.as_str(), v.as_str());
                }
                map
            };
            metric_def.raw.query = tmpl_query.render(&data).unwrap();
        } else if let Some(val) = metric_def.query.clone() {
            metric_def.raw.query = val;
        }
        if let Some(val) = metric_def.op.clone() {
            metric_def.raw.op = val;
        }
        if let Some(val) = metric_def.threshold {
            metric_def.raw.threshold = val;
        }
    }
    config
}

/// Get metrics from TSDB
async fn get_metrics(
    state: &AppState,
    metric_names: Vec<String>,
    from: &str,
    to: &str,
    max_data_points: u16,
) -> Vec<MetricData> {
    let mut graphite_targets: HashMap<&str, String> = HashMap::new();
    // Construct target=>query map
    for metric in metric_names.iter() {
        match state.config.bin_metrics.get(metric) {
            Some(m) => {
                graphite_targets.insert(metric.as_str(), m.raw.query.clone());
            }
            _ => {}
        };
    }
    tracing::debug!("Requesting {:?}", graphite_targets);
    let raw_data: Vec<GraphiteData> = get_graphite_data(
        &state.req_client,
        &state.config.datasource.url.as_str(),
        graphite_targets,
        DateTime::parse_from_rfc3339(from).ok(),
        DateTime::parse_from_rfc3339(to).ok(),
        max_data_points,
    )
    .await
    .unwrap();
    let mut result: Vec<MetricData> = Vec::new();
    // tracing::debug!("Received following data: {:?}", raw_data);
    for data_element in raw_data.iter() {
        match state.config.bin_metrics.get(&data_element.target) {
            Some(metric) => {
                // log::debug!("Data element {:?}", data_element);
                let points: MetricPoints = BTreeMap::new();
                let mut md = MetricData {
                    target: data_element.target.clone(),
                    points: points,
                };
                for (val, ts) in data_element.datapoints.iter() {
                    let is_fulfilled = match *val {
                        Some(x) => match metric.raw.op {
                            CmpType::Lt => (x < metric.raw.threshold),
                            CmpType::Gt => (x > metric.raw.threshold),
                            CmpType::Eq => (x == metric.raw.threshold),
                        },
                        None => false,
                    };
                    md.points.insert(*ts, is_fulfilled);
                }
                result.push(md);
            }
            None => {
                tracing::warn!(
                    "DB Response contains unknown target: {}",
                    data_element.target
                );
            }
        }
    }
    // tracing::debug!("Summary data: {:?}", result);

    return result;
}

/// Return Tabular representation of the data requested
fn get_tab_data(data: Vec<MetricData>) -> BTreeMap<u64, HashMap<String, bool>> {
    let mut metrics_map: BTreeMap<u64, HashMap<String, bool>> = BTreeMap::new();
    for data in data.iter() {
        // Iterate over all fetched series
        for datapoint in data.points.iter() {
            // Iterate over datapoints of the series
            metrics_map
                .entry((*datapoint.0) as u64 * 1000)
                .or_insert(HashMap::new())
                .insert(data.target.clone(), *datapoint.1);
        }
    }
    return metrics_map;
}

/// Handler for the /query endpoint
///
/// It Processes request as described under
/// `<https://grafana.com/grafana/plugins/grafana-simple-json-datasource/>`,
/// queries data from Graphite and returns result.
async fn handler_query(
    Json(payload): Json<GrafanaJsonQueryRequest>,
    Extension(state): Extension<Arc<AppState>>,
) -> impl IntoResponse {
    tracing::debug!("Query with {:?}", payload);
    let mut response: Vec<serde_json::Value> = Vec::new();
    let mut metrics: Vec<String> = Vec::new();
    let mut expression_metrics: Vec<String> = Vec::new();
    let mut expression_mode: bool = false;
    let mut table_mode: bool = false;
    // Construct list of desired metrics
    for tgt in payload.targets.iter() {
        if "*".eq(&tgt.target) {
            metrics.extend(state.config.bin_metrics.keys().cloned());
        } else if tgt.target.ends_with("*") {
            tracing::debug!("* mode");
            let target = &tgt.target[0..tgt.target.len() - 1];
            tracing::debug!("Check with {}", target);
            metrics.extend(
                state
                    .config
                    .bin_metrics
                    .keys()
                    .filter(|x| x.starts_with(target))
                    .cloned(),
            );
        } else if state.config.bin_metrics.contains_key(&tgt.target) {
            metrics.push(tgt.target.clone());
        } else if state.config.expr_metrics.contains_key(&tgt.target) {
            expression_mode = true;
            if let Some(m) = state.config.expr_metrics.get(&tgt.target) {
                expression_metrics.push(tgt.target.clone());
                metrics.extend(m.metrics.iter().cloned())
            }
        }
        match tgt.target_type {
            GrafanaJsonTargetType::Table => table_mode = true,
            _ => {}
        }
    }
    tracing::debug!("requesting {:?}", metrics);
    let raw_data = get_metrics(
        &state,
        metrics,
        payload.range.from.as_str(),
        payload.range.to.as_str(),
        payload.max_data_points,
    )
    .await;
    if expression_mode {
        // In the expression mode we pre-process metrics
        let tab = get_tab_data(raw_data);
        // tracing::debug!("Tab data = {:?}", tab);
        let mut res: HashMap<String, Vec<(f32, u64)>> = HashMap::new();
        for (ts, ts_val) in tab.iter() {
            for target_hm in expression_metrics.iter() {
                if let Some(hm_config) = state.config.expr_metrics.get(target_hm) {
                    let result_metric_entry = res.entry(target_hm.into()).or_insert(Vec::new());
                    let mut context = HashMapContext::new();
                    for metric in hm_config.metrics.iter() {
                        let xval = match ts_val.get(metric) {
                            Some(&x) => x,
                            _ => false,
                        };
                        context.set_value(metric.into(), Value::from(xval)).unwrap();
                    }
                    let mut expression_res: f32 = 0.0;
                    for expr in hm_config.expressions.iter() {
                        if expr.weight as f32 <= expression_res {
                            //    continue;
                        }
                        match eval_boolean_with_context(expr.expression.as_str(), &context) {
                            Ok(m) => {
                                if m {
                                    expression_res = expr.weight as f32;
                                }
                            }
                            Err(e) => {
                                tracing::debug!("Error {:?}", e);
                            }
                        }
                    }
                    result_metric_entry.push((expression_res, *ts));
                }
            }
        }
        for (metric, vals) in res.iter() {
            let frame = GrafanaDataFrameMessage::Data {
                target: metric.into(),
                datapoints: vals.clone(),
            };
            response.push(json!(frame));
        }
    } else {
        // Iterate over result and convert them
        if !table_mode {
            for data in raw_data.iter() {
                let frame = GrafanaDataFrameMessage::Data {
                    target: data.target.clone(),
                    datapoints: data
                        .points
                        .iter()
                        .map(|x| (if *x.1 { 1.0 } else { 0.0 }, (*x.0) as u64 * 1000))
                        .collect(),
                };
                response.push(json!(frame));
            }
        } else {
            // Return data in the tabular mode. Are we interested in that?
            let mut cols: Vec<GrafanaDataTableColumnType> = vec![GrafanaDataTableColumnType {
                text: "time".into(),
                column_type: GrafanaTableColumnType::Time,
            }];
            let metrics: Vec<String> = raw_data.iter().map(|x| x.target.clone()).collect();
            cols.extend(metrics.iter().map(|x| GrafanaDataTableColumnType {
                text: x.clone(),
                column_type: GrafanaTableColumnType::Number,
            }));
            let mut rows: Vec<Vec<serde_json::Value>> = Vec::new();
            for (row_key, row_val) in get_tab_data(raw_data).iter() {
                let mut row_data: Vec<serde_json::Value> = Vec::new();
                row_data.push(json!(row_key));
                for metric in metrics.iter() {
                    row_data.push(json!(row_val.get(metric)));
                }
                rows.push(row_data);
            }
            let tab_response = GrafanaDataFrameMessage::Table {
                columns: cols,
                rows: rows,
                response_type: "table".into(),
            };
            return Json(vec![json!(tab_response)]);
        }
    }
    return Json(response);
}

/// Process /search request
async fn handler_search(
    Json(payload): Json<GrafanaJsonSearchRequest>,
    Extension(state): Extension<Arc<AppState>>,
) -> impl IntoResponse {
    tracing::debug!("Searching with {:?}", payload);
    let mut metrics: Vec<String> = vec!["*".to_string()];
    for (k, _) in state.config.bin_metrics.iter() {
        if k.starts_with(payload.target.as_str()) {
            tracing::debug!("Matching {}", k);
            metrics.push(k.clone());
        }
    }
    Json(metrics)
}

/// Return 404 error
async fn handler_404() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "nothing to see here")
}

/// Shutdown handler for the application
async fn shutdown_signal() {
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
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    println!("signal received, starting graceful shutdown");
}

#[cfg(test)]
mod test {
    use super::*;
    use mockito::{mock, Matcher};

    #[test]
    fn test_alias_graphite_query() {
        assert_eq!(alias_graphite_query("q", "n"), "alias(q,'n')");
    }

    macro_rules! aw {
        ($e:expr) => {
            tokio_test::block_on($e)
        };
    }

    #[test]
    fn test_get_graphite_data() {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .init();

        let mock = mockito::mock("GET", "/render")
            .expect(1)
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("target".into(), "alias(query,'alias')".into()),
                Matcher::UrlEncoded("from".into(), "00:00_20220101".into()),
                Matcher::UrlEncoded("until".into(), "00:00_20220201".into()),
                Matcher::UrlEncoded("maxDataPoints".into(), "15".into()),
            ]))
            .create();
        let timeout = Duration::from_secs(1 as u64);
        let _req_client: reqwest::Client = ClientBuilder::new().timeout(timeout).build().unwrap();

        let mut targets: HashMap<&str, String> = HashMap::new();
        targets.insert("alias", "query".to_string());
        let from: Option<DateTime<FixedOffset>> =
            DateTime::parse_from_rfc3339("2022-01-01T00:00:00+00:00").ok();
        let to: Option<DateTime<FixedOffset>> =
            DateTime::parse_from_rfc3339("2022-02-01T00:00:00+00:00").ok();
        let max_data_points: u16 = 15;
        let _res = aw!(get_graphite_data(
            &_req_client,
            format!("{}", mockito::server_url()).as_str(),
            targets,
            from,
            to,
            max_data_points,
        ));
        mock.assert();
    }
}
