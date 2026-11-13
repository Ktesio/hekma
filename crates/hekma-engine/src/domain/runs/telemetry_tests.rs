use super::*;

#[test]
fn a_cumulative_counter_diffs_forward_and_rebases_on_a_reset() {
    assert_eq!(
        delta(None, 1200),
        (1200, false),
        "the first post is its value"
    );
    assert_eq!(delta(Some(1200), 1500), (300, false));
    assert_eq!(delta(Some(1500), 1500), (0, false), "a repeat is zero");
    assert_eq!(delta(Some(1500), 200), (200, true), "a restart re-bases");
    assert_eq!(delta(Some(0), 0), (0, false));
}

#[test]
fn token_types_round_trip_their_wire_form() {
    for token_type in TelemetryTokenType::ALL {
        assert_eq!(
            TelemetryTokenType::from_wire(token_type.as_str()),
            Some(token_type)
        );
    }
    assert_eq!(TelemetryTokenType::from_wire("reasoning"), None);
    assert_eq!(TelemetryTokenType::Input as usize, 0);
    assert_eq!(TelemetryTokenType::Output as usize, 1);
    assert_eq!(TelemetryTokenType::Cached as usize, 2);
}
