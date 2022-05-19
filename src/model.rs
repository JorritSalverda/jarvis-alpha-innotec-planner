use chrono::prelude::*;
use chrono_tz::Tz;
use jarvis_lib::config_client::SetDefaults;
use jarvis_lib::model::LoadProfile;
use jarvis_lib::model::SpotPrice;
use serde::{Deserialize, Serialize};
use std::error::Error;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub load_profile: LoadProfile,
    pub desinfection_load_profile: LoadProfile,
    pub local_time_zone: String,
    pub heatpump_time_zone: String,
    pub desired_tap_water_temperature: f64,
    pub min_hours_since_last_desinfection: i64,
    pub max_hours_since_last_desinfection: i64,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TimeSlot {
    pub from: NaiveTime,
    pub till: NaiveTime,
    pub if_price_below: Option<f64>,
}

impl Config {
    pub fn get_heatpump_time_zone(&self) -> Result<Tz, Box<dyn Error>> {
        Ok(self.heatpump_time_zone.parse::<Tz>()?)
    }
}

impl SetDefaults for Config {
    fn set_defaults(&mut self) {}
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename = "Content")]
pub struct Content {
    pub item: ContentItem,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename = "item")]
pub struct ContentItem {
    pub name: String,
    #[serde(rename = "item", default)]
    pub item: Vec<Item>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename = "item")]
pub struct Item {
    pub id: String,
    pub value: String,
    pub name: String,
    pub r#type: String,
    pub raw: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct ItemValue {
    #[serde(rename = "$value")]
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub desinfection_enabled: bool,
    pub desinfection_finished_at: Option<DateTime<Utc>>,
    pub planned_spot_prices: Option<Vec<SpotPrice>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_lib::config_client::{ConfigClient, ConfigClientConfig};
    use quick_xml::de::from_str;

    #[test]
    fn deserialize_content() {
        // GET;0xa599f0
        let response = r#"<Content>
        <item>
          <name>Maandag - Zondag</name>
          <item id='0xa57344'>
            <value>10:00 - 00:00</value>
            <name>1)</name>
            <type>timer</type>
            <raw>600</raw>
          </item>
          <item id='0xa53c8c'>
            <value>00:00 - 03:00</value>
            <name>2)</name>
            <type>timer</type>
            <raw>11796480</raw>
          </item>
          <item id='0xa47ee4'>
            <value>00:00 - 00:00</value>
            <name>3)</name>
            <type>timer</type>
            <raw>0</raw>
          </item>
          <item id='0xa6630c'>
            <value>00:00 - 00:00</value>
            <name>4)</name>
            <type>timer</type>
            <raw>0</raw>
          </item>
          <item id='0xa68d74'>
            <value>00:00 - 00:00</value>
            <name>5)</name>
            <type>timer</type>
            <raw>0</raw>
          </item>
        </item>
      </Content>"#;

        // act
        let content: Content = from_str(response).unwrap();

        assert_eq!(content.item.name, "Maandag - Zondag");
        assert_eq!(content.item.item.len(), 5);
        assert_eq!(content.item.item[0].id, "0xa57344".to_string());
        assert_eq!(content.item.item[0].value, "10:00 - 00:00".to_string());
        assert_eq!(content.item.item[0].name, "1)".to_string());
        assert_eq!(content.item.item[0].r#type, "timer".to_string());
        assert_eq!(content.item.item[0].raw, "600".to_string());
    }

    #[test]
    fn read_config_from_file_returns_deserialized_test_file() {
        let config_client =
            ConfigClient::new(ConfigClientConfig::new("test-config.yaml".to_string()).unwrap());

        let config: Config = config_client.read_config_from_file().unwrap();

        assert_eq!(config.local_time_zone, "Europe/Amsterdam".to_string());
        assert_eq!(config.heatpump_time_zone, "Europe/Amsterdam".to_string());
        assert_eq!(config.desired_tap_water_temperature, 50.0);
        assert_eq!(config.min_hours_since_last_desinfection, 96);
        assert_eq!(config.max_hours_since_last_desinfection, 240);
    }
}
