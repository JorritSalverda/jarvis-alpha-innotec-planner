use crate::model::{Config, Content, State};
use crate::state_client::StateClient;
use async_trait::async_trait;
use chrono::{prelude::*, Duration, Utc};
use chrono_tz::Tz;
use jarvis_lib::model::{SpotPrice, SpotPricePlanner};
use jarvis_lib::planner_client::PlannerClient;
use log::{debug, info};
use quick_xml::de::from_str;
use serde::Deserialize;
use std::env;
use std::error::Error;
use websocket::client::ClientBuilder;
use websocket::OwnedMessage;

pub struct WebsocketClientConfig {
    host_address: String,
    host_port: u32,
    login_code: String,
    state_client: Option<StateClient>,
}

impl WebsocketClientConfig {
    pub fn new(
        host_address: String,
        host_port: u32,
        login_code: String,
        state_client: Option<StateClient>,
    ) -> Result<Self, Box<dyn Error>> {
        let config = Self {
            host_address,
            host_port,
            login_code,
            state_client,
        };

        Ok(config)
    }

    pub fn from_env(state_client: Option<StateClient>) -> Result<Self, Box<dyn Error>> {
        let host_address =
            env::var("WEBSOCKET_HOST_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
        let host_port: u32 = env::var("WEBSOCKET_HOST_PORT")
            .unwrap_or_else(|_| "8214".to_string())
            .parse()?;
        let login_code = env::var("WEBSOCKET_LOGIN_CODE")?;

        Self::new(host_address, host_port, login_code, state_client)
    }
}

pub struct WebsocketClient {
    config: WebsocketClientConfig,
}

#[async_trait]
impl PlannerClient<Config> for WebsocketClient {
    async fn plan(
        &self,
        config: Config,
        spot_price_planner: SpotPricePlanner,
        spot_prices: Vec<SpotPrice>,
    ) -> Result<(), Box<dyn Error>> {
        info!("Planning best time to heat tap water for alpha innotec heatpump...");

        let now = Utc::now();

        let state = if let Some(state_client) = &self.config.state_client {
            state_client.read_state()?
        } else {
            None
        };

        let current_desinfection_enabled = match &state {
            Some(st) => st.desinfection_enabled,
            None => false,
        };

        let desinfection_finished_at = match state {
            Some(st) => match st.desinfection_finished_at {
                Some(fa) => fa,
                None => now - Duration::days(7),
            },
            None => now - Duration::days(7),
        };

        // get best time in next 12 hours
        let before = now + Duration::hours(12);

        let best_spot_prices =
            spot_price_planner.get_best_spot_prices(&spot_prices, None, Some(before))?;

        if !best_spot_prices.is_empty() {
            info!(
                "Found block of {} spot price slots to use for planning heating of tap water:\n{:?}",
                best_spot_prices.len(),
                best_spot_prices
            );

            let desinfection_desired = best_spot_prices
                .first()
                .unwrap()
                .from
                .with_timezone(&config.local_time_zone.parse::<Tz>()?)
                .weekday()
                == config.desinfection_day_of_week
                && desinfection_finished_at < now;

            let connection = ClientBuilder::new(&format!(
                "ws://{}:{}",
                self.config.host_address, self.config.host_port
            ))?
            .origin(format!("http://{}", self.config.host_address))
            .add_protocol("Lux_WS")
            .connect_insecure()?;

            let (mut receiver, mut sender) = connection.split()?;

            let navigation = self.login(&mut receiver, &mut sender)?;

            self.set_schedule_from_best_spot_prices(
                &mut receiver,
                &mut sender,
                &navigation,
                config,
                &best_spot_prices,
            )?;

            if desinfection_desired != current_desinfection_enabled {
                // toggle continuous desinfection program
                self.toggle_continuous_desinfection(&mut receiver, &mut sender, &navigation)?;
            }

            if let Some(state_client) = &self.config.state_client {
                state_client
                    .store_state(&State {
                        desinfection_enabled: desinfection_desired,
                        desinfection_finished_at: Some(best_spot_prices.last().unwrap().till),
                    })
                    .await?;
            }

            Ok(())
        } else {
            info!("No available best spot prices, not updating heatpump tap water schedule.");
            Ok(())
        }
    }
}

impl WebsocketClient {
    pub fn new(config: WebsocketClientConfig) -> Self {
        Self { config }
    }

