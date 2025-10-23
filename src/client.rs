use std::env;

use url;
use ws::util::Token;
use ws::{Handler, Sender, Handshake, Result, Message, Request, Error, ErrorKind, CloseCode};
use uuid::Uuid;
use chrono::prelude::*;
use json::JsonValue;

use crate::requests;
use crate::responses;
use crate::components;
use crate::storage;

/// This macro allows to break from a code block outside of a loop.
macro_rules! block {
    ($xs:block) => {
        loop { let _ = $xs; break; }
    };
}

static mut HEARTBEAT_INTERVAL: u64 = 0;

// Timeout events.
const HEARTBEAT: Token = Token(1);
const QUEUE_FETCH: Token = Token(2);
// OCPP constants.
const CALL: u8 = 2;
const CALLRESULT: u8 = 3;
const CALLERROR: u8 = 4;
// Message queue constants.
const QUEUE_FETCH_INTERVAL: u64 = 50;
const QUEUE_MESSAGE_EXPIRATION: u64 = 10;

// Websocket Handler struct.
pub struct Client {
    pub out: Sender,
}

/// We implement the Handler trait for Client so that we can get more
/// fine-grained control of the connection.
impl Handler for Client {

    /// Add protocol to initial handshake request.
    fn build_request(&mut self, url: &url::Url) -> Result<Request> {
        let mut req = Request::from_url(url).unwrap();
        req.add_protocol("ocpp2.0");
        Ok(req)
    }

    /// Called when the WebSocket handshake is successful and the connection is open for sending
    /// and receiving messages.
    ///
    /// Configures interval between fetches in the message queue.
    /// Sends BootNotification message to the message queue.
    fn on_open(&mut self, _: Handshake) -> Result<()> {
        // Start queue worker.
        self.out.timeout(QUEUE_FETCH_INTERVAL, QUEUE_FETCH)?;

        // Get model from environment.
        let model: String = match env::var("MODEL") {
            Ok(var) => if var == "" { "Model".to_string() } else { var },
            _ => "Model".to_string(),
        };

        // Get vendor name from environment.
        let vendor_name: String = match env::var("VENDOR_NAME") {
            Ok(var) => if var == "" { "Vendor name".to_string() } else { var },
            _ => "Vendor name".to_string(),
        };

        // Get serial number from environment.
        let serial_number: Option<String> = match env::var("SERIAL_NUMBER") {
            Ok(data) => Some(data),
            _ => None,
        };

        // Send BootNotification request.

        let msg_id: &str = &Uuid::new_v4().to_string();
        let msg = requests::boot_notification(msg_id, "PowerUp", &model, &vendor_name, serial_number);

        storage::set_message(msg_id.to_string(), msg.to_owned());

        storage::queue_add(msg);

        Ok(())
    }

