use crate::model::{Config, Content, State};
use crate::state_client::StateClient;
use async_trait::async_trait;
use chrono::{prelude::*, Duration, Utc};
use jarvis_lib::model::{
    LoadProfile, LoadProfileSection, PlanningRequest, PlanningResponse, PlanningStrategy,
    SpotPrice, SpotPricePlanner,
};
use jarvis_lib::planner_client::PlannerClient;
use quick_xml::de::from_str;
use rand::Rng;
use regex::Regex;
use serde::Deserialize;
use std::env;
use std::error::Error;
use tracing::{debug, info};
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

        let (best_spot_prices_response, desinfection_desired) = self
            .get_spot_prices_for_tapwater_heating_or_desinfection(
                &config,
                &spot_price_planner,
                &spot_prices,
                now,
                desinfection_finished_at,
            )?;
        let best_spot_prices = best_spot_prices_response.spot_prices;

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

            // add some jitter to start time to prevent all alpha innotec planner controlled heat pumps to start at the exact same time
            let best_spot_prices = Self::add_jitter_to_spot_prices(&config, &best_spot_prices);

            self.set_tap_water_schedule_from_best_spot_prices(
                &mut receiver,
                &mut sender,
                &navigation,
                &config,
                &best_spot_prices,
            )?;

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
                        planned_spot_prices: Some(best_spot_prices),
                    })
                    .await?;
            }
        } else {
            info!("No available best spot prices, not updating heatpump tap water schedule.");
        }

        info!("Blocking worst time for heating for alpha innotec heatpump...");

        let worst_spot_prices_response = self.get_worst_spot_prices_for_blocking_heating(
            &spot_price_planner,
            &spot_prices,
            now,
        )?;
        let worst_spot_prices = worst_spot_prices_response.spot_prices;

        if !worst_spot_prices.is_empty() {
            info!(
                "Found block of {} spot price slots to use for blocking heating:\n{:?}",
                worst_spot_prices.len(),
                worst_spot_prices
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

            // add some jitter to start time to prevent all alpha innotec planner controlled heat pumps to start/stop at the exact same time
            let worst_spot_prices = Self::add_jitter_to_spot_prices(&config, &worst_spot_prices);

            if config.enable_blocking_worst_heating_times {
                self.set_heating_schedule_from_worst_spot_prices(
                    &mut receiver,
                    &mut sender,
                    &navigation,
                    &config,
                    &worst_spot_prices,
                )?;
            }
        } else {
            info!("No available worst spot prices, not updating heatpump heating schedule.");
        }

        Ok(())
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

    fn set_tap_water_schedule_from_best_spot_prices(
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
            let from_time = best_spot_prices
                .first()
                .unwrap()
                .from
                .with_timezone(&heatpump_time_zone);
            let from_hour = from_time.hour();
            let from_minute = from_time.minute();

            // get finish time from last spot price
            let till_time = best_spot_prices
                .last()
                .unwrap()
                .till
                .with_timezone(&heatpump_time_zone);

            let till_hour = till_time.hour();
            let till_minute = till_time.minute();

            if from_hour > till_hour {
                // starts before midnight, finishes after
                let first_item_id = content.item.item.first().unwrap().id.clone();
                info!(
                    "Setting 1) to block {}:{:0>2} - {}:{:0>2}",
                    till_hour, till_minute, from_hour, from_minute
                );
                self.send(
                    sender,
                    websocket::OwnedMessage::Text(format!(
                        "SET;set_{};{}",
                        first_item_id,
                        60 * till_hour + till_minute + 65536 * (60 * from_hour + from_minute)
                    )),
                )?;
            } else {
                // start and finish on same day
                if from_hour > 0 {
                    let first_item_id = content.item.item.first().unwrap().id.clone();
                    info!(
                        "Setting 1) to block 00:00 - {}:{:0>2}",
                        from_hour, from_minute
                    );
                    self.send(
                        sender,
                        websocket::OwnedMessage::Text(format!(
                            "SET;set_{};{}",
                            first_item_id,
                            65536 * (60 * from_hour + from_minute)
                        )),
                    )?;
                }

                if till_hour > 0 {
                    let last_item_id = content.item.item.last().unwrap().id.clone();
                    info!(
                        "Setting 5) to block {}:{:0>2} - 00:00",
                        till_hour, till_minute
                    );
                    self.send(
                        sender,
                        websocket::OwnedMessage::Text(format!(
                            "SET;set_{};{}",
                            last_item_id,
                            60 * till_hour + till_minute
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

    fn set_heating_schedule_from_worst_spot_prices(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        navigation: &Navigation,
        config: &Config,
        worst_spot_prices: &[SpotPrice],
    ) -> Result<(), Box<dyn Error>> {
        info!("Updating heating schedule to block worst spot prices");
        let response_message = self.navigate_to(
            receiver,
            sender,
            navigation,
            "Klokprogramma > Verwarmen > Week",
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

        if !worst_spot_prices.is_empty() && content.item.item.len() > 1 {
            let heatpump_time_zone = config.get_heatpump_time_zone()?;

            // get start time from first spot price
            let from_time = worst_spot_prices
                .first()
                .unwrap()
                .from
                .with_timezone(&heatpump_time_zone);

            let from_hour = from_time.hour();
            let from_minute = from_time.minute();

            // get finish time from last spot price
            let till_time = worst_spot_prices
                .last()
                .unwrap()
                .till
                .with_timezone(&heatpump_time_zone);

            let till_hour = till_time.hour();
            let till_minute = till_time.minute();

            if from_hour > till_hour {
                // starts before midnight, finishes after
                if from_hour > 0 {
                    let first_item_id = content.item.item.first().unwrap().id.clone();
                    info!(
                        "Setting 1) to block {}:{:0>2} - 00:00",
                        from_hour, from_minute
                    );

                    self.send(
                        sender,
                        websocket::OwnedMessage::Text(format!(
                            "SET;set_{};{}",
                            first_item_id,
                            60 * from_hour + from_minute
                        )),
                    )?;
                }

                if till_hour > 0 {
                    let last_item_id = content.item.item.last().unwrap().id.clone();
                    info!(
                        "Setting 5) to block 00:00 - {}:{:0>2}",
                        till_hour, till_minute
                    );
                    self.send(
                        sender,
                        websocket::OwnedMessage::Text(format!(
                            "SET;set_{};{}",
                            last_item_id,
                            65536 * (60 * till_hour + till_minute)
                        )),
                    )?;
                }
            } else {
                // start and finish on same day
                let first_item_id = content.item.item.first().unwrap().id.clone();
                info!(
                    "Setting 1) to block {}:{:0>2} - {}:{:0>2}",
                    from_hour, from_minute, till_hour, till_minute
                );
                self.send(
                    sender,
                    websocket::OwnedMessage::Text(format!(
                        "SET;set_{};{}",
                        first_item_id,
                        60 * from_hour + from_minute + 65536 * (60 * till_hour + till_minute)
                    )),
                )?;
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

    fn add_jitter_to_spot_prices(config: &Config, spot_prices: &[SpotPrice]) -> Vec<SpotPrice> {
        if spot_prices.is_empty() || config.jitter_max_minutes == 0 {
            spot_prices.to_vec()
        } else {
            let mut rng = rand::thread_rng();
            let shift_minutes =
                rng.gen_range(0..2 * config.jitter_max_minutes) - config.jitter_max_minutes;

            let mut updated_spot_prices: Vec<SpotPrice> = vec![];

            for spot_price in spot_prices.iter() {
                updated_spot_prices.push(SpotPrice {
                    from: spot_price.from + Duration::minutes(shift_minutes),
                    till: spot_price.till + Duration::minutes(shift_minutes),
                    ..spot_price.clone()
                })
            }

            updated_spot_prices
        }
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
        sender.send_message(&message)?;

        Ok(())
    }

    fn send_and_await(
        &self,
        receiver: &mut websocket::receiver::Reader<std::net::TcpStream>,
        sender: &mut websocket::sender::Writer<std::net::TcpStream>,
        message: websocket::OwnedMessage,
    ) -> Result<String, Box<dyn Error>> {
        sender.send_message(&message)?;

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

    fn get_spot_prices_for_tapwater_heating_or_desinfection(
        &self,
        config: &Config,
        spot_price_planner: &SpotPricePlanner,
        spot_prices: &[SpotPrice],
        now: DateTime<Utc>,
        desinfection_finished_at: DateTime<Utc>,
    ) -> Result<(PlanningResponse, bool), Box<dyn Error>> {
        let lowest_price_tapwater_heating_response =
            spot_price_planner.get_best_spot_prices(&PlanningRequest {
                spot_prices: spot_prices.to_owned(),
                load_profile: config.load_profile.clone(),
                planning_strategy: PlanningStrategy::LowestPrice,
                after: Some(now),
                before: Some(now + Duration::hours(12)),
            })?;

        let lowest_price_desinfection_response =
            spot_price_planner.get_best_spot_prices(&PlanningRequest {
                spot_prices: spot_prices.to_owned(),
                load_profile: config.desinfection_load_profile.clone(),
                planning_strategy: PlanningStrategy::LowestPrice,
                after: Some(now),
                before: Some(now + Duration::hours(24)),
            })?;

        // if lowest price desinfection spot prices start after next 12 hours we don't want desinfection to run now
        if lowest_price_desinfection_response.spot_prices.is_empty()
            || lowest_price_desinfection_response
                .spot_prices
                .first()
                .unwrap()
                .from
                > now + Duration::hours(12)
        {
            info!("Optimal spot prices for desinfection are more than 12 hours ahead, skip using those for now and will check again next run");
            Ok((lowest_price_tapwater_heating_response, false))
        } else {
            let highest_price_desinfection_response =
                spot_price_planner.get_best_spot_prices(&PlanningRequest {
                    spot_prices: spot_prices.to_owned(),
                    load_profile: config.desinfection_load_profile.clone(),
                    planning_strategy: PlanningStrategy::HighestPrice,
                    after: Some(now),
                    before: Some(now + Duration::hours(24)),
                })?;

            info!("Checking if desinfection is needed");
            let desinfection_desired = is_desinfection_desired(
                config.min_hours_since_last_desinfection,
                config.max_hours_since_last_desinfection,
                &desinfection_finished_at,
                &lowest_price_desinfection_response,
                &highest_price_desinfection_response,
            )?;

            if desinfection_desired {
                Ok((lowest_price_desinfection_response, desinfection_desired))
            } else {
                Ok((lowest_price_tapwater_heating_response, false))
            }
        }
    }

    fn get_worst_spot_prices_for_blocking_heating(
        &self,
        spot_price_planner: &SpotPricePlanner,
        spot_prices: &[SpotPrice],
        now: DateTime<Utc>,
    ) -> Result<PlanningResponse, Box<dyn Error>> {
        let highest_price_desinfection_response =
            spot_price_planner.get_best_spot_prices(&PlanningRequest {
                spot_prices: spot_prices.to_owned(),
                load_profile: LoadProfile {
                    sections: vec![LoadProfileSection {
                        duration_seconds: 3600,
                        power_draw_watt: 1000.0,
                    }],
                },
                planning_strategy: PlanningStrategy::HighestPrice,
                after: Some(now),
                before: Some(now + Duration::hours(24)),
            })?;

        Ok(highest_price_desinfection_response)
    }
}

fn is_desinfection_desired(
    min_hours_since_last_desinfection: i64,
    max_hours_since_last_desinfection: i64,
    desinfection_finished_at: &DateTime<Utc>,
    lowest_price_desinfection_response: &PlanningResponse,
    highest_price_desinfection_response: &PlanningResponse,
) -> Result<bool, Box<dyn Error>> {
    if max_hours_since_last_desinfection <= min_hours_since_last_desinfection {
        return Err(Box::<dyn Error>::from(format!("max_hours_since_last_desinfection ({}) is less or equal to min_hours_since_last_desinfection ({}) which is not allowed", max_hours_since_last_desinfection, min_hours_since_last_desinfection)));
    }

    // make likelihood of desinfection when max of best spot prices < fraction of max of all spot prices
    // where fraction is an exponential curve between 5 and 10 days after previous round
    if lowest_price_desinfection_response.spot_prices.is_empty() {
        info!("No best spot prices, desinfection is not desired");
        Ok(false)
    } else {
        let planned_finished_at = lowest_price_desinfection_response
            .spot_prices
            .last()
            .unwrap()
            .till;
        let hours_since_last_desinfection =
            (planned_finished_at - *desinfection_finished_at).num_hours();

        let lowest_total_price = lowest_price_desinfection_response.total_price(Some(|sp| {
            sp.market_price + sp.market_price_tax + sp.sourcing_markup_price + sp.energy_tax_price
        }));

        if lowest_total_price <= 0.0 {
            info!(
                "Lowest prices is less then equal to zero ({}), desinfection IS desired",
                lowest_total_price
            );
            Ok(true)
        } else if hours_since_last_desinfection < min_hours_since_last_desinfection {
            info!("Hours since last desinfection less than min configured hours ({} < {}), desinfection is not desired", hours_since_last_desinfection, min_hours_since_last_desinfection);
            Ok(false)
        } else if hours_since_last_desinfection > max_hours_since_last_desinfection {
            info!("Hours since last desinfection greater than min configured hours ({} > {}), desinfection IS desired", hours_since_last_desinfection, max_hours_since_last_desinfection);
            Ok(true)
        } else {
            let hours_since_min = hours_since_last_desinfection - min_hours_since_last_desinfection;
            let hours_between_min_and_max: i64 =
                max_hours_since_last_desinfection - min_hours_since_last_desinfection;

            assert!(max_hours_since_last_desinfection > min_hours_since_last_desinfection);

            let fraction_of_max_price = (hours_since_min * hours_since_min) as f64
                / (hours_between_min_and_max * hours_between_min_and_max) as f64;
            debug!(
                "fraction_of_max_price = {}^2 / {}^2 = {}",
                hours_since_min, hours_between_min_and_max, fraction_of_max_price
            );

            let highest_total_price =
                highest_price_desinfection_response.total_price(Some(|sp| sp.market_price));
            let lowest_total_price =
                lowest_price_desinfection_response.total_price(Some(|sp| sp.market_price));

            let is_desired = lowest_total_price < (fraction_of_max_price * highest_total_price);
            info!(
                "is_desired = {} < ({} * {}) = {}",
                lowest_total_price, fraction_of_max_price, highest_total_price, is_desired
            );

            Ok(is_desired)
        }
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
    use jarvis_lib::model::{LoadProfile, LoadProfileSection, SpotPrice};

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

        client.set_tap_water_schedule_from_best_spot_prices(
            &mut receiver,
            &mut sender,
            &navigation,
            &Config {
                local_time_zone: "Europe/Amsterdam".to_string(),
                heatpump_time_zone: "Europe/Amsterdam".to_string(),
                desired_tap_water_temperature: 50.0,
                min_hours_since_last_desinfection: 96,
                max_hours_since_last_desinfection: 240,
                load_profile: LoadProfile {
                    sections: vec![LoadProfileSection {
                        duration_seconds: 7200,
                        power_draw_watt: 2000.0,
                    }],
                },
                desinfection_load_profile: LoadProfile {
                    sections: vec![
                        LoadProfileSection {
                            duration_seconds: 7200,
                            power_draw_watt: 2000.0,
                        },
                        LoadProfileSection {
                            duration_seconds: 1800,
                            power_draw_watt: 8000.0,
                        },
                    ],
                },
                jitter_max_minutes: 15,
                enable_blocking_worst_heating_times: true,
            },
            &vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 4, 21, 13, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 4, 21, 14, 0, 0).unwrap(),
                    market_price: 0.157,
                    market_price_tax: 0.0330708,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.081,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 4, 21, 14, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 4, 21, 15, 0, 0).unwrap(),
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
    fn is_desinfection_desired_returns_false_when_best_spot_prices_is_empty(
    ) -> Result<(), Box<dyn Error>> {
        let desinfection_load_profile = LoadProfile {
            sections: vec![
                LoadProfileSection {
                    duration_seconds: 5400,
                    power_draw_watt: 2000.0,
                },
                LoadProfileSection {
                    duration_seconds: 1800,
                    power_draw_watt: 8000.0,
                },
            ],
        };
        let min_hours_since_last_desinfection: i64 = 96;
        let max_hours_since_last_desinfection: i64 = 240;
        let desinfection_finished_at = Utc.with_ymd_and_hms(2022, 4, 30, 9, 37, 0).unwrap();
        let highest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 13, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    market_price: 0.33,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    market_price: 0.08,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 16, 0, 0).unwrap(),
                    market_price: 0.09,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile.clone(),
        };
        let lowest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![],
            load_profile: desinfection_load_profile,
        };

        let desinfection_desired = is_desinfection_desired(
            min_hours_since_last_desinfection,
            max_hours_since_last_desinfection,
            &desinfection_finished_at,
            &lowest_price_desinfection_response,
            &highest_price_desinfection_response,
        )?;

        assert_eq!(desinfection_desired, false);

        Ok(())
    }

    #[test]
    fn is_desinfection_desired_returns_false_when_hours_since_last_desinfection_are_less_than_min_hours(
    ) -> Result<(), Box<dyn Error>> {
        let desinfection_load_profile = LoadProfile {
            sections: vec![
                LoadProfileSection {
                    duration_seconds: 5400,
                    power_draw_watt: 2000.0,
                },
                LoadProfileSection {
                    duration_seconds: 1800,
                    power_draw_watt: 8000.0,
                },
            ],
        };

        let min_hours_since_last_desinfection: i64 = 96;
        let max_hours_since_last_desinfection: i64 = 240;
        let desinfection_finished_at = Utc.with_ymd_and_hms(2022, 5, 11, 9, 37, 0).unwrap();
        let highest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 13, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    market_price: 0.33,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    market_price: 0.08,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 16, 0, 0).unwrap(),
                    market_price: 0.09,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile.clone(),
        };
        let lowest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    market_price: 0.08,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 16, 0, 0).unwrap(),
                    market_price: 0.09,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile,
        };

        let desinfection_desired = is_desinfection_desired(
            min_hours_since_last_desinfection,
            max_hours_since_last_desinfection,
            &desinfection_finished_at,
            &lowest_price_desinfection_response,
            &highest_price_desinfection_response,
        )?;

        assert_eq!(desinfection_desired, false);

        Ok(())
    }

    #[test]
    fn is_desinfection_desired_returns_true_when_hours_since_last_desinfection_are_less_than_min_hours_but_total_price_for_desinfection_is_negative(
    ) -> Result<(), Box<dyn Error>> {
        let desinfection_load_profile = LoadProfile {
            sections: vec![
                LoadProfileSection {
                    duration_seconds: 5400,
                    power_draw_watt: 2000.0,
                },
                LoadProfileSection {
                    duration_seconds: 1800,
                    power_draw_watt: 8000.0,
                },
            ],
        };

        let min_hours_since_last_desinfection: i64 = 96;
        let max_hours_since_last_desinfection: i64 = 240;
        let desinfection_finished_at = Utc.with_ymd_and_hms(2022, 5, 11, 9, 37, 0).unwrap();
        let highest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 13, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    market_price: 0.33,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    market_price: 0.08,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 16, 0, 0).unwrap(),
                    market_price: 0.09,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile.clone(),
        };
        let lowest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    market_price: -0.40,
                    market_price_tax: -0.10,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 16, 0, 0).unwrap(),
                    market_price: -0.40,
                    market_price_tax: -0.10,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile,
        };

        let desinfection_desired = is_desinfection_desired(
            min_hours_since_last_desinfection,
            max_hours_since_last_desinfection,
            &desinfection_finished_at,
            &lowest_price_desinfection_response,
            &highest_price_desinfection_response,
        )?;

        assert_eq!(desinfection_desired, true);

        Ok(())
    }

    #[test]
    fn is_desinfection_desired_returns_true_when_hours_since_last_desinfection_are_greater_than_max_hours(
    ) -> Result<(), Box<dyn Error>> {
        let desinfection_load_profile = LoadProfile {
            sections: vec![
                LoadProfileSection {
                    duration_seconds: 5400,
                    power_draw_watt: 2000.0,
                },
                LoadProfileSection {
                    duration_seconds: 1800,
                    power_draw_watt: 8000.0,
                },
            ],
        };

        let min_hours_since_last_desinfection: i64 = 96;
        let max_hours_since_last_desinfection: i64 = 240;
        let desinfection_finished_at = Utc.with_ymd_and_hms(2022, 4, 30, 9, 37, 0).unwrap();
        let highest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 13, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    market_price: 0.33,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    market_price: 0.08,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 16, 0, 0).unwrap(),
                    market_price: 0.09,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile.clone(),
        };
        let lowest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    market_price: 0.08,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 16, 0, 0).unwrap(),
                    market_price: 0.09,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile,
        };

        let desinfection_desired = is_desinfection_desired(
            min_hours_since_last_desinfection,
            max_hours_since_last_desinfection,
            &desinfection_finished_at,
            &lowest_price_desinfection_response,
            &highest_price_desinfection_response,
        )?;

        assert_eq!(desinfection_desired, true);

        Ok(())
    }

    #[test]
    fn is_desinfection_desired_returns_true_when_hours_since_last_desinfection_between_min_and_max_hours_and_max_prices_of_best_spot_prices_is_less_than_calculated_percentage_of_max_of_all_spot_prices(
    ) -> Result<(), Box<dyn Error>> {
        let desinfection_load_profile = LoadProfile {
            sections: vec![
                LoadProfileSection {
                    duration_seconds: 5400,
                    power_draw_watt: 2000.0,
                },
                LoadProfileSection {
                    duration_seconds: 1800,
                    power_draw_watt: 8000.0,
                },
            ],
        };

        let min_hours_since_last_desinfection: i64 = 96;
        let max_hours_since_last_desinfection: i64 = 240;
        let desinfection_finished_at = Utc.with_ymd_and_hms(2022, 5, 5, 9, 37, 0).unwrap();
        let highest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 18, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 19, 0, 0).unwrap(),
                    market_price: 0.33,
                    market_price_tax: 0.06,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 19, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 20, 0, 0).unwrap(),
                    market_price: 0.28,
                    market_price_tax: 0.05,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 20, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 21, 0, 0).unwrap(),
                    market_price: 0.25,
                    market_price_tax: 0.04,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile.clone(),
        };
        let lowest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 13, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    market_price: 0.06,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    market_price: 0.08,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 16, 0, 0).unwrap(),
                    market_price: 0.09,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile,
        };

        let desinfection_desired = is_desinfection_desired(
            min_hours_since_last_desinfection,
            max_hours_since_last_desinfection,
            &desinfection_finished_at,
            &lowest_price_desinfection_response,
            &highest_price_desinfection_response,
        )?;

        assert_eq!(desinfection_desired, true);

        Ok(())
    }

    #[test]
    fn is_desinfection_desired_returns_false_when_hours_since_last_desinfection_between_min_and_max_hours_and_max_prices_of_best_spot_prices_is_greater_than_or_equal_to_calculated_percentage_of_max_of_all_spot_prices(
    ) -> Result<(), Box<dyn Error>> {
        let desinfection_load_profile = LoadProfile {
            sections: vec![
                LoadProfileSection {
                    duration_seconds: 5400,
                    power_draw_watt: 2000.0,
                },
                LoadProfileSection {
                    duration_seconds: 1800,
                    power_draw_watt: 8000.0,
                },
            ],
        };

        let min_hours_since_last_desinfection: i64 = 96;
        let max_hours_since_last_desinfection: i64 = 240;
        let desinfection_finished_at = Utc.with_ymd_and_hms(2022, 5, 6, 9, 37, 0).unwrap();
        let highest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 18, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 19, 0, 0).unwrap(),
                    market_price: 0.33,
                    market_price_tax: 0.06,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 19, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 20, 0, 0).unwrap(),
                    market_price: 0.08,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 20, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 21, 0, 0).unwrap(),
                    market_price: 0.09,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile.clone(),
        };
        let lowest_price_desinfection_response = PlanningResponse {
            spot_prices: vec![
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 13, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    market_price: 0.08,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 14, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    market_price: 0.08,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
                SpotPrice {
                    id: None,
                    source: None,
                    from: Utc.with_ymd_and_hms(2022, 5, 12, 15, 0, 0).unwrap(),
                    till: Utc.with_ymd_and_hms(2022, 5, 12, 16, 0, 0).unwrap(),
                    market_price: 0.09,
                    market_price_tax: 0.02,
                    sourcing_markup_price: 0.017,
                    energy_tax_price: 0.08,
                },
            ],
            load_profile: desinfection_load_profile,
        };

        let desinfection_desired = is_desinfection_desired(
            min_hours_since_last_desinfection,
            max_hours_since_last_desinfection,
            &desinfection_finished_at,
            &lowest_price_desinfection_response,
            &highest_price_desinfection_response,
        )?;

        assert_eq!(desinfection_desired, false);

        Ok(())
    }
}