    fn send(
        &self,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        message: websocket::OwnedMessage,
    ) -> Result<(), Box<dyn Error>> {
        let _ = sender.send_message(&message)?;

        Ok(())
    }

    fn send_and_await(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        message: websocket::OwnedMessage,
    ) -> Result<String, Box<dyn Error>> {
        let _ = sender.send_message(&message)?;

        for message in receiver.incoming_messages() {
            match message? {
                OwnedMessage::Text(text) => {
                    return Ok(text);
                }
                OwnedMessage::Close(_) => {
                    // return a close
                    sender.send_message(&OwnedMessage::Close(None))?;
                }
                OwnedMessage::Ping(data) => {
                    // return a pong
                    sender.send_message(&OwnedMessage::Pong(data))?;
                }
                OwnedMessage::Pong(_) => {}
                OwnedMessage::Binary(_) => {}
            }
        }

        Err(Box::<dyn Error>::from(
            "No response received for login message",
        ))
    }

    fn login(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
    ) -> Result<Navigation, Box<dyn Error>> {
        let response_message = self.send_and_await(
            receiver,
            sender,
            websocket::OwnedMessage::Text(format!("LOGIN;{}", self.config.login_code)),
        )?;

        let navigation = self.get_navigation_from_response(response_message)?;

        Ok(navigation)
    }

    fn get_navigation_from_response(
        &self,
        response_message: String,
    ) -> Result<Navigation, Box<dyn Error>> {
        let navigation: Navigation = from_str(&response_message)?;

        Ok(navigation)
    }