    /// Called on incoming messages.
    ///
    /// Handles requests and responses from the Charging Station Management System.
    fn on_message(&mut self, msg: Message) -> Result<()> {
        let parsed_msg = match json::parse(msg.as_text()?) {
            Ok(result) => result,
            Err(e) => panic!("Error during parsing: {:?}", e),
        };

        let msg_type_id = match parsed_msg[0].as_u8() {
            Some(res) => res,
            None => panic!("Parsed message has no value."),
        };

        let msg_id: &str = &parsed_msg[1].to_string();

        println!("Message ID: {}", msg_id);

        match msg_type_id {
            CALL => block!({
                let action: &str = &parsed_msg[2].to_string();
                let payload: &JsonValue = &parsed_msg[3];

                println!("CALL Action: {}", action);
                println!("CALL Payload: {}", payload);

                match action {
                    "SetVariables" => {
                        // Send SetVariables response.

                        let set_variable_data_array = &payload["setVariableData"];

                        let mut variables: JsonValue = JsonValue::new_array();

                        for i in 0..set_variable_data_array.len() {
                            let set_variable_data = &set_variable_data_array[i];
                            let component_name: &str = &set_variable_data["component"].to_string();
                            let variable_name: &str = &set_variable_data["variable"]["name"].to_string();

                            let mut variable = object!{
                                "component" => component_name,
                                "variable" => object!{
                                    "name" => variable_name,
                                },
                            };

                            match component_name {
                                "AuthCtrlr" => {
                                    match variable_name {
                                        "AuthorizeRemoteStart" => variable["attributeStatus"] = "Rejected".into(),
                                        _ => variable["attributeStatus"] = "UnknownVariable".into(),
                                    }
                                },
                                _ => variable["attributeStatus"] = "UnknownComponent".into(),
                            };

                            variables.push(variable).unwrap();
                        }

                        let response_msg: String = responses::set_variables(msg_id, variables);

                        self.out.send(response_msg)?;
                    },
                    "GetVariables" => {
                        // Send GetVariables response.

                        let get_variable_data_array = &payload["getVariableData"];

                        let mut variables: JsonValue = JsonValue::new_array();

                        for i in 0..get_variable_data_array.len() {
                            let get_variable_data = &get_variable_data_array[i];
                            let component_name: &str = &get_variable_data["component"].to_string();
                            let variable_name: &str = &get_variable_data["variable"]["name"].to_string();

                            let (attribute_status, attribute_value): (&str, Option<&str>) = components::get_variable(component_name, variable_name);

                            let mut variable = object!{
                                "attributeStatus" => attribute_status,
                                "component" => component_name,
                                "variable" => object!{
                                    "name" => variable_name,
                                },
                            };

                            match attribute_value {
                                Some(data) => variable["attributeValue"] = data.into(),
                                _ => (),
                            };

                            variables.push(variable).unwrap();
                        }

                        let response_msg: String = responses::get_variables(msg_id, variables);

                        self.out.send(response_msg)?;
                    }
                    "RequestStartTransaction" => {
                        let remote_start_id: u64 = match payload["remoteStartId"].as_number() {
                            Some(res) => (res.as_fixed_point_i64(0).unwrap_or(0) as u64),
                            None => panic!("Parsed message has no value."),
                        };

                        // Generate transaction id.
                        let transaction_id: &str = &Uuid::new_v4().to_string();

                        // Check connector status.
                        let evse_id: usize = match payload["evseId"].as_number() {
                            Some(res) => res.as_fixed_point_i64(0).unwrap_or(0) as usize,
                            _ => panic!("Parsed EVSE ID has no value."),
                        };

                        // FIXME Magic number (connector index).
                        let connector = storage::get_connector(evse_id - 1, 0);

                        let mut response_status = "Accepted";

                        if connector.status != "Available" {
                            response_status = "Rejected";
                        }

                        // Send RequestStartTransaction response.

                        let request_start_transaction_msg = responses::request_start_transaction(msg_id, remote_start_id, response_status);

                        self.out.send(request_start_transaction_msg)?;

                        if response_status == "Rejected" {
                            break;
                        }

                        // Set EVSE status to "Occupied" and send StatusNotification with updated status.

                        let connector_status = "Occupied";
                        let status_notification_msg_id: &str = &Uuid::new_v4().to_string();
                        let status_notification_msg = requests::status_notification(status_notification_msg_id, 1, 1, connector_status);

                        storage::set_message(status_notification_msg_id.to_string(), status_notification_msg.to_owned());

                        storage::queue_add(status_notification_msg);

                        storage::set_connector_status(0, 0, connector_status);

                        // Send "Started" TransactionEvent request to notify CSMS about the started transaction.

                        let transaction_event_started_msg_id: &str = &Uuid::new_v4().to_string();
                        let transaction_event_started_msg = requests::transaction_event(transaction_event_started_msg_id, transaction_id, "Started", "RemoteStart", None, Some(remote_start_id), None);

                        storage::set_message(transaction_event_started_msg_id.to_string(), transaction_event_started_msg.to_owned());

                        storage::queue_add(transaction_event_started_msg);

                        // Save transaction.
                        storage::set_transaction(transaction_id.to_string(), payload.dump());

                        // Send "Updated" TransactionEvent request to notify CSMS about the plugged in cable.

                        let transaction_event_updated_msg_id: &str = &Uuid::new_v4().to_string();
                        let transaction_event_updated_msg = requests::transaction_event(transaction_event_updated_msg_id, transaction_id, "Updated", "CablePluggedIn", Some("Charging"), None, None);

                        storage::set_message(transaction_event_updated_msg_id.to_string(), transaction_event_updated_msg.to_owned());

                        storage::queue_add(transaction_event_updated_msg);
                    },
                    "RequestStopTransaction" => {
                        let transaction_id: &str = &payload["transactionId"].to_string();
                        // Get transaction from hash map.
                        let transaction = storage::get_transaction(transaction_id);

                        let response_status = match transaction.as_str() {
                            "" => "Rejected",
                            _ => "Accepted",
                        };

                        // Send RequestStopTransaction response.

                        let request_stop_transaction_msg = responses::request_stop_transaction(msg_id, response_status);

                        self.out.send(request_stop_transaction_msg)?;

                        if response_status == "Rejected" {
                            break;
                        }

                        // Send "Updated" TransactionEvent request to notify CSMS about remote stop command.

                        let transaction_event_updated_msg_id: &str = &Uuid::new_v4().to_string();
                        let transaction_event_updated_msg = requests::transaction_event(transaction_event_updated_msg_id, transaction_id, "Updated", "RemoteStop", None, None, None);

                        storage::set_message(transaction_event_updated_msg_id.to_string(), transaction_event_updated_msg.to_owned());

                        storage::queue_add(transaction_event_updated_msg);

                        // Send "Ended" TransactionEvent request.

                        let transaction_event_ended_msg_id: &str = &Uuid::new_v4().to_string();
                        let transaction_event_ended_msg = requests::transaction_event(transaction_event_ended_msg_id, transaction_id, "Ended", "RemoteStop", None, None, Some("Remote"));

                        storage::set_message(transaction_event_ended_msg_id.to_string(), transaction_event_ended_msg.to_owned());

                        storage::queue_add(transaction_event_ended_msg);

                        // Delete transaction.
                        storage::delete_transaction(transaction_id);

                        // Set EVSE status to "Available" and send StatusNotification with updated status.

                        let connector_status = "Available";
                        let status_notification_msg_id: &str = &Uuid::new_v4().to_string();
                        let status_notification_msg = requests::status_notification(status_notification_msg_id, 1, 1, connector_status);

                        storage::set_message(status_notification_msg_id.to_string(), status_notification_msg.to_owned());

                        storage::queue_add(status_notification_msg);

                        storage::set_connector_status(0, 0, connector_status);
                    },
                    _ => println!("No request handler for action: {}", action),
                }
            }),
            CALLRESULT => block!({
                let payload: &JsonValue = &parsed_msg[2];

                let msg_from_map = storage::get_message(msg_id);

                if msg_from_map == "" {
                    break;
                }

                let parsed_msg_from_map = match json::parse(&msg_from_map.to_owned()) {
                    Ok(result) => result,
                    Err(e) => panic!("Error during parsing: {:?}", e),
                };

                let msg_from_map_action: &str = &parsed_msg_from_map[2].to_string();
                // NOTE Unused.
                // let msg_from_map_payload: &JsonValue = &parsed_msg_from_map[3];

                match msg_from_map_action {
                    "BootNotification" => {
                        // Check status of the response.
                        if payload["status"].to_string() == "Accepted" {
                            println!("BootNotification was accepted.");

                            // Set EVSE status to "Available" and send StatusNotification with updated status.

                            let connector_status = "Available";
                            let status_notification_msg_id: &str = &Uuid::new_v4().to_string();
                            let status_notification_msg = requests::status_notification(status_notification_msg_id, 1, 1, connector_status);

                            storage::set_message(status_notification_msg_id.to_string(), status_notification_msg.to_owned());

                            storage::queue_add(status_notification_msg);

                            storage::set_connector_status(0, 0, connector_status);

                            // Schedule a Heartbeat using the interval from BootNotification.

                            unsafe {
                                match payload["interval"].as_number() {
                                    Some(res) => HEARTBEAT_INTERVAL = (res.as_fixed_point_i64(0).unwrap_or(0) as u64) * 1000,
                                    None => panic!("Parsed message has no value."),
                                };

                                self.out.timeout(HEARTBEAT_INTERVAL, HEARTBEAT)?;
                            }
                        }
                    },
                    _=> println!("No response handler for action: {}", msg_from_map_action),
                }
            }),
            CALLERROR => {
                let error_code: &str = &parsed_msg[2].to_string();
                let error_description: &str = &parsed_msg[3].to_string();
                let error_details: &str = &parsed_msg[4].to_string();

                println!("CALLERROR Error code: {}", error_code);
                println!("CALLERROR Error Description: {}", error_description);
                println!("CALLERROR Error details: {}", error_details);
            },
            _ => println!("Unknown message type ID"),
        }

        Ok(())
    }

