use crate::model::State;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    api::{Api, PostParams},
    Client,
};
use std::env;
use std::error::Error;
use std::fs;
use std::path::Path;
use tracing::debug;

pub struct StateClientConfig {
    kube_client: kube::Client,
    state_file_path: String,
    state_file_configmap_name: String,
    current_namespace: String,
}

impl StateClientConfig {
    pub fn new(
        kube_client: kube::Client,
        state_file_path: String,
        state_file_configmap_name: String,
        current_namespace: String,
    ) -> Result<Self, Box<dyn Error>> {
        debug!(
          "StateClientConfig::new(state_file_path: {}, state_file_configmap_name: {}, current_namespace: {})",
          state_file_path, state_file_configmap_name, current_namespace
        );

        Ok(Self {
            kube_client,
            state_file_path,
            state_file_configmap_name,
            current_namespace,
        })
    }

    pub async fn from_env() -> Result<Self, Box<dyn Error>> {
        let kube_client: kube::Client = Client::try_default().await?;

        let state_file_path =
            env::var("STATE_FILE_PATH").unwrap_or_else(|_| "/configs/last-state.yaml".to_string());
        let state_file_configmap_name = env::var("STATE_FILE_CONFIG_MAP_NAME")
            .unwrap_or_else(|_| "jarvis-alpha-innotec-planner".to_string());

        let current_namespace =
            fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")?;

        Self::new(
            kube_client,
            state_file_path,
            state_file_configmap_name,
            current_namespace,
        )
    }
}

pub struct StateClient {
    config: StateClientConfig,
}

impl StateClient {
    pub fn new(config: StateClientConfig) -> StateClient {
        StateClient { config }
    }

    pub async fn from_env() -> Result<Self, Box<dyn Error>> {
        Ok(Self::new(StateClientConfig::from_env().await?))
    }

    pub fn read_state(&self) -> Result<Option<State>, Box<dyn std::error::Error>> {
        let state_file_contents = match fs::read_to_string(&self.config.state_file_path) {
            Ok(c) => c,
            Err(_) => return Ok(Option::None),
        };

        let last_state: Option<State> = match serde_yaml::from_str(&state_file_contents) {
            Ok(lm) => Some(lm),
            Err(_) => return Ok(Option::None),
        };

        println!(
            "Read previous state from state file at {}",
            &self.config.state_file_path
        );

        Ok(last_state)
    }

    async fn get_state_configmap(&self) -> Result<ConfigMap, Box<dyn std::error::Error>> {
        let configmaps_api: Api<ConfigMap> = Api::namespaced(
            self.config.kube_client.clone(),
            &self.config.current_namespace,
        );

        let config_map = configmaps_api
            .get(&self.config.state_file_configmap_name)
            .await?;

        Ok(config_map)
    }

    async fn update_state_configmap(
        &self,
        config_map: &ConfigMap,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let configmaps_api: Api<ConfigMap> = Api::namespaced(
            self.config.kube_client.clone(),
            &self.config.current_namespace,
        );

        configmaps_api
            .replace(
                &self.config.state_file_configmap_name,
                &PostParams::default(),
                config_map,
            )
            .await?;

        Ok(())
    }

    pub async fn store_state(&self, state: &State) -> Result<(), Box<dyn std::error::Error>> {
        // retrieve configmap
        let mut config_map = self.get_state_configmap().await?;

        // marshal state to yaml
        let yaml_data = match serde_yaml::to_string(state) {
            Ok(yd) => yd,
            Err(e) => return Err(Box::new(e)),
        };

        // extract filename from config file path
        let state_file_path = Path::new(&self.config.state_file_path);
        let state_file_name = match state_file_path.file_name() {
            Some(filename) => match filename.to_str() {
                Some(filename) => String::from(filename),
                None => return Err(Box::<dyn Error>::from("No filename found in path")),
            },
            None => return Err(Box::<dyn Error>::from("No filename found in path")),
        };

        // update data in configmap
        let mut data: std::collections::BTreeMap<String, String> =
            config_map.data.unwrap_or_default();
        data.insert(state_file_name, yaml_data);
        config_map.data = Some(data);

        // update configmap to have measurement available when the application runs the next time and for other applications
        self.update_state_configmap(&config_map).await?;

        println!(
            "Stored last state in configmap {}",
            &self.config.state_file_configmap_name
        );

        Ok(())
    }
}
