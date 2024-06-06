// Copyright (c) 2024 Jan Holthuis <jan.holthuis@rub.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy
// of the MPL was not distributed with this file, You can obtain one at
// http://mozilla.org/MPL/2.0/.
//
// SPDX-License-Identifier: MPL-2.0

use btleplug::api::{BDAddr, Central, CentralEvent, Manager as _, Peripheral, ScanFilter};
use btleplug::platform::{Adapter, Manager, PeripheralId};
use clap::Parser;
use core::time::Duration;
use futures::StreamExt;
use hass_mqtt_discovery::device::Device as HassDevice;
use hass_mqtt_discovery::device_class::DeviceClass as HassDeviceClass;
use hass_mqtt_discovery::entity::BinarySensor as HassBinarySensor;
use hass_mqtt_discovery::entity::Sensor as HassSensor;
use log::{error, info, warn};
use rumqttc::{AsyncClient, ConnectReturnCode, Incoming, MqttOptions, QoS};
use serde::{Serialize, Serializer};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use tokio::sync::mpsc;
use xiaomi_ble::device::DeviceType;
use xiaomi_ble::parse_service_advertisement;
use xiaomi_ble::sensor::{BinaryMeasurementType, NumericMeasurementType, SensorEvent};

/// Xiaomi BLE MQTT Gateway.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Hostname of the MQTT server.
    #[arg(short = 'H', long, default_value = "localhost")]
    host: String,
    /// Port of the MQTT server.
    #[arg(short = 'P', long, default_value_t = 1883)]
    port: u16,
    /// Username for the MQTT server.
    #[arg(short, long, requires = "password")]
    username: Option<String>,
    /// Password for the MQTT server.
    #[arg(short, long, requires = "username")]
    password: Option<String>,
    /// Bluetooth Adapter Name.
    #[arg(short = 'a', long)]
    bt_adapter: Option<String>,
}

#[derive(Debug)]
struct Message {
    mac_address: BDAddr,
    device_type: Option<&'static DeviceType>,
    event: SensorEvent,
}

enum HassSensorInfo<'a> {
    BinarySensor(HassBinarySensor<'a>),
    Sensor(HassSensor<'a>),
}

impl Serialize for HassSensorInfo<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self {
            Self::BinarySensor(sensor) => sensor.serialize(serializer),
            Self::Sensor(sensor) => sensor.serialize(serializer),
        }
    }
}

