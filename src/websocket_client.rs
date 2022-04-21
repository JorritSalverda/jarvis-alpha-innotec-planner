use crate::model::{Config, Content};
// use chrono::{naive::NaiveTime, DateTime, Utc, Weekday};
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

#[derive(Debug)]
pub struct WebsocketClientConfig {
    host_address: String,
    host_port: u32,
    login_code: String,
}

impl WebsocketClientConfig {
    pub fn new(
        host_address: String,
        host_port: u32,
        login_code: String,
    ) -> Result<Self, Box<dyn Error>> {
        let config = Self {
            host_address,
            host_port,
            login_code,
        };

        println!("{:?}", config);

        Ok(config)
    }

    pub fn from_env() -> Result<Self, Box<dyn Error>> {
        let host_address =
            env::var("WEBSOCKET_HOST_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
        let host_port: u32 = env::var("WEBSOCKET_HOST_PORT")
            .unwrap_or_else(|_| "8214".to_string())
            .parse()?;
        let login_code = env::var("WEBSOCKET_LOGIN_CODE")?;

        Self::new(host_address, host_port, login_code)
    }
}

pub struct WebsocketClient {
    config: WebsocketClientConfig,
}

impl PlannerClient<Config> for WebsocketClient {
    fn plan(
        &self,
        config: Config,
        spot_price_planner: SpotPricePlanner,
        spot_prices: Vec<SpotPrice>,
    ) -> Result<(), Box<dyn Error>> {
        info!("Planning best time to heat tap water for alpha innotec heatpump...");

        // get best time in next 12 hours
        let before = Utc::now() + Duration::hours(12);

        let best_spot_prices =
            spot_price_planner.get_best_spot_prices(&spot_prices, None, Some(before))?;

        if !best_spot_prices.is_empty() {
            info!(
                "Found block of {} spot price slots to use for planning heating of tap water",
                best_spot_prices.len()
            );

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
                navigation,
                config,
                best_spot_prices,
            )?;

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

    fn set_schedule_from_best_spot_prices(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        navigation: Navigation,
        config: Config,
        best_spot_prices: Vec<SpotPrice>,
    ) -> Result<(), Box<dyn Error>> {
        // navigate to Klokprogramma > Warmwater > Week
        let nav = "Klokprogramma > Warmwater > Week".to_string();
        let navigation_id = navigation.get_navigation_item_id(&nav)?;
        let response_message = self.send_and_await(
            receiver,
            sender,
            websocket::OwnedMessage::Text(format!("GET;{}", navigation_id)),
        )?;
        debug!("Retrieved response from '{}':\n{}", &nav, response_message);

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
            let local_time_zone = config.local_time_zone.parse::<Tz>()?;

            let from_hour = best_spot_prices
                .first()
                .unwrap()
                .from
                .with_timezone(&local_time_zone)
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
                .with_timezone(&local_time_zone)
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
#[allow(dead_code)]
struct Navigation {
    // id: String, // `xml:"id,attr"`
    #[serde(rename = "item", default)]
    items: Vec<NavigationItem>, // `xml:"item"`
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct NavigationItem {
    id: String,   //           `xml:"id,attr"`
    name: String, //           `xml:"name"`
    #[serde(rename = "item", default)]
    items: Vec<NavigationItem>, // `xml:"item"`
}

impl Navigation {
    #[allow(dead_code)]
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

    #[test]
    #[ignore]
    fn update_schedule() -> Result<(), Box<dyn Error>> {
        let websocket_host_ip = env::var("WEBSOCKET_HOST_IP")?;
        let client = WebsocketClient::new(WebsocketClientConfig::from_env()?);

        let connection = ClientBuilder::new(&format!("ws://{}:{}", websocket_host_ip, 8214))?
            .origin(format!("http://{}", websocket_host_ip))
            .add_protocol("Lux_WS")
            .connect_insecure()?;

        let (mut receiver, mut sender) = connection.split()?;

        let navigation = client.login(&mut receiver, &mut sender)?;

        client.set_schedule_from_best_spot_prices(
            &mut receiver,
            &mut sender,
            navigation,
            Config {
                local_time_zone: "Europe/Amsterdam".to_string(),
            },
            vec![
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

        // 00:00 - 00:00 => SET;set_0xa84d74;0
        // 00:00 - 01:00 => SET;set_0xa8820c;3932160
        // 00:00 - 02:00 => SET;set_0xa8820c;7864320
        // 00:00 - 03:00 => SET;set_0xa8820c;11796480
        // 00:00 - 04:00 => SET;set_0xa8820c;15728640
        // 00:00 - 05:00 => SET;set_0xa8820c;19660800
        // 00:00 - 06:00 => SET;set_0xa8820c;23592960
        // 00:00 - 07:00 => SET;set_0xa8820c;27525120
        // 00:00 - 08:00 => SET;set_0xa8820c;31457280
        // 00:00 - 09:00 => SET;set_0xa8820c;35389440
        // 00:00 - 10:00 => SET;set_0xa8820c;39321600
        // 00:00 - 11:00 => SET;set_0xa8820c;43253760
        // 00:00 - 12:00 => SET;set_0xa8820c;47185920
        // 00:00 - 13:00 => SET;set_0xa8820c;51118080
        // 00:00 - 14:00 => SET;set_0xa8820c;55050240
        // 00:00 - 15:00 => SET;set_0xa8820c;58982400
        // 00:00 - 16:00 => SET;set_0xa8820c;62914560
        // 00:00 - 17:00 => SET;set_0xa8820c;66846720
        // 00:00 - 18:00 => SET;set_0xa8820c;70778880
        // 00:00 - 19:00 => SET;set_0xa8820c;74711040
        // 00:00 - 20:00 => SET;set_0xa8820c;78643200
        // 00:00 - 21:00 => SET;set_0xa8820c;82575360
        // 00:00 - 22:00 => SET;set_0xa8820c;86507520
        // 00:00 - 23:00 => SET;set_0xa8820c;90439680

        // 00:00 - 10:00 => SET;set_0xa8820c;39321600
        // 01:00 - 10:00 => SET;set_0xa8820c;39321660
        // 02:00 - 10:00 => SET;set_0xa8820c;39321720
        // 03:00 - 10:00 => SET;set_0xa8820c;39321780
        //   03:01 - 10:00 => SET;set_0xa8820c;39321781
        //   03:00 - 10:01 => SET;set_0xa8820c;39387316
        // 04:00 - 10:00 => SET;set_0xa8820c;39321840
        // 05:00 - 10:00 => SET;set_0xa8820c;39321900
        // 06:00 - 10:00 => SET;set_0xa8820c;39321960
        // 07:00 - 10:00 => SET;set_0xa8820c;39322020
        // 08:00 - 10:00 => SET;set_0xa8820c;39322080
        // 09:00 - 10:00 => SET;set_0xa8820c;39322140
        // 10:00 - 10:00 => SET;set_0xa8820c;39322200

        // from: add 1 per minute
        // till: add 65536 per minute

        Ok(())
    }
}
