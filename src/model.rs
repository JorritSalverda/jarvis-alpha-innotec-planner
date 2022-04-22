use chrono::prelude::*;
use jarvis_lib::config_client::SetDefaults;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub local_time_zone: String,
    pub heatpump_time_zone: String,
    pub desinfection_day_of_week: Weekday,
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
        assert_eq!(config.desinfection_day_of_week, Weekday::Sun);
    }
}
