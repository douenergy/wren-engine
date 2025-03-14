// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use crate::errors::CoreError;
use crate::manifest::to_manifest;
use crate::remote_functions::PyRemoteFunction;
use log::debug;
use pyo3::{pyclass, pymethods, PyErr, PyResult};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::ControlFlow;
use std::sync::Arc;
use wren_core::ast::{visit_statements_mut, Expr, Statement, Value};
use wren_core::dialect::GenericDialect;
use wren_core::logical_plan::utils::map_data_type;
use wren_core::mdl::context::create_ctx_with_mdl;
use wren_core::mdl::function::{
    ByPassAggregateUDF, ByPassScalarUDF, ByPassWindowFunction, FunctionType,
    RemoteFunction,
};
use wren_core::{mdl, AggregateUDF, AnalyzedWrenMDL, ScalarUDF, WindowUDF};
/// The Python wrapper for the Wren Core session context.
#[pyclass(name = "SessionContext")]
#[derive(Clone)]
pub struct PySessionContext {
    ctx: wren_core::SessionContext,
    mdl: Arc<AnalyzedWrenMDL>,
    remote_functions: Vec<RemoteFunction>,
}

impl Hash for PySessionContext {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.mdl.hash(state);
        self.remote_functions.hash(state);
    }
}

impl Default for PySessionContext {
    fn default() -> Self {
        Self {
            ctx: wren_core::SessionContext::new(),
            mdl: Arc::new(AnalyzedWrenMDL::default()),
            remote_functions: vec![],
        }
    }
}

#[pymethods]
impl PySessionContext {
    /// Create a new session context.
    ///
    /// if `mdl_base64` is provided, the session context will be created with the given MDL. Otherwise, an empty MDL will be created.
    /// if `remote_functions_path` is provided, the session context will be created with the remote functions defined in the CSV file.
    #[new]
    #[pyo3(signature = (mdl_base64=None, remote_functions_path=None))]
    pub fn new(
        mdl_base64: Option<&str>,
        remote_functions_path: Option<&str>,
    ) -> PyResult<Self> {
        let remote_functions = Self::read_remote_function_list(remote_functions_path)
            .map_err(CoreError::from)?;
        let remote_functions: Vec<RemoteFunction> = remote_functions
            .into_iter()
            .map(|f| f.into())
            .collect::<Vec<_>>();

        let ctx = wren_core::SessionContext::new();

        let Some(mdl_base64) = mdl_base64 else {
            return Ok(Self {
                ctx,
                mdl: Arc::new(AnalyzedWrenMDL::default()),
                remote_functions,
            });
        };

        let manifest = to_manifest(mdl_base64)?;

        let Ok(analyzed_mdl) = AnalyzedWrenMDL::analyze(manifest) else {
            return Err(CoreError::new("Failed to analyze manifest").into());
        };

        let analyzed_mdl = Arc::new(analyzed_mdl);

        let runtime = tokio::runtime::Runtime::new().map_err(CoreError::from)?;
        let ctx = runtime
            .block_on(create_ctx_with_mdl(&ctx, Arc::clone(&analyzed_mdl), false))
            .map_err(CoreError::from)?;

        remote_functions.iter().try_for_each(|remote_function| {
            debug!("Registering remote function: {:?}", remote_function);
            Self::register_remote_function(&ctx, remote_function)?;
            Ok::<(), CoreError>(())
        })?;

        Ok(Self {
            ctx,
            mdl: analyzed_mdl,
            remote_functions,
        })
    }

    /// Transform the given Wren SQL to the equivalent Planned SQL.
    pub fn transform_sql(&self, sql: &str) -> PyResult<String> {
        mdl::transform_sql(Arc::clone(&self.mdl), &self.remote_functions, sql)
            .map_err(|e| PyErr::from(CoreError::from(e)))
    }

