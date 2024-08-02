#[cfg(feature = "schemars")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use crate::component::{Nameable, WithDependsOn};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Column {
    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Aggregate {
    pub name: String,

    pub formula: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Filter {
    pub alias: String,
    pub formula: String,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(JsonSchema))]
pub struct Endpoint {
    pub name: String,

    pub dataset: String,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "columns", default)]
    pub columns: Vec<Column>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "aggregates", default)]
    pub aggregates: Vec<Aggregate>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "filters", default)]
    pub filters: Vec<Filter>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "default_columns", default)]
    pub default_columns: Vec<String>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "dependsOn", default)]
    pub depends_on: Vec<String>,
}

impl Nameable for Endpoint {
    fn name(&self) -> &str {
        &self.name
    }
}


impl WithDependsOn<Endpoint> for Endpoint {
    fn depends_on(&self, depends_on: &[String]) -> Endpoint {
        Self {
            name: self.name.clone(),
            dataset: self.dataset.clone(),
            columns: self.columns.clone(),
            aggregates: self.aggregates.clone(),
            filters: self.filters.clone(),
            default_columns: self.default_columns.clone(),
            depends_on: depends_on.to_vec(),
        }
    }
}

impl Endpoint {
    pub fn get_alias_column(&self, name: &str) -> Option<String> {
        match self.columns.iter().find(|&field| field.name == name) {
            Some(column) => column.alias.clone(),
            _ => None
        }
    }

    pub fn get_aggregate_column(&self, name: &str) -> Option<&Aggregate> {
        self.aggregates.iter().find(|&field| field.name == name)
    }

    pub fn get_filter_statement(&self, alias: &str) -> Option<&Filter> {
        self.filters.iter().find(|&filter| filter.alias == alias)
    }
}