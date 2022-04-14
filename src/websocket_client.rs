use crate::model::{Config, ConfigSample};
use jarvis_lib::planner_client::PlannerClient;
use jarvis_lib::model::SpotPricesState;

use chrono::Utc;
use log::{info, warn};
use regex::Regex;
use serde::Deserialize;
use serde_xml_rs::from_str;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use uuid::Uuid;

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
        spot_prices_state: Option<SpotPricesState>,
    ) -> Result<(), Box<dyn Error>> {
        info!("Planning best time to heat tap water for alpha innotec heatpump...")

        let connection = ClientBuilder::new(&format!(
            "ws://{}:{}",
            self.config.host_address, self.config.host_port
        ))?
        .origin(format!("http://{}", self.config.host_address))
        .add_protocol("Lux_WS")
        .connect_insecure()?;

        let (mut receiver, mut sender) = connection.split()?;

        // login
        let navigation = self.login(&mut receiver, &mut sender)?;

        // // get measurement samples
        // let grouped_sample_configs =
        //     self.group_sample_configs_per_navigation(config.sample_configs);

        // measurement.samples = self.get_samples(
        //     grouped_sample_configs,
        //     &mut receiver,
        //     &mut sender,
        //     navigation,
        // )?;

        // if let Some(lm) = last_measurement {
        //     measurement.samples = self.sanitize_samples(measurement.samples, lm.samples)
        // }

        // println!("Read measurement from alpha innotec heatpump");

        // Ok(measurement)

        Ok(())
    }
}

impl WebsocketClient {
    pub fn new(config: WebsocketClientConfig) -> Self {
        Self { config }
    }

