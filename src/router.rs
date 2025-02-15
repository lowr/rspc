use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use futures::Stream;
use serde_json::Value;
use specta::{
    ts::{self, datatype, ExportConfiguration},
    DataType, DataTypeFrom, TypeDefs,
};

use crate::{
    internal::{
        GlobalData, LayerReturn, Procedure, ProcedureDataType, ProcedureKind, ProcedureStore,
        RequestContext,
    },
    Config, ExecError, ExportError,
};

/// TODO
pub struct Router<TCtx = ()>
where
    TCtx: 'static,
{
    pub data: GlobalData,
    pub(crate) config: Config,
    pub(crate) queries: ProcedureStore<TCtx>,
    pub(crate) mutations: ProcedureStore<TCtx>,
    pub(crate) subscriptions: ProcedureStore<TCtx>,
    pub(crate) typ_store: TypeDefs,
}

// TODO: Move this out of this file
// TODO: Rename??
// TODO: Is similar to `ProcedureKind` and could possible be merged
#[derive(Debug, Copy, Clone)]
pub enum ExecKind {
    Query,
    Mutation,
}

impl<TCtx> Router<TCtx>
where
    TCtx: 'static,
{
    pub async fn exec(
        &self,
        ctx: TCtx,
        kind: ExecKind,
        key: String,
        input: Option<Value>,
    ) -> Result<Value, ExecError> {
        let (operations, kind) = match kind {
            ExecKind::Query => (&self.queries.store, ProcedureKind::Query),
            ExecKind::Mutation => (&self.mutations.store, ProcedureKind::Mutation),
        };

        match operations
            .get(&key)
            .ok_or_else(|| ExecError::OperationNotFound(key.clone()))?
            .exec
            .call(
                ctx,
                input.unwrap_or(Value::Null),
                RequestContext {
                    kind,
                    path: key.clone(),
                },
            )?
            .into_layer_return()
            .await?
        {
            LayerReturn::Request(v) => Ok(v),
            LayerReturn::Stream(_) => Err(ExecError::UnsupportedMethod(key)),
        }
    }

    pub async fn exec_subscription(
        &self,
        ctx: TCtx,
        key: String,
        input: Option<Value>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Value, ExecError>> + Send>>, ExecError> {
        match self
            .subscriptions
            .store
            .get(&key)
            .ok_or_else(|| ExecError::OperationNotFound(key.clone()))?
            .exec
            .call(
                ctx,
                input.unwrap_or(Value::Null),
                RequestContext {
                    kind: ProcedureKind::Subscription,
                    path: key.clone(),
                },
            )?
            .into_layer_return()
            .await?
        {
            LayerReturn::Request(_) => Err(ExecError::UnsupportedMethod(key)),
            LayerReturn::Stream(s) => Ok(s),
        }
    }

    pub fn arced(self) -> Arc<Self> {
        Arc::new(self)
    }

    pub fn typ_store(&self) -> TypeDefs {
        self.typ_store.clone()
    }

    pub fn queries(&self) -> &BTreeMap<String, Procedure<TCtx>> {
        &self.queries.store
    }

    pub fn mutations(&self) -> &BTreeMap<String, Procedure<TCtx>> {
        &self.mutations.store
    }

    pub fn subscriptions(&self) -> &BTreeMap<String, Procedure<TCtx>> {
        &self.subscriptions.store
    }

    #[allow(clippy::unwrap_used)]
    pub fn export_ts<TPath: AsRef<Path>>(&self, export_path: TPath) -> Result<(), ExportError> {
        let export_path = PathBuf::from(export_path.as_ref());
        if let Some(export_dir) = export_path.parent() {
            fs::create_dir_all(export_dir)?;
        }
        let mut file = File::create(export_path)?;
        if let Some(header) = &self.config.bindings_header {
            writeln!(file, "{}", header)?;
        }
        writeln!(file, "// This file was generated by [rspc](https://github.com/oscartbeaumont/rspc). Do not edit this file manually.\n")?;

        let config = ExportConfiguration::new().bigint(
            ts::BigIntExportBehavior::FailWithReason(
                "rspc does not support exporting bigint types (i64, u64, i128, u128) because they are lossily decoded by `JSON.parse` on the frontend. Tracking issue: https://github.com/oscartbeaumont/rspc/issues/93",
            )
        );
        writeln!(file, "{}", Procedures::new(self).big_cringe_export(&config))?;

        for ty in self.typ_store.values() {
            writeln!(file, "\n{}", ts::export_datatype(&config, ty)?)?;
        }

        Ok(())
    }
}

/// This type represents the Typescript bindings which are generated from the router by Rust.
///
/// @internal
#[derive(DataTypeFrom)]
#[cfg_attr(test, derive(specta::Type))]
#[cfg_attr(test, specta(rename = "ProceduresDef"))]
pub(crate) struct Procedures {
    #[specta(type = ProcedureDataType)]
    pub queries: Vec<ProcedureDataType>,
    #[specta(type = ProcedureDataType)]
    pub mutations: Vec<ProcedureDataType>,
    #[specta(type = ProcedureDataType)]
    pub subscriptions: Vec<ProcedureDataType>,
}

impl Procedures {
    pub fn new<TCtx>(router: &Router<TCtx>) -> Self {
        Self {
            queries: store_to_datatypes(&router.queries.store),
            mutations: store_to_datatypes(&router.mutations.store),
            subscriptions: store_to_datatypes(&router.subscriptions.store),
        }
    }

    // TODO: Using the `ToDataType` system causing the formatting of the resulting bindings to be disgusting. This is a really difficult problem to solve because I want the container and children to be formatting differently.
    // TODO: Work on making Specta support custom formatting configs or something like that and then move back to this system.
    pub fn big_cringe_export(&self, config: &ExportConfiguration) -> String {
        // TODO: This is the old code!
        // ts::export_datatype(
        //         &config,
        //         // TODO: I wish this could be an `into` impl but because of `<T as Type>` we can't. We can't assume `derive(DataTypeFrom)` implies `derive(Type)` (to get comments).
        //         // Having an the conversion just implicitly set comments to empty seems like a bit of a footgun.
        //         &DataTypeExt {
        //             name: "Procedures",
        //             comments: &[],
        //             inner: Procedures::new(self).into()
        //         }
        //     )
        //     .unwrap()

        let queries_ts = generate_procedures_ts(config, &self.queries);
        let mutations_ts = generate_procedures_ts(config, &self.mutations);
        let subscriptions_ts = generate_procedures_ts(config, &self.subscriptions);
        format!(
            r#"export type Procedures = {{
    queries: {queries_ts},
    mutations: {mutations_ts},
    subscriptions: {subscriptions_ts}
}};"#
        )
    }
}