    /// Get the available functions in the session context.
    pub fn get_available_functions(&self) -> PyResult<Vec<PyRemoteFunction>> {
        let mut builder = self
            .remote_functions
            .iter()
            .map(|f| (f.name.clone(), f.clone().into()))
            .collect::<HashMap<String, PyRemoteFunction>>();
        self.ctx
            .state()
            .scalar_functions()
            .iter()
            .for_each(|(name, _func)| {
                match builder.entry(name.clone()) {
                    Entry::Occupied(_) => {}
                    Entry::Vacant(entry) => {
                        entry.insert(PyRemoteFunction {
                            function_type: "scalar".to_string(),
                            name: name.clone(),
                            // TODO: get function return type from SessionState
                            return_type: None,
                            param_names: None,
                            param_types: None,
                            description: None,
                        });
                    }
                }
            });
        self.ctx
            .state()
            .aggregate_functions()
            .iter()
            .for_each(|(name, _func)| {
                match builder.entry(name.clone()) {
                    Entry::Occupied(_) => {}
                    Entry::Vacant(entry) => {
                        entry.insert(PyRemoteFunction {
                            function_type: "aggregate".to_string(),
                            name: name.clone(),
                            // TODO: get function return type from SessionState
                            return_type: None,
                            param_names: None,
                            param_types: None,
                            description: None,
                        });
                    }
                }
            });
        self.ctx
            .state()
            .window_functions()
            .iter()
            .for_each(|(name, _func)| {
                match builder.entry(name.clone()) {
                    Entry::Occupied(_) => {}
                    Entry::Vacant(entry) => {
                        entry.insert(PyRemoteFunction {
                            function_type: "window".to_string(),
                            name: name.clone(),
                            // TODO: get function return type from SessionState
                            return_type: None,
                            param_names: None,
                            param_types: None,
                            description: None,
                        });
                    }
                }
            });
        Ok(builder.values().cloned().collect())
    }

    /// Push down the limit to the given SQL.
    /// If the limit is None, the SQL will be returned as is.
    /// If the limit is greater than the pushdown limit, the limit will be replaced with the pushdown limit.
    /// Otherwise, the limit will be kept as is.
    #[pyo3(signature = (sql, limit=None))]
    pub fn pushdown_limit(&self, sql: &str, limit: Option<usize>) -> PyResult<String> {
        if limit.is_none() {
            return Ok(sql.to_string());
        }
        let pushdown = limit.unwrap();
        let mut statements =
            wren_core::parser::Parser::parse_sql(&GenericDialect {}, sql)
                .map_err(CoreError::from)?;
        if statements.len() != 1 {
            return Err(CoreError::new("Only one statement is allowed").into());
        }
        visit_statements_mut(&mut statements, |stmt| {
            if let Statement::Query(q) = stmt {
                if let Some(limit) = &q.limit {
                    if let Expr::Value(Value::Number(n, is)) = limit {
                        if n.parse::<usize>().unwrap() > pushdown {
                            q.limit = Some(Expr::Value(Value::Number(
                                pushdown.to_string(),
                                is.clone(),
                            )));
                        }
                    }
                } else {
                    q.limit =
                        Some(Expr::Value(Value::Number(pushdown.to_string(), false)));
                }
            }
            ControlFlow::<()>::Continue(())
        });
        Ok(statements[0].to_string())
    }
}

impl PySessionContext {
    fn register_remote_function(
        ctx: &wren_core::SessionContext,
        remote_function: &RemoteFunction,
    ) -> PyResult<()> {
        match &remote_function.function_type {
            FunctionType::Scalar => {
                ctx.register_udf(ScalarUDF::new_from_impl(ByPassScalarUDF::new(
                    &remote_function.name,
                    map_data_type(&remote_function.return_type)
                        .map_err(CoreError::from)?,
                )))
            }
            FunctionType::Aggregate => {
                ctx.register_udaf(AggregateUDF::new_from_impl(ByPassAggregateUDF::new(
                    &remote_function.name,
                    map_data_type(&remote_function.return_type)
                        .map_err(CoreError::from)?,
                )))
            }
            FunctionType::Window => {
                ctx.register_udwf(WindowUDF::new_from_impl(ByPassWindowFunction::new(
                    &remote_function.name,
                    map_data_type(&remote_function.return_type)
                        .map_err(CoreError::from)?,
                )))
            }
        }
        Ok(())
    }

    fn read_remote_function_list(path: Option<&str>) -> PyResult<Vec<PyRemoteFunction>> {
        debug!(
            "Reading remote function list from {}",
            path.unwrap_or("path is not provided")
        );
        if let Some(path) = path {
            Ok(csv::Reader::from_path(path)
                .map_err(CoreError::from)?
                .into_deserialize::<PyRemoteFunction>()
                .filter_map(Result::ok)
                .collect::<Vec<_>>())
        } else {
            Ok(vec![])
        }
    }
}