async fn get_mac_address(adapter: &Adapter, id: &PeripheralId) -> Result<BDAddr, btleplug::Error> {
    let peripheral = adapter.peripheral(id).await?;
    let mac_address = peripheral.address();
    Ok(mac_address)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env::set_var("XIAOMI_BLE_MQTT_GATEWAY_LOG_LEVEL", "info");
    pretty_env_logger::init_custom_env("XIAOMI_BLE_MQTT_GATEWAY_LOG_LEVEL");

    let args = Cli::parse();
    let mqtt_host = args.host;
    let mqtt_port = args.port;
    let mqtt_credentials = args.username.and_then(|username| {
        args.password
            .and_then(|password| Some((username, password)))
    });

    let manager = Manager::new().await?;

    // Get the first bluetooth adapter and connect to it.
    let mut adapters = manager.adapters().await?.into_iter();
    let central = if let Some(bt_adapter_name) = args.bt_adapter {
        loop {
            if let Some(adapter) = adapters.next() {
                if adapter
                    .adapter_info()
                    .await
                    .is_ok_and(|info| info.contains(&bt_adapter_name))
                {
                    break Some(adapter);
                }
            } else {
                break None;
            }
        }
    } else {
        adapters.next()
    }
    .expect("Bluetooth adapter not found!");

    if let Ok(adapter_info) = central.adapter_info().await {
        info!("Adapter: {adapter_info}");
    }

    // Each adapter has an event stream, we fetch via events(),
    // simplifying the type, this will return what is essentially a
    // Future<Result<Stream<Item=CentralEvent>>>.
    let mut ble_events = central.events().await?;

    // Start scanning for devices.
    central.start_scan(ScanFilter::default()).await?;

    let (tx, mut rx) = mpsc::channel(10);

    tokio::spawn(async move {
        // When getting a ServiceDataAdvertisement, print the senders's MAC address, name and the
        // contained sensor values.
        while let Some(ble_event) = ble_events.next().await {
            if let CentralEvent::ServiceDataAdvertisement { id, service_data } = &ble_event {
                let mac_address = match get_mac_address(&central, id).await {
                    Ok(value) => value,
                    Err(_) => continue,
                };

                for service_advertisement in service_data
                    .iter()
                    .filter_map(|(uuid, data)| parse_service_advertisement(uuid, data).ok())
                {
                    let device_type = service_advertisement.device_type();

                    for event in service_advertisement.iter_sensor_events() {
                        let mac_address = mac_address.clone();
                        let message = Message {
                            mac_address,
                            device_type,
                            event,
                        };
                        if let Err(err) = tx.send(message).await {
                            warn!("Failed to queue message: {}", err);
                        }
                    }
                }
            }
        }
    });

    let mut mqtt_options = MqttOptions::new("xiaomi-ble-mqtt-gateway", mqtt_host, mqtt_port);
    mqtt_options.set_keep_alive(Duration::from_secs(5));
    if let Some((mqtt_username, mqtt_password)) = mqtt_credentials {
        mqtt_options.set_credentials(mqtt_username, mqtt_password);
    }

    let (mqtt_client, mut mqtt_eventloop) = AsyncClient::new(mqtt_options, 10);

    tokio::spawn(async move {
        loop {
            match mqtt_eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(Incoming::ConnAck(ack))) => {
                    if ack.code == ConnectReturnCode::Success {
                        info!("Successfully connected to mqtt broker");
                    } else {
                        warn!("Could not connect to mqtt broker: {:?}", ack.code);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
                // On disconnection, wait before reconnection
                Ok(rumqttc::Event::Incoming(Incoming::Disconnect)) => {
                    warn!("Disconnected from mqtt broker");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Ok(_) => {}
                // On error, wait before reconnection
                Err(e) => {
                    error!("Error while polling mqtt eventloop: {}", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });

    let mut known_devices = HashMap::new();
    let mut known_sensors = HashMap::new();
    while let Some(message) = rx.recv().await {
        let mac_addr = message.mac_address.to_string_no_delim();
        let device_unique_id = format!("xiaomi_ble_{mac_addr}");
        if !known_devices.contains_key(&device_unique_id) {
            let hass_device = HassDevice {
                connections: vec![].into(),
                identifiers: vec![device_unique_id.clone().into()].into(),
                manufacturer: Some("Xiaomi".into()),
                model: message
                    .device_type
                    .map(|device_type| device_type.model.into()),
                name: message
                    .device_type
                    .map(|device_type| device_type.name.into()),
                suggested_area: None,
                sw_version: None,
                hw_version: None,
                via_device: None,
                configuration_url: None,
            };
            info!("New device: {device_unique_id}");
            known_devices.insert(device_unique_id.clone(), hass_device);
        }
        let (sensor_id, sensor_state) = match message.event {
            SensorEvent::BinaryMeasurement {
                measurement_type,
                value,
            } => (
                measurement_type.as_str(),
                if value { "on" } else { "off" }.to_string(),
            ),
            SensorEvent::NumericMeasurement {
                measurement_type,
                value,
                unit: _,
            } => (measurement_type.as_str(), value.to_string()),
        };

        let sensor_unique_id = format!("xiaomi_ble_{mac_addr}_{sensor_id}");
        let sensor_state_topic = format!("xiaomi_ble_{mac_addr}/{sensor_id}");
        if !known_sensors.contains_key(&sensor_unique_id) {
            let expire_after = nonzero_lit::u32!(60 * 60); // 60 minutes
            let sensor_state_topic = sensor_state_topic.clone();
            let (hass_discovery_topic, hass_sensor) = match message.event {
                SensorEvent::BinaryMeasurement {
                    measurement_type,
                    value: _,
                } => {
                    let (name, icon) = match measurement_type {
                        BinaryMeasurementType::Power => ("Power", "mdi:power"),
                        BinaryMeasurementType::Sleep => ("Sleep", "mdi:sleep"),
                        BinaryMeasurementType::Binding => ("Binding", "mdi:handshake"),
                        BinaryMeasurementType::Switch => ("Switch", "mdi:light-switch"),
                        BinaryMeasurementType::WaterImmersion => {
                            ("Water Immersion", "mdi:water-alert")
                        }
                        BinaryMeasurementType::GasLeak => ("Gas Leak", "mdi:cloud-alert"),
                        BinaryMeasurementType::Light => ("Light", "mdi:lightbulb-on"),
                    };

                    let sensor = HassBinarySensor::new(sensor_state_topic)
                        .name(name)
                        .icon(icon)
                        .unique_id(sensor_unique_id.clone())
                        .object_id(sensor_unique_id.clone())
                        .expire_after(expire_after)
                        .device(
                            known_devices
                                .get(&device_unique_id)
                                .map(HassDevice::clone)
                                .unwrap(),
                        );
                    (
                        format!("homeassistant/binary_sensor/{sensor_unique_id}/config"),
                        HassSensorInfo::BinarySensor(sensor),
                    )
                }
                SensorEvent::NumericMeasurement {
                    measurement_type,
                    value: _,
                    unit,
                } => {
                    let (name, icon, device_class) = match measurement_type {
                        NumericMeasurementType::Temperature => (
                            "Temperature",
                            "mdi:thermometer",
                            HassDeviceClass::Temperature,
                        ),
                        NumericMeasurementType::Humidity => {
                            ("Humidity", "mdi:cloud-percent", HassDeviceClass::Humidity)
                        }
                        NumericMeasurementType::Illuminance => (
                            "Illuminance",
                            "mdi:brightness-percent",
                            HassDeviceClass::Illuminance,
                        ),
                        NumericMeasurementType::Moisture => {
                            ("Moisture", "mdi:water-percent", HassDeviceClass::None)
                        }
                        NumericMeasurementType::Conductivity => {
                            ("Conductivity", "mdi:nutrition", HassDeviceClass::None)
                        }
                        NumericMeasurementType::FormaldehydeConcentration => (
                            "Formaldehyde Concentration",
                            "mdi:air-purifier",
                            HassDeviceClass::None,
                        ),
                        NumericMeasurementType::RemainingSupplies => {
                            ("Remaining Supplies", "mdi:percent", HassDeviceClass::None)
                        }
                        NumericMeasurementType::BatteryPower => {
                            ("Battery Power", "mdi:battery", HassDeviceClass::None)
                        }
                        NumericMeasurementType::Weight => {
                            ("Weight", "mdi:scale", HassDeviceClass::None)
                        }
                        NumericMeasurementType::Impedance => {
                            ("Impedance", "mdi:omega", HassDeviceClass::None)
                        }
                    };
                    let sensor = HassSensor::new(sensor_state_topic)
                        .name(name)
                        .icon(icon)
                        .device_class(device_class)
                        .unit_of_measurement(unit.as_str())
                        .unique_id(sensor_unique_id.clone())
                        .object_id(sensor_unique_id.clone())
                        .expire_after(expire_after)
                        .device(
                            known_devices
                                .get(&device_unique_id)
                                .map(HassDevice::clone)
                                .unwrap(),
                        );
                    (
                        format!("homeassistant/sensor/{sensor_unique_id}/config"),
                        HassSensorInfo::Sensor(sensor),
                    )
                }
            };

            info!("New sensor: {sensor_unique_id}");
            mqtt_client
                .publish(
                    hass_discovery_topic,
                    QoS::AtLeastOnce,
                    false,
                    serde_json::to_string(&hass_sensor)?,
                )
                .await?;
            known_sensors.insert(sensor_unique_id, hass_sensor);
        }

        info!("Publishing {sensor_state_topic}: {sensor_state}");
        mqtt_client
            .publish(sensor_state_topic, QoS::AtLeastOnce, false, sensor_state)
            .await?;
    }

    Ok(())
}
