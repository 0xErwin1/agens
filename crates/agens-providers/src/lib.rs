use std::collections::BTreeMap;

use agens_core::{Error, Message, MessagePart, Role};
use serde_json::Value;

pub fn encode_openai_response_request(model: &str, message: &Message) -> Result<String, Error> {
    let content = match message.parts.as_slice() {
        [MessagePart::Text(content)] => content,
        _ => {
            return Err(Error::Provider(
                "OpenAI request error: only a single text part is supported".to_owned(),
            ));
        }
    };
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        _ => {
            return Err(Error::Provider(
                "OpenAI request error: only system and user messages are supported".to_owned(),
            ));
        }
    };

    Ok(serde_json::json!({
        "model": model,
        "input": [{ "role": role, "content": content }],
        "stream": true,
    })
    .to_string())
}

pub fn decode_openai_response_events<I, S>(events: I) -> Result<Vec<MessagePart>, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut decoder = OpenAiResponseDecoder::default();

    for event in events {
        decoder.process(event.as_ref())?;
    }

    Ok(decoder.parts)
}

#[derive(Default)]
struct OpenAiResponseDecoder {
    parts: Vec<MessagePart>,
    function_calls: BTreeMap<String, FunctionCall>,
}

struct FunctionCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl OpenAiResponseDecoder {
    fn process(&mut self, event_json: &str) -> Result<(), Error> {
        let event: Value =
            serde_json::from_str(event_json).map_err(|_| protocol_error("invalid event JSON"))?;
        let event_type = required_string(&event, "type")?;

        match event_type {
            "response.output_text.delta" => {
                self.parts.push(MessagePart::Text(
                    required_string(&event, "delta")?.to_owned(),
                ));
            }
            "response.output_item.done" => self.process_output_item(&event)?,
            "response.output_item.added" => self.add_function_call(&event)?,
            "response.function_call_arguments.delta" => self.append_function_arguments(&event)?,
            "response.function_call_arguments.done" => self.finish_function_call(&event)?,
            "error" => return Err(upstream_error(&event)),
            _ => {}
        }

        Ok(())
    }

    fn process_output_item(&mut self, event: &Value) -> Result<(), Error> {
        let item = required_object(event, "item")?;

        if required_string(item, "type")? != "reasoning" {
            return Ok(());
        }

        let summaries = required_array(item, "summary")?;

        for summary in summaries {
            if required_string(summary, "type")? == "summary_text" {
                self.parts.push(MessagePart::Reasoning(
                    required_string(summary, "text")?.to_owned(),
                ));
            }
        }

        Ok(())
    }

    fn add_function_call(&mut self, event: &Value) -> Result<(), Error> {
        let item = required_object(event, "item")?;

        if required_string(item, "type")? != "function_call" {
            return Ok(());
        }

        let id = required_string(item, "id")?.to_owned();
        let call = FunctionCall {
            call_id: required_string(item, "call_id")?.to_owned(),
            name: required_string(item, "name")?.to_owned(),
            arguments: required_string(item, "arguments")?.to_owned(),
        };

        if self.function_calls.insert(id, call).is_some() {
            return Err(protocol_error("duplicate function call item"));
        }

        Ok(())
    }

    fn append_function_arguments(&mut self, event: &Value) -> Result<(), Error> {
        let id = required_string(event, "item_id")?;
        let call = self.function_calls.get_mut(id).ok_or_else(|| {
            protocol_error("function arguments arrived before the function call item")
        })?;

        call.arguments.push_str(required_string(event, "delta")?);

        Ok(())
    }

    fn finish_function_call(&mut self, event: &Value) -> Result<(), Error> {
        let id = required_string(event, "item_id")?;
        let mut call = self.function_calls.remove(id).ok_or_else(|| {
            protocol_error("function arguments completed before the function call item")
        })?;

        call.arguments = required_string(event, "arguments")?.to_owned();
        self.parts.push(MessagePart::ToolCall {
            id: call.call_id,
            name: call.name,
            input: call.arguments,
        });

        Ok(())
    }
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_error("event is missing a required string field"))
}

fn required_object<'a>(value: &'a Value, field: &str) -> Result<&'a Value, Error> {
    value
        .get(field)
        .filter(|candidate| candidate.is_object())
        .ok_or_else(|| protocol_error("event is missing a required object field"))
}

fn required_array<'a>(value: &'a Value, field: &str) -> Result<&'a Vec<Value>, Error> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_error("event is missing a required array field"))
}

fn upstream_error(event: &Value) -> Error {
    let message = event
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the upstream provider did not provide an error message");

    Error::Provider(format!("OpenAI stream failed: {message}"))
}

fn protocol_error(detail: &str) -> Error {
    Error::Provider(format!("OpenAI stream protocol error: {detail}"))
}
