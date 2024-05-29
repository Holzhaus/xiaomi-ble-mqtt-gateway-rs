// Copyright (c) 2024 Jan Holthuis <jan.holthuis@rub.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy
// of the MPL was not distributed with this file, You can obtain one at
// http://mozilla.org/MPL/2.0/.
//
// SPDX-License-Identifier: MPL-2.0

use btleplug::api::{BDAddr, Central, CentralEvent, Manager as _, Peripheral, ScanFilter};
use btleplug::platform::{Adapter, Manager, PeripheralId};
use core::time::Duration;
use futures::stream::StreamExt;
use hass_mqtt_discovery::device::Device as HassDevice;
use hass_mqtt_discovery::device_class::DeviceClass as HassDeviceClass;
use hass_mqtt_discovery::entity::BinarySensor as HassBinarySensor;
use hass_mqtt_discovery::entity::Sensor as HassSensor;
use log::{debug, error, info, warn};
use rumqttc::{AsyncClient, ConnectReturnCode, Incoming, MqttOptions, QoS};
use serde::{Serialize, Serializer};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use tokio::sync::mpsc;
use xiaomi_ble::device::DeviceType;
use xiaomi_ble::parse_service_advertisement;
use xiaomi_ble::sensor::SensorValue;

#[derive(Debug)]
struct Message {
    mac_address: BDAddr,
    device_type: Option<&'static DeviceType>,
    event: SensorValue,
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
    pretty_env_logger::init();

    let manager = Manager::new().await?;

    // Get the first bluetooth adapter and connect to it.
    let adapters = manager.adapters().await?;
    let central = adapters.into_iter().next().unwrap();

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

                    for event in service_advertisement.iter_sensor_values() {
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

    let mut mqtt_options = MqttOptions::new("xiaomi-ble-mqtt-gateway", "localhost", 1883);
    mqtt_options.set_keep_alive(Duration::from_secs(5));

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
            SensorValue::Power(value) => ("power", if value { "on" } else { "off" }.to_string()),
            SensorValue::Temperature(value) => ("temperature", format!("{}", value)),
            SensorValue::Humidity(value) => ("humidity", format!("{}", value)),
            SensorValue::Illuminance(value) => ("illuminance", format!("{}", value)),
            SensorValue::Moisture(value) => ("moisture", format!("{}", value)),
            SensorValue::Conductivity(value) => ("conductivity", format!("{}", value)),
            SensorValue::FormaldehydeConcentration(value) => {
                ("formaldehyde_concentration", format!("{}", value))
            }
            SensorValue::Consumable(value) => ("consumable", format!("{}", value)),
            SensorValue::MoistureDetected(value) => (
                "moisture_detected",
                if value { "on" } else { "off" }.to_string(),
            ),
            SensorValue::SmokeDetected(value) => (
                "smoke_detected",
                if value { "on" } else { "off" }.to_string(),
            ),
            SensorValue::TimeWithoutMotion(value) => ("time_without_motion", format!("{}", value)),
        };

        let sensor_unique_id = format!("xiaomi_ble_{mac_addr}_{sensor_id}");
        let sensor_state_topic = format!("xiaomi_ble_{mac_addr}/{sensor_id}");
        if !known_sensors.contains_key(&sensor_unique_id) {
            let sensor_state_topic = sensor_state_topic.clone();
            let hass_sensor = match message.event {
                SensorValue::Power(_) => HassSensorInfo::BinarySensor(
                    HassBinarySensor::new(sensor_state_topic)
                        .name("Power")
                        .icon("power")
                        .device_class(HassDeviceClass::None),
                ),
                SensorValue::Temperature(_) => HassSensorInfo::Sensor(
                    HassSensor::new(sensor_state_topic)
                        .name("Temperature")
                        .icon("thermometer")
                        .device_class(HassDeviceClass::Temperature),
                ),
                SensorValue::Humidity(_) => HassSensorInfo::Sensor(
                    HassSensor::new(sensor_state_topic)
                        .name("Humidity")
                        .icon("cloud-percent")
                        .device_class(HassDeviceClass::Humidity),
                ),
                SensorValue::Illuminance(_) => HassSensorInfo::Sensor(
                    HassSensor::new(sensor_state_topic)
                        .name("Illuminance")
                        .icon("brightness-percent")
                        .device_class(HassDeviceClass::Illuminance),
                ),
                SensorValue::Moisture(_) => HassSensorInfo::Sensor(
                    HassSensor::new(sensor_state_topic)
                        .name("Moisture")
                        .icon("water-percent")
                        .device_class(HassDeviceClass::None),
                ),
                SensorValue::Conductivity(_) => HassSensorInfo::Sensor(
                    HassSensor::new(sensor_state_topic)
                        .name("Conductivity")
                        .icon("nutrition")
                        .device_class(HassDeviceClass::None),
                ),
                SensorValue::FormaldehydeConcentration(_) => HassSensorInfo::Sensor(
                    HassSensor::new(sensor_state_topic)
                        .name("Formaldehyde Concentration")
                        .icon("air-purifier")
                        .device_class(HassDeviceClass::None),
                ),
                SensorValue::Consumable(_) => HassSensorInfo::Sensor(
                    HassSensor::new(sensor_state_topic)
                        .name("Consumable")
                        .icon("percent")
                        .device_class(HassDeviceClass::None),
                ),
                SensorValue::MoistureDetected(_) => HassSensorInfo::BinarySensor(
                    HassBinarySensor::new(sensor_state_topic)
                        .name("Moisture detected")
                        .icon("water-alert")
                        .device_class(HassDeviceClass::None),
                ),
                SensorValue::SmokeDetected(_) => HassSensorInfo::BinarySensor(
                    HassBinarySensor::new(sensor_state_topic)
                        .name("Smoke detected")
                        .icon("smoke-detector-alert")
                        .device_class(HassDeviceClass::None),
                ),
                SensorValue::TimeWithoutMotion(_) => HassSensorInfo::Sensor(
                    HassSensor::new(sensor_state_topic)
                        .name("Time without Motion")
                        .icon("motion-sensor-off")
                        .device_class(HassDeviceClass::None),
                ),
            };

            let (hass_discovery_topic, hass_sensor) = match hass_sensor {
                HassSensorInfo::BinarySensor(sensor) => (
                    format!("homeassistant/binary_sensor/{sensor_unique_id}/config"),
                    HassSensorInfo::BinarySensor(
                        sensor
                            .unique_id(sensor_unique_id.clone())
                            .expire_after(nonzero_lit::u32!(60))
                            .device(
                                known_devices
                                    .get(&device_unique_id)
                                    .map(HassDevice::clone)
                                    .unwrap(),
                            ),
                    ),
                ),
                HassSensorInfo::Sensor(sensor) => (
                    format!("homeassistant/sensor/{sensor_unique_id}/config"),
                    HassSensorInfo::Sensor(
                        sensor
                            .unique_id(sensor_unique_id.clone())
                            .expire_after(nonzero_lit::u32!(60))
                            .device(
                                known_devices
                                    .get(&device_unique_id)
                                    .map(HassDevice::clone)
                                    .unwrap(),
                            ),
                    ),
                ),
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

        debug!("Publishing sensor value {sensor_state_topic}: {sensor_state}");
        mqtt_client
            .publish(sensor_state_topic, QoS::AtLeastOnce, false, sensor_state)
            .await?;
    }

    Ok(())
}