    fn move_right(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
    ) -> Result<(), Box<dyn Error>> {
        debug!("Move right/down");
        self.send_and_await(
            receiver,
            sender,
            websocket::OwnedMessage::Text("MOVE;0".to_string()),
        )?;
        self.send_and_await(
            receiver,
            sender,
            websocket::OwnedMessage::Text("MOVE;6".to_string()),
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    fn move_left(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
    ) -> Result<(), Box<dyn Error>> {
        debug!("Move left/up");
        self.send_and_await(
            receiver,
            sender,
            websocket::OwnedMessage::Text("MOVE;1".to_string()),
        )?;
        self.send_and_await(
            receiver,
            sender,
            websocket::OwnedMessage::Text("MOVE;6".to_string()),
        )?;
        Ok(())
    }

    fn click(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
    ) -> Result<(), Box<dyn Error>> {
        debug!("Click");
        self.send_and_await(
            receiver,
            sender,
            websocket::OwnedMessage::Text("MOVE;2".to_string()),
        )?;
        self.send_and_await(
            receiver,
            sender,
            websocket::OwnedMessage::Text("MOVE;6".to_string()),
        )?;
        Ok(())
    }

    fn toggle_continuous_desinfection(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        navigation: &Navigation,
    ) -> Result<(), Box<dyn Error>> {
        info!("Toggling continuous desinfection");

        info!("To Afstandbediening");
        self.navigate_to(receiver, sender, navigation, "Afstandbediening")?;

        // to menu
        info!("To menu");
        self.click(receiver, sender)?;

        // to warmwater
        info!("To warmwater");
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.click(receiver, sender)?;

        // to onderhoudsprogramma
        info!("To onderhoudsprogramma");
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.click(receiver, sender)?;

        // to thermische desinfectie
        info!("To thermische desinfectie");
        self.click(receiver, sender)?;

        // to continu
        info!("To continu");
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;

        // check/uncheck continu
        info!("Toggle continu checkbox");
        self.click(receiver, sender)?;

        // apply
        info!("Apply changes");
        self.move_right(receiver, sender)?;
        self.click(receiver, sender)?;

        // back to home
        info!("To home");
        self.click(receiver, sender)?;
        self.click(receiver, sender)?;
        self.click(receiver, sender)?;
        self.click(receiver, sender)?;

        Ok(())
    }

    fn navigate_to(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        navigation: &Navigation,
        nav: &str,
    ) -> Result<String, Box<dyn Error>> {
        debug!("Navigate to '{}'", nav);
        let navigation_id = navigation.get_navigation_item_id(nav)?;
        let response_message = self.send_and_await(
            receiver,
            sender,
            websocket::OwnedMessage::Text(format!("GET;{}", navigation_id)),
        )?;
        debug!("Retrieved response from '{}':\n{}", &nav, response_message);

        Ok(response_message)
    }

    fn set_schedule_from_best_spot_prices(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        navigation: &Navigation,
        config: Config,
        best_spot_prices: &[SpotPrice],
    ) -> Result<(), Box<dyn Error>> {
        let response_message = self.navigate_to(
            receiver,
            sender,
            navigation,
            "Klokprogramma > Warmwater > Week",
        )?;
        let content: Content = from_str(&response_message).unwrap();
        debug!("Deserialized response:\n{:?}", content);

        // set all items to 0
        for item in &content.item.item {
            debug!("Setting {} to 00:00 - 00:00", item.name);
            self.send(
                sender,
                websocket::OwnedMessage::Text(format!("SET;set_{};{}", item.id, 0)),
            )?;
        }

        // get start time from first spot price
        if !best_spot_prices.is_empty() && content.item.item.len() > 1 {
            let heatpump_time_zone = config.heatpump_time_zone.parse::<Tz>()?;

            let from_hour = best_spot_prices
                .first()
                .unwrap()
                .from
                .with_timezone(&heatpump_time_zone)
                .hour();
            let first_item_id = content.item.item.first().unwrap().id.clone();
            debug!("Setting 1) to 00:00 - {}:00", from_hour);
            self.send(
                sender,
                websocket::OwnedMessage::Text(format!(
                    "SET;set_{};{}",
                    first_item_id,
                    65536 * 60 * from_hour
                )),
            )?;

            let till_hour = best_spot_prices
                .last()
                .unwrap()
                .till
                .with_timezone(&heatpump_time_zone)
                .hour();
            let last_item_id = content.item.item.last().unwrap().id.clone();
            debug!("Setting 5) to {}:00 - 00:00", till_hour);
            self.send(
                sender,
                websocket::OwnedMessage::Text(format!(
                    "SET;set_{};{}",
                    last_item_id,
                    60 * till_hour
                )),
            )?;
        }

        debug!("Saving changes");
        self.send(sender, websocket::OwnedMessage::Text("SAVE;1".to_string()))?;

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct Navigation {
    // id: String, // `xml:"id,attr"`
    #[serde(rename = "item", default)]
    items: Vec<NavigationItem>, // `xml:"item"`
}

#[derive(Debug, Deserialize)]
struct NavigationItem {
    id: String,   //           `xml:"id,attr"`
    name: String, //           `xml:"name"`
    #[serde(rename = "item", default)]
    items: Vec<NavigationItem>, // `xml:"item"`
}

impl Navigation {
    fn get_navigation_item_id(&self, item_path: &str) -> Result<String, Box<dyn Error>> {
        let item_path_parts: Vec<&str> = item_path.split(" > ").collect();

        let mut navigation_id: String = "".to_string();
        let mut items = &self.items;

        for part in item_path_parts.iter() {
            let mut exists = false;
            for item in items.iter() {
                if *part == item.name {
                    exists = true;

                    navigation_id = item.id.clone();
                    items = &item.items;

                    break;
                }
            }

            if !exists {
                return Err(Box::<dyn Error>::from(format!(
                    "Item {} does not exist",
                    part
                )));
            }
        }

        Ok(navigation_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_lib::model::SpotPrice;

    #[test]
    fn deserialize_navigation_xml() {
        let xml_string = "<Navigation id=\"0x45cd88\"><item id=\"0x45df90\"><name>Informatie</name><item id=\"0x45df90\"><name>Temperaturen</name></item><item id=\"0x455968\"><name>Ingangen</name></item></item><item id=\"0x450798\"><name>Instelling</name></item><item id=\"0x3dc420\"><name>Klokprogramma</name></item><item id=\"0x45c7b0\"><name>Toegang: Gebruiker</name></item></Navigation>";

        // act
        let navigation: Navigation = from_str(xml_string).unwrap();

        assert_eq!(navigation.items.len(), 4);
        assert_eq!(navigation.items[0].name, "Informatie".to_string());
        assert_eq!(navigation.items[0].items.len(), 2);
        assert_eq!(
            navigation.items[0].items[0].name,
            "Temperaturen".to_string()
        );
        assert_eq!(navigation.items[0].items[0].id, "0x45df90".to_string());
        assert_eq!(navigation.items[0].items[1].name, "Ingangen".to_string());
        assert_eq!(navigation.items[0].items[1].id, "0x455968".to_string());
    }

    #[test]
    fn get_navigation_item_id_returns_id_if_it_exists() {
        // <Navigation id='0x45cd88'><item id='0x45e068'><name>Informatie</name><item id='0x45df90'><name>Temperaturen</name></item><item id='0x455968'><name>Ingangen</name></item><item id='0x455760'><name>Uitgangen</name></item><item id='0x45bf10'><name>Aflooptijden</name></item><item id='0x456f08'><name>Bedrijfsuren</name></item><item id='0x4643a8'><name>Storingsbuffer</name></item><item id='0x3ddfa8'><name>Afschakelingen</name></item><item id='0x45d840'><name>Installatiestatus</name></item><item id='0x460cb8'><name>Energie</name></item><item id='0x4586a8'><name>GBS</name></item></item><item id='0x450798'><name>Instelling</name><item id='0x460bd0'><name>Bedrijfsmode</name></item><item id='0x461170'><name>Temperaturen</name></item><item id='0x462988'><name>Systeeminstelling</name></item></item><item id='0x3dc420'><name>Klokprogramma</name><readOnly>true</readOnly><item id='0x453560'><name>Verwarmen</name><readOnly>true</readOnly><item id='0x45e118'><name>Week</name></item><item id='0x45df00'><name>5+2</name></item><item id='0x45c200'><name>Dagen (Ma, Di,...)</name></item></item><item id='0x43e8e8'><name>Warmwater</name><readOnly>true</readOnly><item id='0x4642a8'><name>Week</name></item><item id='0x463940'><name>5+2</name></item><item id='0x463b68'><name>Dagen (Ma, Di,...)</name></item></item><item id='0x3dcc00'><name>Zwembad</name><readOnly>true</readOnly><item id='0x455580'><name>Week</name></item><item id='0x463f78'><name>5+2</name></item><item id='0x462690'><name>Dagen (Ma, Di,...)</name></item></item></item><item id='0x45c7b0'><name>Toegang: Gebruiker</name></item></Navigation>

        let navigation = Navigation {
            // id: "0x45cd88".to_string(),
            items: vec![
                NavigationItem {
                    id: "0x45df90".to_string(),
                    name: "Informatie".to_string(),
                    items: vec![
                        NavigationItem {
                            id: "0x45df90".to_string(),
                            name: "Temperaturen".to_string(),
                            items: vec![],
                        },
                        NavigationItem {
                            id: "0x455968".to_string(),
                            name: "Ingangen".to_string(),
                            items: vec![],
                        },
                    ],
                },
                NavigationItem {
                    id: "0x450798".to_string(),
                    name: "Instelling".to_string(),
                    items: vec![],
                },
                NavigationItem {
                    id: "0x3dc420".to_string(),
                    name: "Klokprogramma".to_string(),
                    items: vec![],
                },
                NavigationItem {
                    id: "0x45c7b0".to_string(),
                    name: "Toegang: Gebruiker".to_string(),
                    items: vec![],
                },
            ],
        };

        let item_id = navigation
            .get_navigation_item_id(&"Informatie".to_string())
            .unwrap();

        assert_eq!(item_id, "0x45df90".to_string());
    }

    #[test]
    fn get_navigation_item_id_returns_id_if_it_exists_as_nested_item_inside_top_level_item() {
        // <Navigation id='0x45cd88'><item id='0x45e068'><name>Informatie</name><item id='0x45df90'><name>Temperaturen</name></item><item id='0x455968'><name>Ingangen</name></item><item id='0x455760'><name>Uitgangen</name></item><item id='0x45bf10'><name>Aflooptijden</name></item><item id='0x456f08'><name>Bedrijfsuren</name></item><item id='0x4643a8'><name>Storingsbuffer</name></item><item id='0x3ddfa8'><name>Afschakelingen</name></item><item id='0x45d840'><name>Installatiestatus</name></item><item id='0x460cb8'><name>Energie</name></item><item id='0x4586a8'><name>GBS</name></item></item><item id='0x450798'><name>Instelling</name><item id='0x460bd0'><name>Bedrijfsmode</name></item><item id='0x461170'><name>Temperaturen</name></item><item id='0x462988'><name>Systeeminstelling</name></item></item><item id='0x3dc420'><name>Klokprogramma</name><readOnly>true</readOnly><item id='0x453560'><name>Verwarmen</name><readOnly>true</readOnly><item id='0x45e118'><name>Week</name></item><item id='0x45df00'><name>5+2</name></item><item id='0x45c200'><name>Dagen (Ma, Di,...)</name></item></item><item id='0x43e8e8'><name>Warmwater</name><readOnly>true</readOnly><item id='0x4642a8'><name>Week</name></item><item id='0x463940'><name>5+2</name></item><item id='0x463b68'><name>Dagen (Ma, Di,...)</name></item></item><item id='0x3dcc00'><name>Zwembad</name><readOnly>true</readOnly><item id='0x455580'><name>Week</name></item><item id='0x463f78'><name>5+2</name></item><item id='0x462690'><name>Dagen (Ma, Di,...)</name></item></item></item><item id='0x45c7b0'><name>Toegang: Gebruiker</name></item></Navigation>

        let navigation = Navigation {
            // id: "0x45cd88".to_string(),
            items: vec![
                NavigationItem {
                    id: "0x45df90".to_string(),
                    name: "Informatie".to_string(),
                    items: vec![
                        NavigationItem {
                            id: "0x45df90".to_string(),
                            name: "Temperaturen".to_string(),
                            items: vec![],
                        },
                        NavigationItem {
                            id: "0x455968".to_string(),
                            name: "Ingangen".to_string(),
                            items: vec![],
                        },
                    ],
                },
                NavigationItem {
                    id: "0x450798".to_string(),
                    name: "Instelling".to_string(),
                    items: vec![],
                },
                NavigationItem {
                    id: "0x3dc420".to_string(),
                    name: "Klokprogramma".to_string(),
                    items: vec![],
                },
                NavigationItem {
                    id: "0x45c7b0".to_string(),
                    name: "Toegang: Gebruiker".to_string(),
                    items: vec![],
                },
            ],
        };

        let item_id = navigation
            .get_navigation_item_id(&"Informatie > Ingangen".to_string())
            .unwrap();

        assert_eq!(item_id, "0x455968".to_string());
    }

    #[tokio::test]
    #[ignore]
    async fn update_schedule() -> Result<(), Box<dyn Error>> {
        let websocket_host_ip = env::var("WEBSOCKET_HOST_IP")?;
        let client = WebsocketClient::new(WebsocketClientConfig::from_env(None)?);

        let connection = ClientBuilder::new(&format!("ws://{}:{}", websocket_host_ip, 8214))?
            .origin(format!("http://{}", websocket_host_ip))
            .add_protocol("Lux_WS")
            .connect_insecure()?;

        let (mut receiver, mut sender) = connection.split()?;

        let navigation = client.login(&mut receiver, &mut sender)?;

        client.set_schedule_from_best_spot_prices(
            &mut receiver,
            &mut sender,
            &navigation,
            Config {
                local_time_zone: "Europe/Amsterdam".to_string(),
                heatpump_time_zone: "Europe/Amsterdam".to_string(),
                desinfection_day_of_week: Weekday::Sun,
            },
            &vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.ymd(2022, 4, 21).and_hms(13, 0, 0),
                    till: Utc.ymd(2022, 4, 21).and_hms(14, 0, 0),
                    market_price: 0.157,
                    market_price_tax: 0.0330708,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.081,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.ymd(2022, 4, 21).and_hms(14, 0, 0),
                    till: Utc.ymd(2022, 4, 21).and_hms(15, 0, 0),
                    market_price: 0.164,
                    market_price_tax: 0.0344316,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.081,
                },
            ],
        )?;

        Ok(())
    }

    #[tokio::test]
    #[ignore]
    async fn toggle_continuous_desinfection() -> Result<(), Box<dyn Error>> {
        let websocket_host_ip = env::var("WEBSOCKET_HOST_IP")?;
        let client = WebsocketClient::new(WebsocketClientConfig::from_env(None)?);

        let connection = ClientBuilder::new(&format!("ws://{}:{}", websocket_host_ip, 8214))?
            .origin(format!("http://{}", websocket_host_ip))
            .add_protocol("Lux_WS")
            .connect_insecure()?;

        let (mut receiver, mut sender) = connection.split()?;

        let navigation = client.login(&mut receiver, &mut sender)?;

        client.toggle_continuous_desinfection(&mut receiver, &mut sender, &navigation)?;

        Ok(())
    }
}
