#![feature(proc_macro_hygiene, decl_macro)]

#[macro_use]
extern crate log;

use std::{env};
use std::borrow::{Cow, ToOwned};
use std::collections::BTreeMap;

use std::sync::Arc;

use futures::stream::StreamExt;
use k8s_openapi::api::core::v1::{Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::Resource;
use kube::ResourceExt;
use kube::{
    api::ListParams, client::Client, runtime::controller::Action, runtime::Controller, Api, Error as KubeError,
};
use tokio::time::Duration;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use serde_json::{json, Value};
use config::Config;
use const_format::formatcp;
use crate::bw::BitwardenClientWrapper;

use crate::crd::BitwardenSecret;

pub mod crd;
mod bw;
// mod bitwarden;

const BW_OPERATOR_ENV_PREFIX: &'static str = "BW_OPERATOR";
const ENV_CONFIG_PATH: &'static str = formatcp!("{}_CONFIG", BW_OPERATOR_ENV_PREFIX);
const DEFAULT_CONFIG_PATH: &'static str = "config/config";

// TODO add status
// TODO Watch secret deletion, if owner refs contains a bitwardensecret, recreate
#[tokio::main]
async fn main() {
    env::set_var("RUST_LOG", "info");
    env_logger::init();

    let config: Config = Config::builder()
        .add_source(config::File::with_name(&env::var(ENV_CONFIG_PATH).unwrap_or(DEFAULT_CONFIG_PATH.to_owned())))
        .add_source(config::Environment::with_prefix(BW_OPERATOR_ENV_PREFIX))
        .build()
        .expect("Could not initialize config");

    let bw_client = BitwardenClientWrapper::new(config);

    let kubernetes_client: Client = Client::try_default()
        .await
        .expect("Expected a valid KUBECONFIG environment variable.");

    let crd_api: Api<BitwardenSecret> = Api::all(kubernetes_client.clone());
    let context: Arc<ContextData> = Arc::new(ContextData::new(kubernetes_client.clone(), bw_client));

    // The controller comes from the `kube_runtime` crate and manages the reconciliation process.
    // It requires the following information:
    // - `kube::Api<T>` this controller "owns". In this case, `T = BitwardenSecret`, as this controller owns the `BitwardenSecret` resource,
    // - `kube::api::ListParams` to select the `BitwardenSecret` resources with. Can be used for BitwardenSecret filtering `BitwardenSecret` resources before reconciliation,
    // - `reconcile` function with reconciliation logic to be called each time a resource of `BitwardenSecret` kind is created/updated/deleted,
    // - `on_error` function to call whenever reconciliation fails.
    Controller::new(crd_api.clone(), ListParams::default())
        .run(reconcile, on_error, context)
        .for_each(|reconciliation_result| async move {
            match reconciliation_result {
                Ok(bitwarden_secret_resource) => {
                    println!("Reconciliation successful. Resource: {:?}", bitwarden_secret_resource);
                }
                Err(reconciliation_err) => {
                    eprintln!("Reconciliation error: {:?}", reconciliation_err)
                }
            }
        })
        .await;
}

struct ContextData {
    client: Client,
    bw_client: BitwardenClientWrapper,
}

impl ContextData {
    pub fn new(client: Client, bw_client: BitwardenClientWrapper) -> Self {
        ContextData { client, bw_client }
    }
}

enum BitwardenSecretAction {
    Create,
    Delete,
    NoOp,
}

async fn reconcile(bitwarden_secret: Arc<BitwardenSecret>, context: Arc<ContextData>) -> Result<Action, Error> {
    let client: Client = context.client.clone(); // The `Client` is shared -> a clone from the reference is obtained
    let mut bw_client: BitwardenClientWrapper = context.bw_client.clone(); // The `Client` is shared -> a clone from the reference is obtained

    // The resource of `BitwardenSecret` kind is required to have a namespace set. However, it is not guaranteed
    // the resource will have a `namespace` set. Therefore, the `namespace` field on object's metadata
    // is optional and Rust forces the programmer to check for it's existence first.
    let namespace: String = match bitwarden_secret.namespace() {
        None => "default".to_string(),
        Some(namespace) => namespace,
    };

    let name = bitwarden_secret.name_any();

    return match determine_action(&bitwarden_secret) {
        BitwardenSecretAction::Create => {
            add_finalizer(client.clone(), &name, &namespace).await?;

            let mut labels: BTreeMap<String, String> = BTreeMap::new();
            labels.insert("app".to_owned(), name.to_owned());
            // TODO copy labels (all but?)

            let result = bw_client.fetch_item("homelab/argo-minio".to_string());
            if result.is_err() {
                info!("Resetting bw context");
                if let Some(e) = result.err() {
                    info!("source: {}", e.to_string())
                }
                bw_client.reset();
            } else {
                let secret_keys: BTreeMap<String, String> = result.unwrap();


                let owner_ref = OwnerReference {
                    // api_version: api_v_test(bitwarden_secret.as_ref()),
                    // kind: kind_test(bitwarden_secret.as_ref()),
                    api_version: "tomjo.net/v1".to_string(),
                    kind: "BitwardenSecret".to_string(),
                    name: name.clone(),
                    uid: bitwarden_secret.uid().expect(&format!("Bitwarden secret without uid: {}/{}", namespace, &name)),
                    block_owner_deletion: Some(true),
                    controller: None,
                };


                create_secret(client, owner_ref, &name, &namespace, &bitwarden_secret.spec.type_, secret_keys, labels).await?;
            }
            Ok(Action::requeue(Duration::from_secs(10)))
        }
        BitwardenSecretAction::Delete => {
            delete_secret(client.clone(), &name, &namespace).await?;
            delete_finalizer(client, &name, &namespace).await?;
            Ok(Action::await_change())
        }
        BitwardenSecretAction::NoOp => Ok(Action::requeue(Duration::from_secs(10))),
    };
}

pub fn api_v_test<T: Resource<DynamicType=()>>(resource: &BitwardenSecret) -> String {
    return T::api_version(&()).to_string();
    // .kind(T::kind(&()))
    // .name(resource.name_any())
    // .uid_opt(resource.meta().uid.clone());
}

pub fn kind_test<T: Resource<DynamicType=()>>(resource: &BitwardenSecret) -> String {
    return T::kind(&()).to_string();
    // .kind(T::kind(&()))
    // .name(resource.name_any())
    // .uid_opt(resource.meta().uid.clone());
}

fn determine_action(bitwarden_secret: &BitwardenSecret) -> BitwardenSecretAction {
    return if bitwarden_secret.meta().deletion_timestamp.is_some() {
        BitwardenSecretAction::Delete
    } else if bitwarden_secret
        .meta()
        .finalizers
        .as_ref()
        .map_or(true, |finalizers| finalizers.is_empty()) {
        BitwardenSecretAction::Create
    } else {
        BitwardenSecretAction::NoOp
    };
}

/// TODO Note: Does not check for resource's existence for simplicity.
pub async fn add_finalizer(client: Client, name: &str, namespace: &str) -> Result<BitwardenSecret, Error> {
    let api: Api<BitwardenSecret> = Api::namespaced(client, namespace);
    let finalizer: Value = json!({
        "metadata": {
            "finalizers": ["bitwardensecrets.tomjo.net/finalizer.secret"]
        }
    });

    let patch: Patch<&Value> = Patch::Merge(&finalizer);
    Ok(api.patch(name, &PatchParams::default(), &patch).await?)
}

pub async fn delete_finalizer(client: Client, name: &str, namespace: &str) -> Result<(), Error> {
    let api: Api<BitwardenSecret> = Api::namespaced(client, namespace);
    let has_resource = api.get_opt(name).await?.is_some();
    if has_resource {
        let finalizer: Value = json!({
            "metadata": {
                "finalizers": null
            }
        });

        let patch: Patch<&Value> = Patch::Merge(&finalizer);
        api.patch(name, &PatchParams::default(), &patch).await?;
    }
    Ok(())
}

/// TODO Note: It is assumed the resource does not already exists for simplicity. Returns an `Error` if it does.
pub async fn create_secret(
    client: Client,
    owner_ref: OwnerReference,
    name: &str,
    namespace: &str,
    type_: &str,
    secret_keys: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
) -> Result<Secret, KubeError> {
    let secret: Secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(labels.clone()),
            owner_references: Some(vec![owner_ref]),
            ..ObjectMeta::default()
        },
        string_data: Some(secret_keys.clone()),
        type_: Some(type_.to_owned()),
        ..Secret::default()
    };

    let secret_api: Api<Secret> = Api::namespaced(client, namespace);
    secret_api
        .create(&PostParams::default(), &secret)
        .await
}

pub async fn delete_secret(client: Client, name: &str, namespace: &str) -> Result<(), Error> {
    let api: Api<Secret> = Api::namespaced(client, namespace);
    let has_secret = api.get_opt(name).await?.is_some();
    if has_secret {
        api.delete(name, &DeleteParams::default()).await?;
    }
    Ok(())
}

fn on_error(bitwarden_secret: Arc<BitwardenSecret>, error: &Error, _context: Arc<ContextData>) -> Action {
    eprintln!("Reconciliation error:\n{:?}.\n{:?}", error, bitwarden_secret);
    Action::requeue(Duration::from_secs(5)) //TODO exponential backoff
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kubernetes reported error: {source}")]
    KubeError {
        #[from]
        source: kube::Error,
    },
    #[error("Invalid BitwardenSecret CRD: {0}")]
    UserInputError(String),
}
