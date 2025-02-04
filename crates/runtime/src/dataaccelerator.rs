/*
Copyright 2024 The Spice.ai OSS Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use crate::component::dataset::acceleration::{self, Acceleration, Engine, IndexType, Mode};
use crate::secrets::{ExposeSecret, ParamStr, Secrets};
use ::arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use datafusion::common::Constraint;
use datafusion::{
    common::{Constraints, TableReference, ToDFSchema},
    datasource::TableProvider,
    logical_expr::CreateExternalTable,
};
use datafusion_table_providers::util::{
    column_reference::ColumnReference, on_conflict::OnConflict,
};
use secrecy::SecretString;
use snafu::prelude::*;
use std::sync::LazyLock;
use std::{any::Any, collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, RwLock};

use self::arrow::ArrowAccelerator;

#[cfg(feature = "duckdb")]
use self::duckdb::DuckDBAccelerator;
#[cfg(feature = "postgres")]
use self::postgres::PostgresAccelerator;
#[cfg(feature = "sqlite")]
use self::sqlite::SqliteAccelerator;

pub mod arrow;
#[cfg(feature = "duckdb")]
pub mod duckdb;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;

pub mod metadata;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Invalid configuration: {msg}"))]
    InvalidConfiguration { msg: String },

    #[snafu(display("Unknown engine: {engine}"))]
    UnknownEngine { engine: Arc<str> },

    #[snafu(display("Acceleration creation failed: {source}"))]
    AccelerationCreationFailed {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

static DATA_ACCELERATOR_ENGINES: LazyLock<Mutex<HashMap<Engine, Arc<dyn DataAccelerator>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub async fn register_accelerator_engine(
    name: Engine,
    accelerator_engine: Arc<dyn DataAccelerator>,
) {
    let mut registry = DATA_ACCELERATOR_ENGINES.lock().await;

    registry.insert(name, accelerator_engine);
}

pub async fn register_all() {
    register_accelerator_engine(Engine::Arrow, Arc::new(ArrowAccelerator::new())).await;
    #[cfg(feature = "duckdb")]
    register_accelerator_engine(Engine::DuckDB, Arc::new(DuckDBAccelerator::new())).await;
    #[cfg(feature = "postgres")]
    register_accelerator_engine(Engine::PostgreSQL, Arc::new(PostgresAccelerator::new())).await;
    #[cfg(feature = "sqlite")]
    register_accelerator_engine(Engine::Sqlite, Arc::new(SqliteAccelerator::new())).await;
}

pub async fn get_accelerator_engine(engine: Engine) -> Option<Arc<dyn DataAccelerator>> {
    let guard = DATA_ACCELERATOR_ENGINES.lock().await;

    let engine = guard.get(&engine);

    match engine {
        Some(engine_ref) => Some(Arc::clone(engine_ref)),
        None => None,
    }
}

/// A `DataAccelerator` knows how to read, write and create new tables.
#[async_trait]
pub trait DataAccelerator: Send + Sync {
    fn as_any(&self) -> &dyn Any;

    /// Creates a new table in the accelerator engine, returning a `TableProvider` that supports reading and writing.
    async fn create_external_table(
        &self,
        cmd: &CreateExternalTable,
    ) -> Result<Arc<dyn TableProvider>, Box<dyn std::error::Error + Send + Sync>>;
}

pub struct AcceleratorExternalTableBuilder {
    table_name: TableReference,
    schema: SchemaRef,
    engine: Engine,
    mode: Mode,
    options: Option<HashMap<String, SecretString>>,
    indexes: HashMap<ColumnReference, IndexType>,
    constraints: Option<Constraints>,
    on_conflict: Option<OnConflict>,
}

impl AcceleratorExternalTableBuilder {
    #[must_use]
    pub fn new(table_name: TableReference, schema: SchemaRef, engine: Engine) -> Self {
        Self {
            table_name,
            schema,
            engine,
            mode: Mode::Memory,
            options: None,
            indexes: HashMap::new(),
            constraints: None,
            on_conflict: None,
        }
    }

    #[must_use]
    pub fn indexes(mut self, indexes: HashMap<ColumnReference, IndexType>) -> Self {
        self.indexes = indexes;
        self
    }

    #[must_use]
    pub fn on_conflict(mut self, on_conflict: OnConflict) -> Self {
        self.on_conflict = Some(on_conflict);
        self
    }

    #[must_use]
    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    #[must_use]
    pub fn options(mut self, options: HashMap<String, SecretString>) -> Self {
        self.options = Some(options);
        self
    }

    #[must_use]
    pub fn constraints(mut self, constraints: Constraints) -> Self {
        self.constraints = Some(constraints);
        self
    }

    fn validate_arrow(&self) -> Result<(), Error> {
        if Mode::File == self.mode {
            InvalidConfigurationSnafu {
                msg: "File mode not supported for Arrow engine".to_string(),
            }
            .fail()?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), Error> {
        match self.engine {
            Engine::Arrow => self.validate_arrow(),
            _ => Ok(()),
        }
    }

    pub fn build(self) -> Result<CreateExternalTable> {
        self.validate()?;

        let mut options: HashMap<String, String> = self
            .options
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, v.expose_secret().to_string()))
            .collect();

        let df_schema = ToDFSchema::to_dfschema_ref(Arc::clone(&self.schema));

        let mode = self.mode;
        options.insert("mode".to_string(), mode.to_string());

        if !self.indexes.is_empty() {
            let indexes_option_str = Acceleration::hashmap_to_option_string(&self.indexes);
            options.insert("indexes".to_string(), indexes_option_str);
        }

        if let Some(on_conflict) = self.on_conflict {
            options.insert("on_conflict".to_string(), on_conflict.to_string());
        }

        let constraints = match self.constraints {
            Some(constraints) => constraints,
            None => Constraints::empty(),
        };

        let external_table = CreateExternalTable {
            schema: df_schema.map_err(|e| {
                InvalidConfigurationSnafu {
                    msg: format!("Failed to convert schema: {e}"),
                }
                .build()
            })?,
            name: self.table_name.clone(),
            location: String::new(),
            file_type: String::new(),
            table_partition_cols: vec![],
            if_not_exists: true,
            definition: None,
            order_exprs: vec![],
            unbounded: false,
            options,
            constraints,
            column_defaults: HashMap::default(),
        };

        Ok(external_table)
    }
}

pub async fn create_accelerator_table(
    table_name: TableReference,
    schema: SchemaRef,
    constraints: Option<&Constraints>,
    acceleration_settings: &acceleration::Acceleration,
    secrets: Arc<RwLock<Secrets>>,
) -> Result<Arc<dyn TableProvider>> {
    let engine = acceleration_settings.engine;

    let accelerator =
        get_accelerator_engine(engine)
            .await
            .ok_or_else(|| Error::InvalidConfiguration {
                msg: format!("Unknown engine: {engine}"),
            })?;

    if let Err(e) = acceleration_settings.validate_indexes(&schema) {
        InvalidConfigurationSnafu {
            msg: format!("{e}"),
        }
        .fail()?;
    };

    let secret_guard = secrets.read().await;
    let mut params_with_secrets: HashMap<String, SecretString> = HashMap::new();

    // Inject secrets from the user-supplied params.
    // This will replace any instances of `${ store:key }` with the actual secret value.
    for (k, v) in &acceleration_settings.params {
        let secret = secret_guard.inject_secrets(k, ParamStr(v)).await;
        params_with_secrets.insert(k.clone(), secret);
    }

    let mut external_table_builder =
        AcceleratorExternalTableBuilder::new(table_name, Arc::clone(&schema), engine)
            .mode(acceleration_settings.mode)
            .options(params_with_secrets)
            .indexes(acceleration_settings.indexes.clone());

    // If there are constraints from the federated table, then add them to the accelerated table
    // and automatically configure upsert behavior for them. This can be overridden by the user.
    if let Some(constraints) = constraints {
        if !constraints.is_empty() {
            external_table_builder = external_table_builder.constraints(constraints.clone());
            let primary_keys: Vec<String> = get_primary_keys_from_constraints(constraints, &schema);
            external_table_builder = external_table_builder
                .on_conflict(OnConflict::Upsert(ColumnReference::new(primary_keys)));
        }
    }

    if let Some(on_conflict) =
        acceleration_settings
            .on_conflict()
            .map_err(|e| Error::InvalidConfiguration {
                msg: format!("on_conflict invalid: {e}"),
            })?
    {
        external_table_builder = external_table_builder.on_conflict(on_conflict);
    };

    match acceleration_settings.table_constraints(Arc::clone(&schema)) {
        Ok(Some(constraints)) => {
            if !constraints.is_empty() {
                external_table_builder = external_table_builder.constraints(constraints);
            }
        }
        Ok(None) => {}
        Err(e) => {
            InvalidConfigurationSnafu {
                msg: format!("{e}"),
            }
            .fail()?;
        }
    }

    let external_table = external_table_builder.build()?;

    let table_provider = accelerator
        .create_external_table(&external_table)
        .await
        .context(AccelerationCreationFailedSnafu)?;

    Ok(table_provider)
}

fn get_primary_keys_from_constraints(constraints: &Constraints, schema: &SchemaRef) -> Vec<String> {
    constraints
        .iter()
        .filter_map(|constraint| {
            if let Constraint::PrimaryKey(col_indexes) = constraint {
                Some(
                    col_indexes
                        .iter()
                        .map(|&col_index| schema.field(col_index).name().to_string()),
                )
            } else {
                None
            }
        })
        .flatten()
        .collect()
}
