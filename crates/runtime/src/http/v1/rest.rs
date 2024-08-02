use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query};
use axum::http::status;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::info;

use app::App;

use crate::datafusion::DataFusion;
use crate::http::v1::datasets::MessageResponse;
use crate::http::v1::sql_to_http_response;

#[derive(Debug, Deserialize)]
pub struct QueryParams {
    columns: Option<String>,
    #[serde(flatten)]
    filters: HashMap<String, String>,
    order: Option<String>,
    limit: Option<u32>,
    page: Option<u32>,
}

fn failure(status: status::StatusCode, reason: String) -> Response {
    (status, Json(MessageResponse { message: reason })).into_response()
}

pub(crate) async fn get(
    Extension(app): Extension<Arc<RwLock<Option<App>>>>,
    Extension(df): Extension<Arc<DataFusion>>,
    Path(endpoint_name): Path<String>,
    Query(params): Query<QueryParams>,
) -> Response {
    let app_lock = app.read().await;
    let Some(readable_app) = &*app_lock else {
        return (status::StatusCode::INTERNAL_SERVER_ERROR).into_response();
    };

    let endpoint = match readable_app
        .endpoints
        .iter()
        .find(|d| d.name.to_lowercase() == endpoint_name.to_lowercase())
    {
        Some(endpoint) => endpoint.clone(),
        None => {
            return failure(
                status::StatusCode::NOT_FOUND,
                format!("Source {endpoint_name} not found"),
            )
        }
    };

    let dataset_name = endpoint.dataset.clone();

    let mut columns = Vec::new();
    let mut group_by = Vec::new();

    let table = df.ctx.table(dataset_name.clone()).await;
    let schema = match table {
        Ok(table) => table.schema().clone(),
        Err(e) => {
            tracing::error!("{:?}", e);
            return failure(
                status::StatusCode::INTERNAL_SERVER_ERROR,
                "Please, contact us".to_string(),
            );
        }
    };

    let column_source: Vec<String> = if params.columns.is_some() {
        params
            .columns
            .unwrap()
            .split(',')
            .map(|s| s.to_string())
            .collect()
    } else {
        endpoint.default_columns.iter().map(|s| s.clone()).collect()
    };

    match column_source.is_empty() {
        true => columns.push("*".to_string()),
        false => {
            for column in column_source.iter() {
                let column_name = column.as_str();

                if let Some(aggregate) = endpoint.get_aggregate_column(column_name) {
                    columns.push(format!("{} AS {}", aggregate.formula, aggregate.name));
                } else if schema.has_column_with_unqualified_name(column_name) {
                    match endpoint.get_alias_column(column_name) {
                        None => columns.push(column_name.to_string()),
                        Some(alias) => {
                            columns.push(format!("{} AS {}", column_name.to_string(), alias))
                        }
                    }
                    group_by.push(column_name);
                } else {
                    return failure(
                        status::StatusCode::BAD_REQUEST,
                        format!("The field {column_name} does not exist"),
                    );
                }
            }
        }
    }

    let mut wheres = Vec::new();
    let mut having = Vec::new();

    for (key, value) in &params.filters {
        let column;
        let operator;

        if key.starts_with("filter[") {
            let parts: Vec<&str> = key.split(|c| c == '[' || c == ']').collect();

            if parts.len() > 3 {
                return failure(
                    status::StatusCode::BAD_REQUEST,
                    format!("Not valid statement {key}"),
                );
            }

            column = parts[1];
            operator = parts[2];
        } else if let Some(filter) = endpoint.get_filter_statement(key) {
            let parts: Vec<&str> = filter.formula.rsplitn(2, "__").collect();

            if parts.len() > 2 {
                return failure(
                    status::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Filtering {key} contains an error"),
                );
            }

            column = parts[1];
            operator = parts[0];
        } else {
            continue;
        }

        let operator = match operator {
            "eq" | "" => "=",
            "lt" => "<",
            "lte" | "lteq" => "<=",
            "gt" => ">",
            "gte" | "gteq" => ">=",
            ch => {
                return failure(
                    status::StatusCode::BAD_REQUEST,
                    format!("Not valid filter operator {ch}"),
                )
            }
        };

        if let Some(aggregate) = endpoint.get_aggregate_column(column) {
            having.push(format!("{} {} '{}'", aggregate.formula, operator, value));
        } else if schema.has_column_with_unqualified_name(column) {
            wheres.push(format!("{} {} '{}'", column, operator, value));
        } else {
            return failure(
                status::StatusCode::BAD_REQUEST,
                format!("The field {column} does not exist"),
            );
        }
    }

    let orders: Vec<String>;

    // Sort: col1, -col2
    if let Some(val) = params.order {
        orders = val
            .split(',')
            .map(|val| match val.chars().next() {
                Some('-') => format!("{} DESC", &val[1..]),
                Some('+') => format!("{}", &val[1..]),
                _ => format!("{}", val),
            })
            .collect::<Vec<_>>();
    } else {
        orders = Vec::new();
    }

    // limit=100
    // limit needs to be applied after sort to make sure the result is deterministics
    let limit = params.limit.unwrap_or(1000);
    let offset = params
        .page
        .map(|page| if page == 0 { limit } else { (page - 1) * limit })
        .unwrap_or(0);

    let has_aggregates = group_by.len() != columns.len();

    let query = format!(
        "SELECT {} FROM {}{}{}{}{}{}{}",
        columns.join(", "),
        dataset_name,
        if !wheres.is_empty() {
            format!(" WHERE {}", wheres.join(" AND "))
        } else {
            "".to_string()
        },
        if has_aggregates & (group_by.len() > 0) {
            format!(" GROUP BY {}", group_by.join(", "))
        } else {
            "".to_string()
        },
        if !having.is_empty() {
            format!(" HAVING {}", having.join(", "))
        } else {
            "".to_string()
        },
        if !orders.is_empty() {
            format!(" ORDER BY {}", orders.join(", "))
        } else {
            "".to_string()
        },
        if limit > 0 {
            format!(" LIMIT {}", limit)
        } else {
            "".to_string()
        },
        if offset > 0 {
            format!(" OFFSET {}", offset)
        } else {
            "".to_string()
        },
    );

    info!("{}", query);

    sql_to_http_response(df, &query, None).await
}