    fn group_sample_configs_per_navigation(
        &self,
        sample_configs: Vec<ConfigSample>,
    ) -> HashMap<String, Vec<ConfigSample>> {
        let mut grouped_sample_configs: HashMap<String, Vec<ConfigSample>> = HashMap::new();

        for sample_config in sample_configs.into_iter() {
            if grouped_sample_configs
                .get(&sample_config.navigation)
                .is_none()
            {
                grouped_sample_configs.insert(sample_config.navigation.clone(), vec![]);
            }

            match grouped_sample_configs.get_mut(&sample_config.navigation) {
                Some(v) => {
                    v.push(sample_config);
                }
                None => {
                    grouped_sample_configs
                        .insert(sample_config.navigation.clone(), vec![sample_config]);
                }
            }
        }

        grouped_sample_configs
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

    fn get_samples(
        &self,
        grouped_sample_configs: HashMap<String, Vec<ConfigSample>>,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,

        navigation: Navigation,
    ) -> Result<Vec<Sample>, Box<dyn Error>> {
        let mut samples = Vec::new();

        for (nav, sample_configs) in grouped_sample_configs {
            println!("Fetching values from page {}...", nav);
            let navigation_id = navigation.get_navigation_item_id(&nav)?;
            let response_message = self.send_and_await(
                receiver,
                sender,
                websocket::OwnedMessage::Text(format!("GET;{}", navigation_id)),
            )?;

            println!(
                "Reading {} values from response for page {}...",
                sample_configs.len(),
                nav
            );
            for sample_config in sample_configs.iter() {
                let value = self.get_item_from_response(&sample_config.item, &response_message)?;

                samples.push(Sample {
                    entity_type: sample_config.entity_type,
                    entity_name: sample_config.entity_name.clone(),
                    sample_type: sample_config.sample_type,
                    sample_name: sample_config.sample_name.clone(),
                    metric_type: sample_config.metric_type,
                    value: value * sample_config.value_multiplier,
                });
            }
        }

        Ok(samples)
    }

    fn get_navigation_from_response(
        &self,
        response_message: String,
    ) -> Result<Navigation, Box<dyn Error>> {
        let navigation: Navigation = from_str(&response_message)?;

        Ok(navigation)
    }

    fn get_item_from_response(
        &self,
        item: &str,
        response_message: &str,
    ) -> Result<f64, Box<dyn Error>> {
        // <Content><item id='0x4816ac'><name>Aanvoer</name><value>22.0°C</value></item><item id='0x44fdcc'><name>Retour</name><value>22.0°C</value></item><item id='0x4807dc'><name>Retour berekend</name><value>23.0°C</value></item><item id='0x45e1bc'><name>Heetgas</name><value>38.0°C</value></item><item id='0x448894'><name>Buitentemperatuur</name><value>11.6°C</value></item><item id='0x48047c'><name>Gemiddelde temp.</name><value>13.1°C</value></item><item id='0x457724'><name>Tapwater gemeten</name><value>54.2°C</value></item><item id='0x45e97c'><name>Tapwater ingesteld</name><value>57.0°C</value></item><item id='0x45a41c'><name>Bron-in</name><value>10.5°C</value></item><item id='0x480204'><name>Bron-uit</name><value>10.3°C</value></item><item id='0x4803cc'><name>Menggroep2-aanvoer</name><value>22.0°C</value></item><item id='0x4609cc'><name>Menggr2-aanv.ingest.</name><value>19.0°C</value></item><item id='0x45a514'><name>Zonnecollector</name><value>5.0°C</value></item><item id='0x461ecc'><name>Zonneboiler</name><value>150.0°C</value></item><item id='0x4817a4'><name>Externe energiebron</name><value>5.0°C</value></item><item id='0x4646b4'><name>Aanvoer max.</name><value>66.0°C</value></item><item id='0x45e76c'><name>Zuiggasleiding comp.</name><value>19.4°C</value></item><item id='0x4607d4'><name>Comp. verwarming</name><value>37.7°C</value></item><item id='0x43e60c'><name>Oververhitting</name><value>4.8 K</value></item><name>Temperaturen</name></Content>

        let re = Regex::new(&format!(
            r"<item id='[^']*'><name>{}</name><value>(-?[0-9.]+|---)[^<]*</value></item>",
            item
        ))?;
        let matches = match re.captures(response_message) {
            Some(m) => m,
            None => {
                return Err(Box::<dyn Error>::from(format!(
                    "No match for item {}",
                    item
                )));
            }
        };

        if matches.len() != 2 {
            return Err(Box::<dyn Error>::from(format!(
                "No match for item {}",
                item
            )));
        }

        return match matches.get(1) {
            None => Ok(0.0),
            Some(m) => {
                let value = m.as_str();
                if value == "---" {
                    return Ok(0.0);
                }
                Ok(value.parse()?)
            }
        };
    }

    fn sanitize_samples(
        &self,
        current_samples: Vec<Sample>,
        last_samples: Vec<Sample>,
    ) -> Vec<Sample> {
        let mut sanitized_samples: Vec<Sample> = Vec::new();

        for current_sample in current_samples.into_iter() {
            // check if there's a corresponding sample in lastSamples and see if the difference with it's value isn't too large
            let mut sanitize = false;
            for last_sample in last_samples.iter() {
                if current_sample.entity_type == last_sample.entity_type
                    && current_sample.entity_name == last_sample.entity_name
                    && current_sample.sample_type == last_sample.sample_type
                    && current_sample.sample_name == last_sample.sample_name
                    && current_sample.metric_type == last_sample.metric_type
                {
                    if current_sample.metric_type == MetricType::Counter
                        && current_sample.value / last_sample.value > 1.1
                    {
                        sanitize = true;
                        println!("Value for {} is more than 10 percent larger than the last sampled value {}, keeping previous value instead", current_sample.sample_name, last_sample.value);
                        sanitized_samples.push(last_sample.clone());
                    }

                    break;
                }
            }

            if !sanitize {
                sanitized_samples.push(current_sample);
            }
        }

        sanitized_samples
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
    use jarvis_lib::model::{EntityType, MetricType, SampleType};

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
    fn get_item_from_response_returns_value_for_item_without_unit() {
        let websocket_client = WebsocketClient::new(
            WebsocketClientConfig::new("192.168.178.94".to_string(), 8214, "999999".to_string())
                .unwrap(),
        );

        let response_message = "<Content><item id='0x4816ac'><name>Aanvoer</name><value>22.3°C</value></item><item id='0x44fdcc'><name>Retour</name><value>22.0°C</value></item><item id='0x4807dc'><name>Retour berekend</name><value>23.0°C</value></item><item id='0x45e1bc'><name>Heetgas</name><value>38.0°C</value></item><item id='0x448894'><name>Buitentemperatuur</name><value>11.6°C</value></item><item id='0x48047c'><name>Gemiddelde temp.</name><value>13.1°C</value></item><item id='0x457724'><name>Tapwater gemeten</name><value>54.2°C</value></item><item id='0x45e97c'><name>Tapwater ingesteld</name><value>57.0°C</value></item><item id='0x45a41c'><name>Bron-in</name><value>10.5°C</value></item><item id='0x480204'><name>Bron-uit</name><value>10.3°C</value></item><item id='0x4803cc'><name>Menggroep2-aanvoer</name><value>22.0°C</value></item><item id='0x4609cc'><name>Menggr2-aanv.ingest.</name><value>19.0°C</value></item><item id='0x45a514'><name>Zonnecollector</name><value>5.0°C</value></item><item id='0x461ecc'><name>Zonneboiler</name><value>150.0°C</value></item><item id='0x4817a4'><name>Externe energiebron</name><value>5.0°C</value></item><item id='0x4646b4'><name>Aanvoer max.</name><value>66.0°C</value></item><item id='0x45e76c'><name>Zuiggasleiding comp.</name><value>19.4°C</value></item><item id='0x4607d4'><name>Comp. verwarming</name><value>37.7°C</value></item><item id='0x43e60c'><name>Oververhitting</name><value>4.8 K</value></item><name>Temperaturen</name></Content>".to_string();

        //act
        let value = websocket_client
            .get_item_from_response(&"Aanvoer".to_string(), &response_message)
            .unwrap();

        assert_eq!(value, 22.3);
    }

    #[test]
    #[should_panic]
    fn get_item_from_response_returns_error_if_item_id_is_not_in_response() {
        let websocket_client = WebsocketClient::new(
            WebsocketClientConfig::new("192.168.178.94".to_string(), 8214, "999999".to_string())
                .unwrap(),
        );

        let response_message = "<Content><item id='0x4816ac'><name>Aanvoer</name><value>22.3°C</value></item><item id='0x44fdcc'><name>Retour</name><value>22.0°C</value></item><item id='0x4807dc'><name>Retour berekend</name><value>23.0°C</value></item><item id='0x45e1bc'><name>Heetgas</name><value>38.0°C</value></item><item id='0x448894'><name>Buitentemperatuur</name><value>11.6°C</value></item><item id='0x48047c'><name>Gemiddelde temp.</name><value>13.1°C</value></item><item id='0x457724'><name>Tapwater gemeten</name><value>54.2°C</value></item><item id='0x45e97c'><name>Tapwater ingesteld</name><value>57.0°C</value></item><item id='0x45a41c'><name>Bron-in</name><value>10.5°C</value></item><item id='0x480204'><name>Bron-uit</name><value>10.3°C</value></item><item id='0x4803cc'><name>Menggroep2-aanvoer</name><value>22.0°C</value></item><item id='0x4609cc'><name>Menggr2-aanv.ingest.</name><value>19.0°C</value></item><item id='0x45a514'><name>Zonnecollector</name><value>5.0°C</value></item><item id='0x461ecc'><name>Zonneboiler</name><value>150.0°C</value></item><item id='0x4817a4'><name>Externe energiebron</name><value>5.0°C</value></item><item id='0x4646b4'><name>Aanvoer max.</name><value>66.0°C</value></item><item id='0x45e76c'><name>Zuiggasleiding comp.</name><value>19.4°C</value></item><item id='0x4607d4'><name>Comp. verwarming</name><value>37.7°C</value></item><item id='0x43e60c'><name>Oververhitting</name><value>4.8 K</value></item><name>Temperaturen</name></Content>".to_string();

        //act
        let _ = websocket_client
            .get_item_from_response(&"DoesNotExist".to_string(), &response_message)
            .unwrap();
    }

    #[test]
    fn get_item_from_response_returns_value_for_item_with_pressure_unit() {
        let websocket_client = WebsocketClient::new(
            WebsocketClientConfig::new("192.168.178.94".to_string(), 8214, "999999".to_string())
                .unwrap(),
        );

        let response_message = "<Content><item id='0x4e7944'><name>ASD</name><value>Aan</value></item><item id='0x4ffbfc'><name>EVU</name><value>Aan</value></item><item id='0x4ef3b4'><name>HD</name><value>Uit</value></item><item id='0x4dac64'><name>MOT</name><value>Aan</value></item><item id='0x4ca4c4'><name>SWT</name><value>Uit</value></item><item id='0x4fa864'><name>Analoog-In 21</name><value>0.00 V</value></item><item id='0x4d5f1c'><name>Analoog-In 22</name><value>0.00 V</value></item><item id='0x4e6a3c'><name>HD</name><value>8.10 bar</value></item><item id='0x4ca47c'><name>ND</name><value>8.38 bar</value></item><item id='0x4e8004'><name>Debiet</name><value>1200 l/h</value></item><name>Ingangen</name></Content>".to_string();

        //act
        let value = websocket_client
            .get_item_from_response(&"HD".to_string(), &response_message)
            .unwrap();

        assert_eq!(value, 8.10);
    }

    #[test]
    fn group_sample_configs_per_navigation_returns_hashmap_with_grouped_sample_configs() {
        let sample_configs: Vec<ConfigSample> = vec![
            ConfigSample {
                entity_type: EntityType::Device,
                entity_name: "Alpha Innotec SWCV 92K3".to_string(),
                sample_type: SampleType::Temperature,
                sample_name: "Aanvoer".to_string(),
                metric_type: MetricType::Gauge,
                value_multiplier: 1.0,
                navigation: "Informatie > Temperaturen".to_string(),
                item: "Aanvoer".to_string(),
            },
            ConfigSample {
                entity_type: EntityType::Device,
                entity_name: "Alpha Innotec SWCV 92K3".to_string(),
                sample_type: SampleType::Temperature,
                sample_name: "Retour".to_string(),
                metric_type: MetricType::Gauge,
                value_multiplier: 1.0,
                navigation: "Informatie > Temperaturen".to_string(),
                item: "Retour".to_string(),
            },
            ConfigSample {
                entity_type: EntityType::Device,
                entity_name: "Alpha Innotec SWCV 92K3".to_string(),
                sample_type: SampleType::Energy,
                sample_name: "Tapwater".to_string(),
                metric_type: MetricType::Counter,
                value_multiplier: 3600000.0,
                navigation: "Informatie > Energie".to_string(),
                item: "Warmwater".to_string(),
            },
        ];

        let websocket_client = WebsocketClient::new(
            WebsocketClientConfig::new("192.168.178.94".to_string(), 8214, "999999".to_string())
                .unwrap(),
        );

        let grouped_sample_configs =
            websocket_client.group_sample_configs_per_navigation(sample_configs);

        assert_eq!(grouped_sample_configs.len(), 2);
        assert_eq!(
            grouped_sample_configs
                .get(&"Informatie > Temperaturen".to_string())
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            grouped_sample_configs
                .get(&"Informatie > Energie".to_string())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    #[ignore]
    fn get_measurement() {
        let websocket_client = WebsocketClient::new(
            WebsocketClientConfig::new("192.168.195.4".to_string(), 8214, "999999".to_string())
                .unwrap(),
        );
        let config = Config {
            location: "My address".to_string(),
            sample_configs: vec![ConfigSample {
                entity_type: EntityType::Device,
                entity_name: "Alpha Innotec SWCV 92K3".to_string(),
                sample_type: SampleType::Temperature,
                sample_name: "Aanvoer".to_string(),
                metric_type: MetricType::Gauge,
                value_multiplier: 1.0,
                navigation: "Informatie > Temperaturen".to_string(),
                item: "Aanvoer".to_string(),
            }],
        };

        // act
        let measurement = websocket_client
            .get_measurement(config, Option::None)
            .unwrap();

        assert_eq!(measurement.samples.len(), 1);
        assert_eq!(
            measurement.samples[0].entity_name,
            "Alpha Innotec SWCV 92K3".to_string()
        );
        assert_eq!(measurement.samples[0].sample_name, "Aanvoer".to_string());
        assert_eq!(measurement.samples[0].metric_type, MetricType::Gauge);
        // assert_eq!(measurement.samples[0].value, 0.0);
    }
}

// func TestGetNavigationFromResponse(t *testing.T) {
// 	t.Run("ReturnsValueForItemWithoutUnit", func(t *testing.T) {

// 		client := client{}

// 		response := `<Navigation id='0x45cd88'><item id='0x45e068'><name>Informatie</name><item id='0x45df90'><name>Temperaturen</name></item><item id='0x455968'><name>Ingangen</name></item><item id='0x455760'><name>Uitgangen</name></item><item id='0x45bf10'><name>Aflooptijden</name></item><item id='0x456f08'><name>Bedrijfsuren</name></item><item id='0x4643a8'><name>Storingsbuffer</name></item><item id='0x3ddfa8'><name>Afschakelingen</name></item><item id='0x45d840'><name>Installatiestatus</name></item><item id='0x460cb8'><name>Energie</name></item><item id='0x4586a8'><name>GBS</name></item></item><item id='0x450798'><name>Instelling</name><item id='0x460bd0'><name>Bedrijfsmode</name></item><item id='0x461170'><name>Temperaturen</name></item><item id='0x462988'><name>Systeeminstelling</name></item></item><item id='0x3dc420'><name>Klokprogramma</name><readOnly>true</readOnly><item id='0x453560'><name>Verwarmen</name><readOnly>true</readOnly><item id='0x45e118'><name>Week</name></item><item id='0x45df00'><name>5+2</name></item><item id='0x45c200'><name>Dagen (Ma, Di,...)</name></item></item><item id='0x43e8e8'><name>Warmwater</name><readOnly>true</readOnly><item id='0x4642a8'><name>Week</name></item><item id='0x463940'><name>5+2</name></item><item id='0x463b68'><name>Dagen (Ma, Di,...)</name></item></item><item id='0x3dcc00'><name>Zwembad</name><readOnly>true</readOnly><item id='0x455580'><name>Week</name></item><item id='0x463f78'><name>5+2</name></item><item id='0x462690'><name>Dagen (Ma, Di,...)</name></item></item></item><item id='0x45c7b0'><name>Toegang: Gebruiker</name></item></Navigation>`

// 		// act
// 		navigation, err := client.getNavigationFromResponse([]byte(response))

// 		assert.Nil(t, err)
// 		assert.Equal(t, 4, len(navigation.Items))
// 		assert.Equal(t, "Informatie", navigation.Items[0].Name)
// 		assert.Equal(t, 10, len(navigation.Items[0].Items))
// 		assert.Equal(t, "Temperaturen", navigation.Items[0].Items[0].Name)
// 		assert.Equal(t, "0x45df90", navigation.Items[0].Items[0].ID)
// 	})
// }
