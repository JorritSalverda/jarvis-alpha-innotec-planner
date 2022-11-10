mod model;
mod state_client;
mod websocket_client;

use jarvis_lib::config_client::{ConfigClient, ConfigClientConfig};
use jarvis_lib::planner_service::{PlannerService, PlannerServiceConfig};
use jarvis_lib::spot_prices_state_client::{SpotPricesStateClient, SpotPricesStateClientConfig};
use state_client::StateClient;
use websocket_client::WebsocketClient;

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let spot_prices_state_client_config = SpotPricesStateClientConfig::from_env().await?;
    let spot_prices_state_client = SpotPricesStateClient::new(spot_prices_state_client_config);

    let config_client_config = ConfigClientConfig::from_env()?;
    let config_client = ConfigClient::new(config_client_config);

    let state_client = StateClient::from_env().await?;
    let websocket_client = WebsocketClient::from_env(Some(state_client))?;

    let planner_service_config = PlannerServiceConfig::new(
        config_client,
        spot_prices_state_client,
        Box::new(websocket_client),
    )?;
    let planner_service = PlannerService::new(planner_service_config);

    planner_service.run().await?;

    Ok(())
}

#[cfg(test)]
#[ctor::ctor]
fn init() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}
