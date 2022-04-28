use crate::model::{Config, Content, State, TimeSlot};
use crate::state_client::StateClient;
use async_trait::async_trait;
use chrono::{prelude::*, Duration, Utc};
use chrono_tz::Tz;
use jarvis_lib::model::{SpotPrice, SpotPricePlanner};
use jarvis_lib::planner_client::PlannerClient;
use log::{debug, info};
use quick_xml::de::from_str;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use websocket::client::ClientBuilder;
use websocket::OwnedMessage;

const MAXIMUM_TAP_WATER_TEMPERATURE: f64 = 58.0;

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

        debug!("state: {:?}", state);

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

        // get best time in next n hours
        let before = now + Duration::hours(config.maximum_hours_to_plan_ahead as i64);

        let best_spot_prices =
            spot_price_planner.get_best_spot_prices(&spot_prices, None, Some(before))?;

        if !best_spot_prices.is_empty() {
            info!(
                "Found block of {} spot price slots to use for planning heating of tap water:\n{:?}",
                best_spot_prices.len(),
                best_spot_prices
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
                &navigation,
                &config,
                &best_spot_prices,
            )?;

            info!("Checking if desinfection is needed");
            let desinfection_desired = are_spot_prices_in_time_slot(
                config.get_local_time_zone()?,
                &best_spot_prices,
                &config.desinfection_local_time_slots,
            )? && desinfection_finished_at
                < now - Duration::days(config.minimal_days_between_desinfection as i64);

            if desinfection_desired && !current_desinfection_enabled {
                info!("Enabling desinfection mode");
                self.toggle_continuous_desinfection(&mut receiver, &mut sender, &navigation)?;
            } else if !desinfection_desired && current_desinfection_enabled {
                info!("Disabling desinfection mode");
                self.toggle_continuous_desinfection(&mut receiver, &mut sender, &navigation)?;
            } else if desinfection_desired {
                info!("No need to update desinfection mode, it's already enabled");
            } else {
                info!("No need to update desinfection mode, it's already disabled");
            }

            let desired_tap_water_temperature = if desinfection_desired {
                MAXIMUM_TAP_WATER_TEMPERATURE
            } else {
                config.desired_tap_water_temperature
            };

            self.set_tap_water_temperature(
                &mut receiver,
                &mut sender,
                &navigation,
                desired_tap_water_temperature,
            )?;

            let mut desinfection_finished_at = desinfection_finished_at;
            if desinfection_desired {
                desinfection_finished_at = best_spot_prices.last().unwrap().till;
            }

            if let Some(state_client) = &self.config.state_client {
                state_client
                    .store_state(&State {
                        desinfection_enabled: desinfection_desired,
                        desinfection_finished_at: Some(desinfection_finished_at),
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

    pub fn from_env(state_client: Option<StateClient>) -> Result<Self, Box<dyn Error>> {
        Ok(Self::new(WebsocketClientConfig::from_env(state_client)?))
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
        debug!("Retrieved response for login:\n{}", response_message);

        let navigation = self.get_navigation_from_response(response_message)?;

        Ok(navigation)
    }

    fn toggle_continuous_desinfection(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        navigation: &Navigation,
    ) -> Result<(), Box<dyn Error>> {
        info!("Toggling continuous desinfection");

        debug!("To Afstandbediening");
        self.navigate_to(receiver, sender, navigation, "Afstandbediening")?;

        // to menu
        debug!("To menu");
        self.click(receiver, sender)?;

        // to warmwater
        debug!("To warmwater");
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.click(receiver, sender)?;

        // to onderhoudsprogramma
        debug!("To onderhoudsprogramma");
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.click(receiver, sender)?;

        // to thermische desinfectie
        debug!("To thermische desinfectie");
        self.click(receiver, sender)?;

        // to continu
        debug!("To continu");
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;
        self.move_right(receiver, sender)?;

        // check/uncheck continu
        debug!("Toggle continu checkbox");
        self.click(receiver, sender)?;

        // apply
        debug!("Apply changes");
        self.move_right(receiver, sender)?;
        self.click(receiver, sender)?;

        // back to home
        debug!("To home");
        self.click(receiver, sender)?;
        self.click(receiver, sender)?;
        self.click(receiver, sender)?;
        self.click(receiver, sender)?;

        Ok(())
    }

    fn set_tap_water_temperature(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        navigation: &Navigation,
        desired_tap_water_temperature: f64,
    ) -> Result<(), Box<dyn Error>> {
        // get current set tap water temperature
        let response_message =
            self.navigate_to(receiver, sender, navigation, "Informatie > Temperaturen")?;

        let value = self.get_item_from_response("Tapwater ingesteld", &response_message)?;
        if value != desired_tap_water_temperature {
            debug!("To Afstandbediening");
            self.navigate_to(receiver, sender, navigation, "Afstandbediening")?;

            // to menu
            debug!("To menu");
            self.click(receiver, sender)?;

            // to warmwater
            debug!("To warmwater");
            self.move_right(receiver, sender)?;
            self.move_right(receiver, sender)?;
            self.move_right(receiver, sender)?;
            self.click(receiver, sender)?;

            // to temperatuur
            debug!("To temperatuur");
            self.move_right(receiver, sender)?;
            self.click(receiver, sender)?;

            // to gewenste waarde
            debug!("To gewenste waarde");
            self.click(receiver, sender)?;

            // raise / lower temperature
            let desired_tap_water_temperature_diff = desired_tap_water_temperature - value;
            if desired_tap_water_temperature_diff > 0.0 {
                let temperature_increments = (desired_tap_water_temperature_diff / 0.5) as i64;
                debug!(
                    "Raising temperature by {} increments of 0.5°C",
                    temperature_increments
                );
                for _n in 0..temperature_increments {
                    self.move_right(receiver, sender)?;
                }
                self.click(receiver, sender)?;
            } else {
                let temperature_decrements =
                    (-1.0 * desired_tap_water_temperature_diff / 0.5) as i64;
                debug!(
                    "Lowering temperature by {} decrements of 0.5°C",
                    temperature_decrements
                );
                for _n in 0..temperature_decrements {
                    self.move_left(receiver, sender)?;
                }
                self.click(receiver, sender)?;
            }

            // apply
            debug!("Apply changes");
            self.click(receiver, sender)?;

            // back to home
            debug!("To home");
            self.click(receiver, sender)?;
            self.click(receiver, sender)?;
            self.click(receiver, sender)?;

            info!(
                "Finished updating tap water temperature to {}°C",
                desired_tap_water_temperature
            )
        } else {
            info!(
                "Set tap water temperature is already at {}°C, no need to update it",
                value
            )
        }

        Ok(())
    }

    fn set_schedule_from_best_spot_prices(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        navigation: &Navigation,
        config: &Config,
        best_spot_prices: &[SpotPrice],
    ) -> Result<(), Box<dyn Error>> {
        info!("Updating tap water heating schedule from best spot prices");
        let response_message = self.navigate_to(
            receiver,
            sender,
            navigation,
            "Klokprogramma > Warmwater > Week",
        )?;
        let content: Content = from_str(&response_message).unwrap();
        debug!("Deserialized response:\n{:?}", content);

        // set all items to 0
        info!("Resetting schedule");
        for item in &content.item.item {
            debug!("Setting {} to 00:00 - 00:00", item.name);
            self.send(
                sender,
                websocket::OwnedMessage::Text(format!("SET;set_{};{}", item.id, 0)),
            )?;
        }

        if !best_spot_prices.is_empty() && content.item.item.len() > 1 {
            let heatpump_time_zone = config.get_heatpump_time_zone()?;

            // get start time from first spot price
            let from_hour = best_spot_prices
                .first()
                .unwrap()
                .from
                .with_timezone(&heatpump_time_zone)
                .hour();

            // get finish time from last spot price
            let till_hour = best_spot_prices
                .last()
                .unwrap()
                .till
                .with_timezone(&heatpump_time_zone)
                .hour();

            if from_hour > till_hour {
                // starts before midnight, finishes after
                let first_item_id = content.item.item.first().unwrap().id.clone();
                info!("Setting 1) to block {}:00 - {}:00", till_hour, from_hour);
                self.send(
                    sender,
                    websocket::OwnedMessage::Text(format!(
                        "SET;set_{};{}",
                        first_item_id,
                        60 * till_hour + 65536 * 60 * from_hour
                    )),
                )?;
            } else {
                // start and finish on same day
                if from_hour > 0 {
                    let first_item_id = content.item.item.first().unwrap().id.clone();
                    info!("Setting 1) to block 00:00 - {}:00", from_hour);
                    self.send(
                        sender,
                        websocket::OwnedMessage::Text(format!(
                            "SET;set_{};{}",
                            first_item_id,
                            65536 * 60 * from_hour
                        )),
                    )?;
                }

                if till_hour > 0 {
                    let last_item_id = content.item.item.last().unwrap().id.clone();
                    info!("Setting 5) to block {}:00 - 00:00", till_hour);
                    self.send(
                        sender,
                        websocket::OwnedMessage::Text(format!(
                            "SET;set_{};{}",
                            last_item_id,
                            60 * till_hour
                        )),
                    )?;
                }
            }
        }

        info!("Saving changes");
        self.send_and_await(
            receiver,
            sender,
            websocket::OwnedMessage::Text("SAVE;1".to_string()),
        )?;

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
        debug!(
            "Retrieved response from '{}' ({}):\n{}",
            &nav, navigation_id, response_message
        );

        Ok(response_message)
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
}

fn are_spot_prices_in_time_slot(
    local_time_zone: Tz,
    spot_prices: &[SpotPrice],
    local_time_slots: &HashMap<Weekday, Vec<TimeSlot>>,
) -> Result<bool, Box<dyn Error>> {
    info!("Checking if spot prices are in time slots");
    debug!("local_time_zone: {:?}", local_time_zone);
    debug!("spot_prices: {:?}", spot_prices);
    debug!("local_time_slots: {:?}", local_time_slots);

    Ok(spot_prices.iter().all(|spot_price| {
        let local_from = spot_price.from.with_timezone(&local_time_zone);
        let local_till = spot_price.till.with_timezone(&local_time_zone);

        if let Some(time_slots) = local_time_slots.get(&local_from.weekday()) {
            time_slots.iter().any(|time_slot| {
                if let Some(price_limit) = time_slot.if_price_below {
                    if spot_price.total_price() >= price_limit {
                        return false;
                    }
                }

                let time_slot_from = local_from.date().and_hms(
                    time_slot.from.hour(),
                    time_slot.from.minute(),
                    time_slot.from.second(),
                );

                let time_slot_till = if time_slot.till.hour() > 0 {
                    local_from.date().and_hms(
                        time_slot.till.hour(),
                        time_slot.till.minute(),
                        time_slot.till.second(),
                    )
                } else {
                    local_from.date().and_hms(
                        time_slot.till.hour(),
                        time_slot.till.minute(),
                        time_slot.till.second(),
                    ) + Duration::days(1)
                };

                local_from >= time_slot_from
                    && local_from < time_slot_till
                    && local_till > time_slot_from
                    && local_till <= time_slot_till
            })
        } else {
            false
        }
    }))
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
            &Config {
                local_time_zone: "Europe/Amsterdam".to_string(),
                heatpump_time_zone: "Europe/Amsterdam".to_string(),
                desinfection_local_time_slots: HashMap::new(),
                desired_tap_water_temperature: 50.0,
                maximum_hours_to_plan_ahead: 12,
                minimal_days_between_desinfection: 4,
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
    async fn set_tap_water_temperature() -> Result<(), Box<dyn Error>> {
        let websocket_host_ip = env::var("WEBSOCKET_HOST_IP")?;
        let client = WebsocketClient::new(WebsocketClientConfig::from_env(None)?);

        let connection = ClientBuilder::new(&format!("ws://{}:{}", websocket_host_ip, 8214))?
            .origin(format!("http://{}", websocket_host_ip))
            .add_protocol("Lux_WS")
            .connect_insecure()?;

        let (mut receiver, mut sender) = connection.split()?;

        let navigation = client.login(&mut receiver, &mut sender)?;

        client.set_tap_water_temperature(&mut receiver, &mut sender, &navigation, 50.0)?;

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

    #[test]
    fn are_spot_prices_in_time_slot_returns_true_if_all_spot_prices_fit_inside_time_slots(
    ) -> Result<(), Box<dyn Error>> {
        let local_time_zone = "Europe/Amsterdam".parse::<Tz>()?;

        let spot_prices = vec![
            SpotPrice {
                id: None,
                source: None,
                from: Utc.ymd(2022, 4, 23).and_hms(11, 0, 0),
                till: Utc.ymd(2022, 4, 23).and_hms(12, 0, 0),
                market_price: -0.222,
                market_price_tax: -0.0466956,
                sourcing_markup_price: 0.017,
                energy_tax_price: 0.081,
            },
            SpotPrice {
                id: None,
                source: None,
                from: Utc.ymd(2022, 4, 23).and_hms(12, 0, 0),
                till: Utc.ymd(2022, 4, 23).and_hms(13, 0, 0),
                market_price: -0.217,
                market_price_tax: -0.0456582,
                sourcing_markup_price: 0.017,
                energy_tax_price: 0.081,
            },
        ];

        let local_time_slots = HashMap::from([
            (
                Weekday::Fri,
                vec![TimeSlot {
                    from: NaiveTime::from_hms(7, 0, 0),
                    till: NaiveTime::from_hms(19, 0, 0),
                    if_price_below: Some(0.0),
                }],
            ),
            (
                Weekday::Sat,
                vec![TimeSlot {
                    from: NaiveTime::from_hms(7, 0, 0),
                    till: NaiveTime::from_hms(19, 0, 0),
                    if_price_below: Some(0.1),
                }],
            ),
            (
                Weekday::Sun,
                vec![TimeSlot {
                    from: NaiveTime::from_hms(7, 0, 0),
                    till: NaiveTime::from_hms(19, 0, 0),
                    if_price_below: None,
                }],
            ),
        ]);

        let result =
            are_spot_prices_in_time_slot(local_time_zone, &spot_prices, &local_time_slots)?;

        assert_eq!(result, true);

        Ok(())
    }

    #[test]
    fn are_spot_prices_in_time_slot_returns_false_if_not_all_spot_prices_fit_inside_time_slots(
    ) -> Result<(), Box<dyn Error>> {
        let local_time_zone = "Europe/Amsterdam".parse::<Tz>()?;

        let spot_prices = vec![
            SpotPrice {
                id: None,
                source: None,
                from: Utc.ymd(2022, 4, 23).and_hms(11, 0, 0),
                till: Utc.ymd(2022, 4, 23).and_hms(12, 0, 0),
                market_price: -0.222,
                market_price_tax: -0.0466956,
                sourcing_markup_price: 0.017,
                energy_tax_price: 0.081,
            },
            SpotPrice {
                id: None,
                source: None,
                from: Utc.ymd(2022, 4, 23).and_hms(12, 0, 0),
                till: Utc.ymd(2022, 4, 23).and_hms(13, 0, 0),
                market_price: -0.217,
                market_price_tax: -0.0456582,
                sourcing_markup_price: 0.017,
                energy_tax_price: 0.081,
            },
        ];

        let local_time_slots = HashMap::from([(
            Weekday::Sat,
            vec![TimeSlot {
                from: NaiveTime::from_hms(7, 0, 0),
                till: NaiveTime::from_hms(14, 0, 0),
                if_price_below: Some(0.1),
            }],
        )]);

        let result =
            are_spot_prices_in_time_slot(local_time_zone, &spot_prices, &local_time_slots)?;

        assert_eq!(result, false);

        Ok(())
    }

    #[test]
    fn are_spot_prices_in_time_slot_returns_false_if_no_spot_prices_fit_inside_time_slots(
    ) -> Result<(), Box<dyn Error>> {
        let local_time_zone = "Europe/Amsterdam".parse::<Tz>()?;

        let spot_prices = vec![
            SpotPrice {
                id: None,
                source: None,
                from: Utc.ymd(2022, 4, 23).and_hms(11, 0, 0),
                till: Utc.ymd(2022, 4, 23).and_hms(12, 0, 0),
                market_price: -0.222,
                market_price_tax: -0.0466956,
                sourcing_markup_price: 0.017,
                energy_tax_price: 0.081,
            },
            SpotPrice {
                id: None,
                source: None,
                from: Utc.ymd(2022, 4, 23).and_hms(12, 0, 0),
                till: Utc.ymd(2022, 4, 23).and_hms(13, 0, 0),
                market_price: -0.217,
                market_price_tax: -0.0456582,
                sourcing_markup_price: 0.017,
                energy_tax_price: 0.081,
            },
        ];

        let local_time_slots: HashMap<Weekday, Vec<TimeSlot>> = HashMap::new();

        let result =
            are_spot_prices_in_time_slot(local_time_zone, &spot_prices, &local_time_slots)?;

        assert_eq!(result, false);

        Ok(())
    }

    #[test]
    fn are_spot_prices_in_time_slot_returns_false_if_not_all_prices_are_below_configured_price_limit(
    ) -> Result<(), Box<dyn Error>> {
        let local_time_zone = "Europe/Amsterdam".parse::<Tz>()?;

        let spot_prices = vec![
            SpotPrice {
                id: None,
                source: None,
                from: Utc.ymd(2022, 4, 23).and_hms(11, 0, 0),
                till: Utc.ymd(2022, 4, 23).and_hms(12, 0, 0),
                market_price: 0.222,
                market_price_tax: 0.0466956,
                sourcing_markup_price: 0.017,
                energy_tax_price: 0.081,
            },
            SpotPrice {
                id: None,
                source: None,
                from: Utc.ymd(2022, 4, 23).and_hms(12, 0, 0),
                till: Utc.ymd(2022, 4, 23).and_hms(13, 0, 0),
                market_price: 0.217,
                market_price_tax: 0.0456582,
                sourcing_markup_price: 0.017,
                energy_tax_price: 0.081,
            },
        ];

        let local_time_slots = HashMap::from([(
            Weekday::Sat,
            vec![TimeSlot {
                from: NaiveTime::from_hms(7, 0, 0),
                till: NaiveTime::from_hms(19, 0, 0),
                if_price_below: Some(0.1),
            }],
        )]);

        let result =
            are_spot_prices_in_time_slot(local_time_zone, &spot_prices, &local_time_slots)?;

        assert_eq!(result, false);

        Ok(())
    }
}
