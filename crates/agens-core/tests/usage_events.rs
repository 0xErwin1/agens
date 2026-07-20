use agens_core::{TurnEvent, Usage};

#[test]
fn usage_event_retains_reported_and_unavailable_values() {
    let event = TurnEvent::Usage(Usage {
        input_tokens: Some(120),
        output_tokens: None,
        total_tokens: Some(145),
        context_window: None,
    });

    assert_eq!(
        event,
        TurnEvent::Usage(Usage {
            input_tokens: Some(120),
            output_tokens: None,
            total_tokens: Some(145),
            context_window: None,
        })
    );
}

#[test]
fn usage_event_does_not_replace_reported_zero_with_a_counter() {
    let event = TurnEvent::Usage(Usage {
        input_tokens: Some(0),
        output_tokens: Some(0),
        total_tokens: Some(0),
        context_window: Some(128_000),
    });

    assert_eq!(
        event,
        TurnEvent::Usage(Usage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            total_tokens: Some(0),
            context_window: Some(128_000),
        })
    );
}