fn store_to_datatypes<Ctx>(
    procedures: &BTreeMap<String, Procedure<Ctx>>,
) -> Vec<ProcedureDataType> {
    procedures
        .values()
        .map(|p| p.ty.clone())
        .collect::<Vec<_>>()
}

// TODO: Move this out into a Specta API
fn generate_procedures_ts(
    config: &ExportConfiguration,
    procedures: &Vec<ProcedureDataType>,
) -> String {
    // TODO: WTF does this have results is the `ToDataType` alternative doesn't return a result. Is it magic or does it just do the big bad and panic internally?

    match procedures.len() {
        0 => "never".to_string(),
        _ => procedures
            .iter()
            .map(|operation| {
                let input = match &operation.input {
                    DataType::Tuple(def)
                        // This condition is met with an empty enum or `()`.
                        if def.fields.is_empty() =>
                    {
                        "never".into()
                    }
                    ty => datatype(&config, &ty).expect("Failed to generate Typescript bindings"),
                };
                let result_ts = datatype(&Default::default(), &operation.result)
                    .expect("Failed to generate Typescript bindings");

                let key = &operation.key;
                format!(
                    r#"
        {{ key: "{key}", input: {input}, result: {result_ts} }}"#
                )
            })
            .collect::<Vec<_>>()
            .join(" | "),
    }
}
