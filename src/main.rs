mod model;
mod websocket_client;

use jarvis_lib::config_client::{ConfigClient, ConfigClientConfig};
use jarvis_lib::planner_service::{PlannerService, PlannerServiceConfig};
use jarvis_lib::spot_prices_state_client::{SpotPricesStateClient, SpotPricesStateClientConfig};
use websocket_client::{WebsocketClient, WebsocketClientConfig};

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let spot_prices_state_client_config = SpotPricesStateClientConfig::from_env().await?;
    let spot_prices_state_client = SpotPricesStateClient::new(spot_prices_state_client_config);

    let config_client_config = ConfigClientConfig::from_env()?;
    let config_client = ConfigClient::new(config_client_config);

    let websocket_client_config = WebsocketClientConfig::from_env()?;
    let websocket_client = WebsocketClient::new(websocket_client_config);

    let planner_service_config = PlannerServiceConfig::new(
        config_client,
        spot_prices_state_client,
        Box::new(websocket_client),
    )?;
    let planner_service = PlannerService::new(planner_service_config);

    planner_service.run().await?;

    Ok(())
}
