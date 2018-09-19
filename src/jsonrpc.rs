use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::option::Option;
use std::result::Result::{Err, Ok};
use std::sync::mpsc::{channel, RecvError, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use jsonrpc_core::types::{
    Call, Error as JSONRPCError, ErrorCode, Failure, Id, MethodCall, Output, Params, Response, Success,
};
use rand;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json;
use serde_json::{Error as SerdeError, Value};

use super::router::Error as RouterError;
use super::ws::{Error as WSError, Message, Sender as WSSender};

pub fn handle<F>(router: F, text: String) -> Option<String>
where
    F: FnOnce(String, Value) -> Result<Option<Value>, RouterError>, {
    let deserialized = serde_json::from_str(&text);
    let response: Option<Response> = match deserialized {
        Err(_) => Some(
            Failure {
                jsonrpc: None,
                id: Id::Null,
                error: JSONRPCError::new(ErrorCode::ParseError),
            }.into(),
        ),
        Ok(Call::Invalid(id)) => Some(
            Failure {
                jsonrpc: None,
                id,
                error: JSONRPCError::new(ErrorCode::ParseError),
            }.into(),
        ),
        Ok(Call::MethodCall(MethodCall {
            id,
            method,
            params,
            ..
        })) => {
            let value_params = serde_json::to_value(params).expect("Change to value always success");
            match router(method, value_params) {
                Ok(Some(value)) => Some(
                    Success {
                        jsonrpc: None,
                        result: value,
                        id,
                    }.into(),
                ),
                Ok(None) => {
                    let mut error = JSONRPCError::new(ErrorCode::InternalError);
                    error.data = Some(serde_json::Value::String("API returns no value".to_string()));
                    Some(
                        Failure {
                            jsonrpc: None,
                            id,
                            error,
                        }.into(),
                    )
                }
                Err(RouterError::MethodNotFound) => Some(
                    Failure {
                        jsonrpc: None,
                        id,
                        error: JSONRPCError::new(ErrorCode::MethodNotFound),
                    }.into(),
                ),
                Err(RouterError::RPC(err)) => Some(
                    Failure {
                        jsonrpc: None,
                        id,
                        error: err.to_jsonrpc_error(),
                    }.into(),
                ),
            }
        }
        Ok(Call::Notification(_)) => None,
    };
    response.map(|response| serde_json::to_string(&response).expect("Should success serialize"))
}

pub fn invalid_format() -> String {
    serde_json::to_string(&Failure {
        jsonrpc: None,
        id: Id::Null,
        error: JSONRPCError::new(ErrorCode::ParseError),
    }).expect("Should success serialize")
}

#[derive(Clone)]
pub struct Context {
    pub ws_sender: WSSender,
    pub ws_callback: Arc<Mutex<HashMap<u64, Sender<String>>>>,
}

impl Context {
    pub fn new(sender: WSSender) -> Self {
        Self {
            ws_sender: sender,
            ws_callback: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn add_callback(&self, id: u64, callback: Sender<String>) {
        let mut ws_callback = self.ws_callback.lock().expect("Should success get ws_callback");
        ws_callback.insert(id, callback);
    }
}

pub enum CallError {
    InternalWS(WSError),
    InternalRecv(RecvError),
    InternalSerde(SerdeError),
    InternalSync(String),
    Response(JSONRPCError),
    Timeout(RecvTimeoutError),
}

impl From<WSError> for CallError {
    fn from(error: WSError) -> Self {
        CallError::InternalWS(error)
    }
}

impl From<RecvError> for CallError {
    fn from(error: RecvError) -> Self {
        CallError::InternalRecv(error)
    }
}

impl From<SerdeError> for CallError {
    fn from(error: SerdeError) -> Self {
        CallError::InternalSerde(error)
    }
}

impl<T> From<PoisonError<T>> for CallError {
    fn from(error: PoisonError<T>) -> Self {
        CallError::InternalSync(format!("{:?}", error))
    }
}

impl From<JSONRPCError> for CallError {
    fn from(error: JSONRPCError) -> Self {
        CallError::Response(error)
    }
}

impl From<RecvTimeoutError> for CallError {
    fn from(error: RecvTimeoutError) -> Self {
        CallError::Timeout(error)
    }
}

pub fn call_no_arg<Res>(context: Context, method: &str) -> Result<Res, CallError>
where
    Res: DeserializeOwned, {
    call(context, method, Value::Null)
}

pub fn call<Arg, Res>(context: Context, method: &str, arg: Arg) -> Result<Res, CallError>
where
    Arg: Serialize,
    Res: DeserializeOwned, {
    let (tx, rx) = channel();
    let arg_value = serde_json::to_value(arg)?;
    let id = rand::random();
    let request = MethodCall {
        jsonrpc: None,
        method: method.to_string(),
        params: Some(Params::Array(vec![arg_value])),
        id: Id::Num(id),
    };
    let serialized_request = serde_json::to_string(&request)?;
    context.add_callback(id, tx);
    ctrace!("jend JSONRPC {}", serialized_request);
    context.ws_sender.send(Message::Text(serialized_request))?;
    let received_string = rx.recv_timeout(Duration::new(10, 0))?;
    ctrace!("Receive JSONRPC {}", received_string);

    let res = serde_json::from_str(&received_string)?;

    match res {
        Output::Success(success) => {
            let result = serde_json::from_value(success.result)?;
            Ok(result)
        }
        Output::Failure(failure) => Err(failure.error.into()),
    }
}

// Called on websocket thread
pub fn on_receive(context: Context, text: String) {
    match on_receive_internal(context, text) {
        Ok(_) => {}
        Err(err) => cerror!("{}", err),
    }
}

fn on_receive_internal(context: Context, text: String) -> Result<(), String> {
    let json_parsed_result: Output = serde_json::from_str(&text)
        .map_err(|err| format!("Cannot parse response from agent, data is {}\n{}", text.clone(), err))?;

    let id = json_parsed_result.id();
    let id = match id {
        Id::Null => Err(id),
        Id::Str(_) => Err(id),
        Id::Num(id) => Ok(id),
    }.map_err(|id| format!("Invalid id {:#?}", id))?;

    let ws_callback = context
        .ws_callback
        .lock()
        .map_err(|err| format!("Cannot acquire ws_callback lock on handling {}\n{}", text.clone(), err))?;
    let callback = ws_callback.get(&id).ok_or(format!("Invalid id {}", id))?;
    callback.send(text.clone()).map_err(|err| format!("Callback call failed, response was {}\n{}", text.clone(), err))
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CallError::InternalWS(err) => write!(f, "Call Internal Error {}", err),
            CallError::InternalRecv(err) => write!(f, "Call Internal Error {}", err),
            CallError::InternalSerde(err) => write!(f, "Call Internal Error {}", err),
            CallError::InternalSync(err) => write!(f, "Call Internal Error {}", err),
            CallError::Response(err) => write!(f, "JSONRPC error {:?}", err),
            CallError::Timeout(err) => write!(f, "Timeout {}", err),
        }
    }
}

impl fmt::Debug for CallError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Error for CallError {}