    /// Called any time this endpoint receives a close control frame.
    fn on_close(&mut self, code: CloseCode, reason: &str) {
       println!("WebSocket closing for ({:?}) {}", code, reason);
       println!("Shutting down server after first connection closes.");
       self.out.shutdown().unwrap();
   }

   /// Shutdown on any error.
   fn on_error(&mut self, err: Error) {
        println!("Shutting down server for error: {}", err);
        self.out.shutdown().unwrap();
    }

    /// Called when a timeout has been scheduled on the eventloop.
    ///
    /// Sends Heartbeat message.
    /// Fetches and sends messages from the message queue.
    fn on_timeout(&mut self, event: Token) -> Result<()> {
        match event {
            HEARTBEAT => {
                // Send Heartbeat message.

                let msg_id: &str = &Uuid::new_v4().to_string();
                let msg = requests::heartbeat(msg_id);

                storage::set_message(msg_id.to_string(), msg.to_owned());

                storage::queue_add(msg);

                // Schedule next message.
                unsafe {
                    self.out.timeout(HEARTBEAT_INTERVAL, HEARTBEAT)?;
                }

                Ok(())
            },
            QUEUE_FETCH => {
                let current_timestamp: u64 = Utc::now().timestamp() as u64;

                let last_sent_msg = storage::get_last_sent_message();
                // Check whether last sent message exists or not.
                let last_sent_msg_exist: bool = last_sent_msg.id != None;
                // Check whether last sent message has expired or not.
                let last_sent_msg_expired: bool = match last_sent_msg.timestamp {
                    Some(timestamp) => timestamp + QUEUE_MESSAGE_EXPIRATION < current_timestamp,
                    None => true,
                };

                if storage::queue_size() > 0 && (!last_sent_msg_exist || last_sent_msg_expired) {
                    let msg = storage::queue_pop();

                    if msg != "" {
                        let parsed_msg = match json::parse(&msg.to_owned()) {
                            Ok(result) => result,
                            Err(e) => panic!("Error during parsing: {:?}", e),
                        };

                        let msg_id: &str = &parsed_msg[1].to_string();
                        let msg_action: &str = &parsed_msg[2].to_string();

                        self.out.send(msg)?;

                        println!("{} ({}) was sent.", msg_action, msg_id);

                        storage::set_last_sent_message(msg_id.to_string(), current_timestamp);
                    }
                }

                self.out.timeout(QUEUE_FETCH_INTERVAL, QUEUE_FETCH)?;

                Ok(())
            },
            // No other events are possible.
            _ => Err(Error::new(
                ErrorKind::Internal,
                "Invalid timeout token encountered!",
            )),
        }
    }
}
